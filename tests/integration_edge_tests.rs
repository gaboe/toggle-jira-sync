use std::sync::{Arc, Mutex};
use std::time::Duration;

use httpmock::{Method, MockServer};
use serde_json::json;
use tempfile::NamedTempFile;
use toggl_jira_sync::{
    db::{Database, NewJiraWorklogLink},
    jira::{JiraClient, JiraError, Sleeper, TogglSyncMarker, MARKER_PROPERTY_KEY},
    sync::{
        executor::{execute_plan, ExecutorOptions, MutationStatus},
        planner::{
            compute_source_hash, plan_sync, ExistingWorklogLink, IssueSiteMapping, PlannedMutation,
            PlannerInput, PlannerIssue, PlannerOutcome, SkipCause,
        },
        recovery::{recover, RecoveryConflict, RecoveryInput, RecoverySite, RecoveryWarning},
    },
    toggl::{TogglClient, TogglClientConfig, TogglTimeEntry},
};

const EMAIL: &str = "sync@example.test";
const TOKEN: &str = "fake-jira-token-do-not-log";
const SITE_KEY: &str = "sabservis";
const ISSUE_KEY: &str = "SAB-123";
const WORKLOG_ID: &str = "10001";
const DUPLICATE_WORKLOG_ID: &str = "10002";
const WORKSPACE_ID: &str = "700001";
const ENTRY_ID: &str = "900001";

#[derive(Debug, Clone, Default)]
struct RecordingSleeper {
    sleeps: Arc<Mutex<Vec<Duration>>>,
}

impl RecordingSleeper {
    fn durations(&self) -> Vec<Duration> {
        self.sleeps
            .lock()
            .expect("sleep lock should not poison")
            .clone()
    }
}

impl Sleeper for RecordingSleeper {
    fn sleep<'a>(
        &'a self,
        duration: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.sleeps
                .lock()
                .expect("sleep lock should not poison")
                .push(duration);
        })
    }
}

fn temp_database() -> (NamedTempFile, Database) {
    let file = NamedTempFile::new().expect("temp sqlite file should be created");
    let database = Database::open(file.path()).expect("temp sqlite database should open");
    database.run_migrations().expect("migrations should run");
    (file, database)
}

fn jira_client(server: &MockServer) -> JiraClient {
    JiraClient::from_credentials(server.base_url(), EMAIL.to_owned(), TOKEN.to_owned())
}

fn recording_jira_client(
    server: &MockServer,
    sleeper: RecordingSleeper,
) -> JiraClient<RecordingSleeper> {
    JiraClient::new(
        server.base_url(),
        EMAIL.to_owned(),
        TOKEN.to_owned(),
        sleeper,
    )
}

fn entry(entry_id: &str, description: &str) -> TogglTimeEntry {
    TogglTimeEntry {
        workspace_id: WORKSPACE_ID.to_owned(),
        entry_id: entry_id.to_owned(),
        start: "2024-05-02T01:00:00.000+0000".to_owned(),
        duration_seconds: 1_800,
        description: Some(description.to_owned()),
        updated_at: "2024-05-02T03:06:40Z".to_owned(),
        deleted_at: None,
        skip_reason: None,
    }
}

fn deleted_entry(entry_id: &str, description: &str) -> TogglTimeEntry {
    TogglTimeEntry {
        deleted_at: Some("2024-05-02T03:06:40Z".to_owned()),
        ..entry(entry_id, description)
    }
}

