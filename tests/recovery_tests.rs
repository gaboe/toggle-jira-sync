use httpmock::{Method, MockServer};
use serde_json::json;
use tempfile::NamedTempFile;
use toggl_jira_sync::{
    db::Database,
    jira::{JiraClient, TogglSyncMarker, MARKER_PROPERTY_KEY},
    sync::{
        executor::{execute_plan, ExecutorOptions},
        planner::{
            compute_source_hash, plan_sync, ExistingWorklogLink, IssueSiteMapping, PlannerInput,
        },
        recovery::{recover, RecoveryConflict, RecoveryInput, RecoverySite, RecoveryWarning},
    },
    toggl::TogglTimeEntry,
};

const EMAIL: &str = "sync@example.test";
const TOKEN: &str = "fake-jira-token-do-not-log";
const SITE_KEY: &str = "sabservis";
const ISSUE_KEY: &str = "SAB-123";
const WORKLOG_ID: &str = "10001";
const WORKLOG_ID_DUPLICATE: &str = "10002";

fn temp_database() -> (NamedTempFile, Database) {
    let file = NamedTempFile::new().expect("temp sqlite file should be created");
    let database = Database::open(file.path()).expect("temp sqlite database should open");
    database.run_migrations().expect("migrations should run");
    (file, database)
}

fn client(server: &MockServer) -> JiraClient {
    JiraClient::from_credentials(server.base_url(), EMAIL.to_owned(), TOKEN.to_owned())
}

fn entry(description: &str) -> TogglTimeEntry {
    TogglTimeEntry {
        workspace_id: "700001".to_owned(),
        entry_id: "900001".to_owned(),
        start: "2024-05-02T01:00:00.000+0000".to_owned(),
        duration_seconds: 1_800,
        description: Some(description.to_owned()),
        updated_at: "2024-05-02T03:06:40Z".to_owned(),
        deleted_at: None,
        skip_reason: None,
    }
}

fn source_hash(description: &str) -> String {
    compute_source_hash(
        ISSUE_KEY,
        SITE_KEY,
        1_800,
        "2024-05-02T01:00:00.000+0000",
        description,
    )
}

fn marker(source_hash: &str) -> TogglSyncMarker {
    TogglSyncMarker {
        toggl_workspace_id: 700001,
        toggl_entry_id: 900001,
        source_hash: source_hash.to_owned(),
        synced_at: "2024-05-02T03:06:40Z".to_owned(),
        tool_version: "0.1.0".to_owned(),
    }
}

fn recovery_site(server: &MockServer) -> RecoverySite {
    RecoverySite {
        key: SITE_KEY.to_owned(),
        client: client(server),
    }
}

fn issue_site_mapping() -> IssueSiteMapping {
    IssueSiteMapping {
        issue_key: ISSUE_KEY.to_owned(),
        jira_site_key: SITE_KEY.to_owned(),
    }
}

#[tokio::test]
async fn recovery_after_db_delete_prevents_duplicate_create() {
    let (_file, db) = temp_database();
    let server = MockServer::start();
    let description = "SAB-123 created entry";
    let expected_hash = source_hash(description);

    let list = server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({
            "worklogs": [{ "id": WORKLOG_ID, "comment": {"type":"doc","version":1,"content":[]} }]
        }));
    });
    let property = server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200)
            .json_body(json!({ "key": MARKER_PROPERTY_KEY, "value": marker(&expected_hash) }));
    });
    let create = server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(201).json_body(json!({ "id": "duplicate" }));
    });

    let report = recover(RecoveryInput {
        database: &db,
        entries: vec![entry(description)],
        issue_site_mappings: vec![issue_site_mapping()],
        recovery_sites: vec![recovery_site(&server)],
        recovery_scan_days: 180,
        requested_scan_days: None,
    })
    .await
    .expect("recovery should reconstruct DB link");

    assert_eq!(report.recovered_links, 1);
    assert_eq!(db.count_rows("jira_worklog_links").unwrap(), 1);
    assert_eq!(create.hits(), 0, "recovery must not create Jira worklogs");
    list.assert();
    property.assert();

    let stored = db
        .get_jira_worklog_link("700001", "900001", SITE_KEY)
        .expect("link lookup should succeed")
        .expect("recovered link should exist");
    let plan = plan_sync(PlannerInput {
        entries: vec![entry(description)],
        issue_site_mappings: vec![issue_site_mapping()],
        existing_links: vec![ExistingWorklogLink {
            toggl_workspace_id: stored.toggl_workspace_id,
            toggl_entry_id: stored.toggl_entry_id,
            jira_site_key: stored.jira_site_key,
            jira_issue_key: stored.jira_issue_key,
            jira_worklog_id: stored.jira_worklog_id.expect("worklog id recovered"),
            source_hash: stored.source_hash,
            marked_toggl_managed: true,
        }],
    })
    .expect("planning should not fail");

    let sync_report = execute_plan(&plan, &client(&server), &db, ExecutorOptions::default())
        .await
        .expect("subsequent sync should be a no-op");
    assert_eq!(sync_report.succeeded, 0);
    assert_eq!(
        create.hits(),
        0,
        "sync after recovery must not duplicate POST"
    );
}