fn default_input(entries: Vec<TogglTimeEntry>) -> PlannerInput {
    PlannerInput {
        entries,
        issue_site_mappings: vec![IssueSiteMapping {
            issue_key: ISSUE_KEY.to_owned(),
            jira_site_key: SITE_KEY.to_owned(),
        }],
        existing_links: Vec::new(),
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

fn marker(hash: &str) -> TogglSyncMarker {
    TogglSyncMarker {
        toggl_workspace_id: WORKSPACE_ID.parse().expect("numeric workspace id"),
        toggl_entry_id: ENTRY_ID.parse().expect("numeric entry id"),
        source_hash: hash.to_owned(),
        synced_at: "2024-05-02T03:06:40Z".to_owned(),
        tool_version: "0.1.0".to_owned(),
    }
}

fn marked_link(source_hash: &str) -> ExistingWorklogLink {
    ExistingWorklogLink {
        toggl_workspace_id: WORKSPACE_ID.to_owned(),
        toggl_entry_id: ENTRY_ID.to_owned(),
        jira_site_key: SITE_KEY.to_owned(),
        jira_issue_key: ISSUE_KEY.to_owned(),
        jira_worklog_id: WORKLOG_ID.to_owned(),
        source_hash: source_hash.to_owned(),
        marked_toggl_managed: true,
    }
}

fn insert_link(database: &Database, source_hash: &str) {
    database
        .insert_jira_worklog_link(&NewJiraWorklogLink {
            toggl_workspace_id: WORKSPACE_ID,
            toggl_entry_id: ENTRY_ID,
            jira_site_key: SITE_KEY,
            jira_issue_key: ISSUE_KEY,
            jira_worklog_id: Some(WORKLOG_ID),
            source_hash,
            rounded_duration_seconds: 1_800,
            status: "created",
        })
        .expect("link should insert");
}

fn empty_worklog_list(server: &MockServer) -> httpmock::Mock<'_> {
    server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({ "worklogs": [] }));
    })
}

#[test]
fn integration_edge_no_issue_key_skips_without_jira_mutation() {
    let plan = plan_sync(default_input(vec![entry(
        ENTRY_ID,
        "No ticket in this entry",
    )]))
    .expect("planner should not fail globally");

    assert!(plan.mutations.is_empty());
    assert!(matches!(
        plan.entries[0].outcome,
        PlannerOutcome::Skip(SkipCause::MissingIssueKey)
    ));
}

#[test]
fn integration_edge_multiple_issue_keys_errors_without_jira_mutation() {
    let plan = plan_sync(default_input(vec![entry(
        ENTRY_ID,
        "Touches SAB-123 and SAB-456",
    )]))
    .expect("planner should not fail globally");

    assert!(plan.mutations.is_empty());
    assert!(matches!(
        &plan.entries[0].outcome,
        PlannerOutcome::Error(PlannerIssue::MultipleIssueKeys { issue_keys })
            if issue_keys == &vec!["SAB-123".to_owned(), "SAB-456".to_owned()]
    ));
}

#[test]
fn integration_edge_unknown_jira_site_errors_without_jira_mutation() {
    let plan = plan_sync(default_input(vec![entry(ENTRY_ID, "Work on OPS-123")]))
        .expect("planner should not fail globally");

    assert!(plan.mutations.is_empty());
    assert!(matches!(
        &plan.entries[0].outcome,
        PlannerOutcome::Error(PlannerIssue::UnresolvedIssueSite { issue_key })
            if issue_key == "OPS-123"
    ));
}

#[tokio::test]
async fn integration_edge_invalid_issue_records_error_without_db_link() {
    let (_file, database) = temp_database();
    let server = MockServer::start();
    empty_worklog_list(&server);
    let create = server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(404)
            .json_body(json!({ "errorMessages": ["Issue does not exist"] }));
    });
    let plan = plan_sync(default_input(vec![entry(
        ENTRY_ID,
        "SAB-123 invalid issue",
    )]))
    .expect("planner should create a mutation");

    let error = execute_plan(
        &plan,
        &jira_client(&server),
        &database,
        ExecutorOptions::default(),
    )
    .await
    .expect_err("invalid issue should be surfaced");

    assert!(error.to_string().contains("not found"));
    assert_eq!(database.count_rows("jira_worklog_links").unwrap(), 0);
    create.assert_hits(1);
}

#[tokio::test]
async fn integration_edge_permission_denied_records_error_without_db_link() {
    let (_file, database) = temp_database();
    let server = MockServer::start();
    empty_worklog_list(&server);
    server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(403)
            .json_body(json!({ "errorMessages": ["Forbidden"] }));
    });
    let plan = plan_sync(default_input(vec![entry(ENTRY_ID, "SAB-123 forbidden")]))
        .expect("planner should create a mutation");

    let error = execute_plan(
        &plan,
        &jira_client(&server),
        &database,
        ExecutorOptions::default(),
    )
    .await
    .expect_err("permission denial should be surfaced");

    assert!(error.to_string().contains("permission denied"));
    assert_eq!(database.count_rows("jira_worklog_links").unwrap(), 0);
}