#[tokio::test]
async fn recovery_never_mutates_jira() {
    let (_file, db) = temp_database();
    let server = MockServer::start();
    let description = "SAB-123 created entry";
    let expected_hash = source_hash(description);
    server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200)
            .json_body(json!({ "worklogs": [{ "id": WORKLOG_ID }] }));
    });
    server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200)
            .json_body(json!({ "key": MARKER_PROPERTY_KEY, "value": marker(&expected_hash) }));
    });
    let mutations = server.mock(|when, then| {
        when.any_request()
            .matches(|request| matches!(request.method.as_str(), "POST" | "PUT" | "DELETE"));
        then.status(500).body("recovery must be read-only");
    });

    recover(RecoveryInput {
        database: &db,
        entries: vec![entry(description)],
        issue_site_mappings: vec![issue_site_mapping()],
        recovery_sites: vec![recovery_site(&server)],
        recovery_scan_days: 180,
        requested_scan_days: None,
    })
    .await
    .expect("read-only recovery should pass");

    assert_eq!(mutations.hits(), 0);
}

#[tokio::test]
async fn recovery_duplicate_marked_worklogs_report_conflict() {
    let (_file, db) = temp_database();
    let server = MockServer::start();
    let description = "SAB-123 created entry";
    let expected_hash = source_hash(description);
    server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({
            "worklogs": [{ "id": WORKLOG_ID }, { "id": WORKLOG_ID_DUPLICATE }]
        }));
    });
    for worklog_id in [WORKLOG_ID, WORKLOG_ID_DUPLICATE] {
        server.mock(|when, then| {
            when.method(Method::GET).path(format!(
                "/rest/api/3/issue/{ISSUE_KEY}/worklog/{worklog_id}/properties/{MARKER_PROPERTY_KEY}"
            ));
            then.status(200)
                .json_body(json!({ "key": MARKER_PROPERTY_KEY, "value": marker(&expected_hash) }));
        });
    }
    let delete = server.mock(|when, then| {
        when.method(Method::DELETE);
        then.status(500)
            .body("recovery must not resolve conflicts by deleting");
    });

    let report = recover(RecoveryInput {
        database: &db,
        entries: vec![entry(description)],
        issue_site_mappings: vec![issue_site_mapping()],
        recovery_sites: vec![recovery_site(&server)],
        recovery_scan_days: 180,
        requested_scan_days: None,
    })
    .await
    .expect("duplicates should be reported, not fatal");

    assert_eq!(report.recovered_links, 0);
    assert!(report
        .conflicts
        .contains(&RecoveryConflict::DuplicateMarkedWorklogs {
            toggl_workspace_id: "700001".to_owned(),
            toggl_entry_id: "900001".to_owned(),
            jira_site_key: SITE_KEY.to_owned(),
            jira_issue_key: ISSUE_KEY.to_owned(),
            jira_worklog_ids: vec![WORKLOG_ID.to_owned(), WORKLOG_ID_DUPLICATE.to_owned()],
        }));
    assert_eq!(db.count_rows("jira_worklog_links").unwrap(), 0);
    assert_eq!(delete.hits(), 0);
}

#[tokio::test]
async fn recovery_warns_when_requested_range_exceeds_configured_window() {
    let (_file, db) = temp_database();
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({ "worklogs": [] }));
    });

    let report = recover(RecoveryInput {
        database: &db,
        entries: vec![entry("SAB-123 old entry")],
        issue_site_mappings: vec![issue_site_mapping()],
        recovery_sites: vec![recovery_site(&server)],
        recovery_scan_days: 180,
        requested_scan_days: Some(365),
    })
    .await
    .expect("bounded recovery should warn but continue");

    assert!(report
        .warnings
        .contains(&RecoveryWarning::RequestedWindowClamped {
            requested_days: 365,
            configured_days: 180,
        }));
}

#[tokio::test]
async fn recovery_uses_comment_fallback_marker_when_property_is_missing() {
    let (_file, db) = temp_database();
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({
            "worklogs": [{
                "id": WORKLOG_ID,
                "comment": {"type":"doc","version":1,"content":[{"type":"paragraph","content":[{"type":"text","text":"SAB-123 created entry [toggl-sync:workspace=700001;entry=900001]"}]}]}
            }]
        }));
    });
    server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(404)
            .json_body(json!({ "errorMessages": ["missing property"] }));
    });

    let report = recover(RecoveryInput {
        database: &db,
        entries: vec![entry("SAB-123 created entry")],
        issue_site_mappings: vec![issue_site_mapping()],
        recovery_sites: vec![recovery_site(&server)],
        recovery_scan_days: 180,
        requested_scan_days: None,
    })
    .await
    .expect("comment fallback recovery should pass");

    assert_eq!(report.recovered_links, 1);
    assert_eq!(db.count_rows("jira_worklog_links").unwrap(), 1);
}