#[tokio::test]
async fn integration_edge_time_tracking_disabled_records_error_without_db_link() {
    let (_file, database) = temp_database();
    let server = MockServer::start();
    empty_worklog_list(&server);
    server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(400)
            .json_body(json!({ "errorMessages": ["Time tracking is disabled"] }));
    });
    let plan = plan_sync(default_input(vec![entry(ENTRY_ID, "SAB-123 no tracking")]))
        .expect("planner should create a mutation");

    let error = execute_plan(
        &plan,
        &jira_client(&server),
        &database,
        ExecutorOptions::default(),
    )
    .await
    .expect_err("disabled time tracking should be surfaced");

    assert!(error.to_string().contains("time tracking is disabled"));
    assert_eq!(database.count_rows("jira_worklog_links").unwrap(), 0);
}

#[test]
fn integration_edge_toggl_soft_delete_before_first_sync_skips_destructive_delete() {
    let plan = plan_sync(default_input(vec![deleted_entry(
        ENTRY_ID,
        "SAB-123 already deleted",
    )]))
    .expect("planner should not fail globally");

    assert!(plan.mutations.is_empty());
    assert!(matches!(
        plan.entries[0].outcome,
        PlannerOutcome::Skip(SkipCause::MissingManagedWorklog)
    ));
}

#[tokio::test]
async fn integration_edge_jira_worklog_manually_deleted_blocks_update_without_recreate() {
    let (_file, database) = temp_database();
    insert_link(&database, "sha256:old");
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(404)
            .json_body(json!({ "errorMessages": ["Worklog not found"] }));
    });
    let update = server.mock(|when, then| {
        when.method(Method::PUT).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}"
        ));
        then.status(200).json_body(json!({ "id": WORKLOG_ID }));
    });
    let mut input = default_input(vec![entry(ENTRY_ID, "SAB-123 changed after manual delete")]);
    input.existing_links = vec![marked_link("sha256:old")];
    let plan = plan_sync(input).expect("planner should schedule update from existing link");

    let report = execute_plan(
        &plan,
        &jira_client(&server),
        &database,
        ExecutorOptions::default(),
    )
    .await
    .expect("missing marked worklog should be reported, not recreated");

    assert_eq!(report.succeeded, 0);
    assert_eq!(report.failed, 1);
    assert_eq!(report.statuses, vec![MutationStatus::UnmanagedWorklog]);
    assert_eq!(
        update.hits(),
        0,
        "missing/unmarked worklog must not be updated"
    );
}

#[tokio::test]
async fn integration_edge_jira_worklog_manually_edited_marked_is_overwritten() {
    let (_file, database) = temp_database();
    insert_link(&database, "sha256:old");
    let server = MockServer::start();
    let new_hash = source_hash("SAB-123 edited in Toggl");
    server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200)
            .json_body(json!({ "key": MARKER_PROPERTY_KEY, "value": marker(&new_hash) }));
    });
    let update = server.mock(|when, then| {
        when.method(Method::PUT)
            .path(format!(
                "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}"
            ))
            .json_body_partial(r#"{"timeSpentSeconds":1800}"#);
        then.status(200).json_body(json!({ "id": WORKLOG_ID }));
    });
    let set_property = server.mock(|when, then| {
        when.method(Method::PUT).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200).json_body(json!({}));
    });
    let mut input = default_input(vec![entry(ENTRY_ID, "SAB-123 edited in Toggl")]);
    input.existing_links = vec![marked_link("sha256:old")];
    let plan = plan_sync(input).expect("planner should schedule update");

    let report = execute_plan(
        &plan,
        &jira_client(&server),
        &database,
        ExecutorOptions::default(),
    )
    .await
    .expect("marked worklog update should succeed");

    assert_eq!(report.statuses, vec![MutationStatus::Updated]);
    update.assert_hits(1);
    set_property.assert_hits(1);
}

#[tokio::test]
async fn integration_edge_no_unmarked_jira_worklog_is_mutated() {
    let (_file, database) = temp_database();
    insert_link(&database, "sha256:old");
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(404)
            .json_body(json!({ "errorMessages": ["missing property"] }));
    });
    let destructive_calls = server.mock(|when, then| {
        when.any_request()
            .matches(|request| matches!(request.method.as_str(), "PUT" | "DELETE"));
        then.status(500)
            .body("unmarked worklogs must not be mutated");
    });
    let mut input = default_input(vec![deleted_entry(ENTRY_ID, "SAB-123 removed")]);
    input.existing_links = vec![marked_link("sha256:old")];
    let plan = plan_sync(input).expect("planner should schedule delete for known link");

    let report = execute_plan(
        &plan,
        &jira_client(&server),
        &database,
        ExecutorOptions::default(),
    )
    .await
    .expect("unmarked delete should become a conflict");

    assert_eq!(report.statuses, vec![MutationStatus::Conflict]);
    assert_eq!(destructive_calls.hits(), 0);
}

#[tokio::test]
async fn integration_edge_429_retry_after_is_honored_without_db_link() {
    let (_file, database) = temp_database();
    let server = MockServer::start();
    let sleeper = RecordingSleeper::default();
    empty_worklog_list(&server);
    server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(429)
            .header("Retry-After", "2")
            .json_body(json!({ "errorMessages": ["Rate limit exceeded"] }));
    });
    let plan = plan_sync(default_input(vec![entry(ENTRY_ID, "SAB-123 rate limited")]))
        .expect("planner should create a mutation");

    let error = execute_plan(
        &plan,
        &recording_jira_client(&server, sleeper.clone()),
        &database,
        ExecutorOptions::default(),
    )
    .await
    .expect_err("429 should be surfaced after honoring retry-after");

    assert!(matches!(
        error,
        toggl_jira_sync::sync::executor::ExecutorError::Jira(JiraError::RateLimited { .. })
    ));
    assert_eq!(sleeper.durations(), vec![Duration::from_secs(2)]);
    assert_eq!(database.count_rows("jira_worklog_links").unwrap(), 0);
}

#[tokio::test]
async fn integration_edge_timeout_after_create_no_duplicate() {
    let (_file, database) = temp_database();
    let server = MockServer::start();
    let mut empty_list = empty_worklog_list(&server);
    let create = server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(201).json_body(json!({ "id": WORKLOG_ID }));
    });
    server.mock(|when, then| {
        when.method(Method::PUT).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200).json_body(json!({}));
    });
    let plan = plan_sync(default_input(vec![entry(
        ENTRY_ID,
        "SAB-123 created entry",
    )]))
    .expect("planner should create a mutation");

    execute_plan(
        &plan,
        &jira_client(&server),
        &database,
        ExecutorOptions {
            crash_after_remote_create: true,
        },
    )
    .await
    .expect_err("simulated network loss after remote create should skip DB commit");
    assert_eq!(database.count_rows("jira_worklog_links").unwrap(), 0);

    empty_list.delete();
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
        then.status(200).json_body(json!({
            "key": MARKER_PROPERTY_KEY,
            "value": marker(&source_hash("SAB-123 created entry"))
        }));
    });

    let report = execute_plan(
        &plan,
        &jira_client(&server),
        &database,
        ExecutorOptions::default(),
    )
    .await
    .expect("second run should recover the remote create");

    assert_eq!(report.statuses, vec![MutationStatus::RecoveredExisting]);
    assert_eq!(create.hits(), 1, "only the first run may create remotely");
    assert_eq!(database.count_rows("jira_worklog_links").unwrap(), 1);
}

#[test]
fn integration_edge_dst_timezone_offset_preserved_in_worklog_draft_and_hash() {
    let mut dst_entry = entry(ENTRY_ID, "SAB-123 DST work");
    dst_entry.start = "2024-03-31T01:30:00.000+0200".to_owned();

    let plan = plan_sync(default_input(vec![dst_entry])).expect("planner should create a mutation");

    let PlannedMutation::Create(create) = &plan.mutations[0] else {
        panic!("expected create mutation");
    };
    assert_eq!(create.draft.started, "2024-03-31T01:30:00.000+0200");
    assert_eq!(
        create.source_hash,
        compute_source_hash(
            ISSUE_KEY,
            SITE_KEY,
            1_800,
            "2024-03-31T01:30:00.000+0200",
            "SAB-123 DST work",
        )
    );
}

#[tokio::test]
async fn integration_edge_large_initial_backfill_uses_bounded_pages_windows() {
    let now = 1_714_604_800;
    let requested_since = now - 365 * 24 * 60 * 60;
    let bounded_since = 1_714_521_600;
    let toggl_server = MockServer::start();
    let bounded_fetch = toggl_server.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/v9/me/time_entries")
            .query_param("since", bounded_since.to_string().as_str());
        then.status(200).json_body(json!([]));
    });
    let unbounded_fetch = toggl_server.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/v9/me/time_entries")
            .query_param("since", requested_since.to_string().as_str());
        then.status(500)
            .body("unbounded historical fetch is forbidden");
    });
    let toggl = TogglClient::new(TogglClientConfig {
        base_url: toggl_server.base_url(),
        workspace_id: WORKSPACE_ID.parse().expect("numeric workspace id"),
        api_token: "fake-toggl-token-do-not-log".to_owned(),
        max_rps: 1.0,
        initial_backfill_days: 14,
    })
    .expect("toggl client config should be valid");

    toggl
        .fetch_bounded_backfill_since(requested_since, now)
        .await
        .expect("bounded Toggl backfill should use configured lower bound");

    bounded_fetch.assert_hits(1);
    assert_eq!(unbounded_fetch.hits(), 0);

    let (_file, database) = temp_database();
    let jira_server = MockServer::start();
    jira_server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({ "worklogs": [] }));
    });
    let report = recover(RecoveryInput {
        database: &database,
        entries: vec![entry(ENTRY_ID, "SAB-123 old entry")],
        issue_site_mappings: vec![IssueSiteMapping {
            issue_key: ISSUE_KEY.to_owned(),
            jira_site_key: SITE_KEY.to_owned(),
        }],
        recovery_sites: vec![RecoverySite {
            key: SITE_KEY.to_owned(),
            client: jira_client(&jira_server),
        }],
        recovery_scan_days: 14,
        requested_scan_days: Some(365),
    })
    .await
    .expect("bounded recovery should warn and continue");

    assert!(report
        .warnings
        .contains(&RecoveryWarning::RequestedWindowClamped {
            requested_days: 365,
            configured_days: 14,
        }));
    assert_eq!(
        report.scanned_issues, 1,
        "recovery scans one issue window, not unbounded pages"
    );
}

#[tokio::test]
async fn integration_edge_duplicate_marked_jira_worklogs_conflict_without_destructive_resolution() {
    let (_file, database) = temp_database();
    let server = MockServer::start();
    let expected_hash = source_hash("SAB-123 created entry");
    server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({
            "worklogs": [{ "id": WORKLOG_ID }, { "id": DUPLICATE_WORKLOG_ID }]
        }));
    });
    for worklog_id in [WORKLOG_ID, DUPLICATE_WORKLOG_ID] {
        server.mock(|when, then| {
            when.method(Method::GET).path(format!(
                "/rest/api/3/issue/{ISSUE_KEY}/worklog/{worklog_id}/properties/{MARKER_PROPERTY_KEY}"
            ));
            then.status(200)
                .json_body(json!({ "key": MARKER_PROPERTY_KEY, "value": marker(&expected_hash) }));
        });
    }
    let destructive_calls = server.mock(|when, then| {
        when.any_request()
            .matches(|request| matches!(request.method.as_str(), "POST" | "PUT" | "DELETE"));
        then.status(500)
            .body("duplicates must be reported, not auto-mutated");
    });

    let report = recover(RecoveryInput {
        database: &database,
        entries: vec![entry(ENTRY_ID, "SAB-123 created entry")],
        issue_site_mappings: vec![IssueSiteMapping {
            issue_key: ISSUE_KEY.to_owned(),
            jira_site_key: SITE_KEY.to_owned(),
        }],
        recovery_sites: vec![RecoverySite {
            key: SITE_KEY.to_owned(),
            client: jira_client(&server),
        }],
        recovery_scan_days: 180,
        requested_scan_days: None,
    })
    .await
    .expect("duplicate markers should be reported, not fatal");

    assert_eq!(report.recovered_links, 0);
    assert!(report
        .conflicts
        .contains(&RecoveryConflict::DuplicateMarkedWorklogs {
            toggl_workspace_id: WORKSPACE_ID.to_owned(),
            toggl_entry_id: ENTRY_ID.to_owned(),
            jira_site_key: SITE_KEY.to_owned(),
            jira_issue_key: ISSUE_KEY.to_owned(),
            jira_worklog_ids: vec![WORKLOG_ID.to_owned(), DUPLICATE_WORKLOG_ID.to_owned()],
        }));
    assert_eq!(database.count_rows("jira_worklog_links").unwrap(), 0);
    assert_eq!(destructive_calls.hits(), 0);
}
