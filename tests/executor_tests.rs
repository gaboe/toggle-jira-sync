use httpmock::{Method, MockServer};
use serde_json::json;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;
use toggl_jira_sync::{
    db::{Database, NewJiraWorklogLink},
    jira::{JiraClient, TogglSyncMarker, WorklogDraft, MARKER_PROPERTY_KEY},
    sync::{
        executor::{
            execute_plan, ExecutorConflict, ExecutorFailurePolicy, ExecutorOptions, MutationStatus,
        },
        planner::{PlannedCreate, PlannedDelete, PlannedMutation, PlannedUpdate, SyncPlan},
    },
};

const EMAIL: &str = "sync@example.test";
const TOKEN: &str = "fake-jira-token-do-not-log";
const SITE_KEY: &str = "sabservis";
const ISSUE_KEY: &str = "SAB-123";
const MOVED_ISSUE_KEY: &str = "SAB-456";
const WORKLOG_ID: &str = "10001";
const MOVED_WORKLOG_ID: &str = "10002";

fn temp_database() -> (NamedTempFile, Database) {
    let file = NamedTempFile::new().expect("temp sqlite file should be created");
    let database = Database::open(file.path()).expect("temp sqlite database should open");
    database.run_migrations().expect("migrations should run");
    (file, database)
}

fn client(server: &MockServer) -> JiraClient {
    JiraClient::from_credentials(server.base_url(), EMAIL.to_owned(), TOKEN.to_owned())
}

fn draft(comment: &str) -> WorklogDraft {
    WorklogDraft {
        started: "2024-05-02T01:00:00.000+0000".to_owned(),
        time_spent_seconds: 1_800,
        comment: comment.to_owned(),
    }
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

fn create_mutation(issue_key: &str, worklog_id_source_hash: &str) -> PlannedMutation {
    create_mutation_for("700001", "900001", issue_key, worklog_id_source_hash)
}

fn create_mutation_for(
    workspace_id: &str,
    entry_id: &str,
    issue_key: &str,
    worklog_id_source_hash: &str,
) -> PlannedMutation {
    PlannedMutation::Create(PlannedCreate {
        toggl_workspace_id: workspace_id.to_owned(),
        toggl_entry_id: entry_id.to_owned(),
        jira_site_key: SITE_KEY.to_owned(),
        jira_issue_key: issue_key.to_owned(),
        draft: draft(&format!("{issue_key} created entry")),
        source_hash: worklog_id_source_hash.to_owned(),
    })
}

fn update_mutation(source_hash: &str) -> PlannedMutation {
    PlannedMutation::Update(PlannedUpdate {
        toggl_workspace_id: "700001".to_owned(),
        toggl_entry_id: "900001".to_owned(),
        jira_site_key: SITE_KEY.to_owned(),
        jira_issue_key: ISSUE_KEY.to_owned(),
        jira_worklog_id: WORKLOG_ID.to_owned(),
        draft: draft("SAB-123 updated from Toggl"),
        previous_source_hash: "sha256:old".to_owned(),
        source_hash: source_hash.to_owned(),
    })
}

fn delete_mutation(source_hash: &str) -> PlannedMutation {
    PlannedMutation::Delete(PlannedDelete {
        toggl_workspace_id: "700001".to_owned(),
        toggl_entry_id: "900001".to_owned(),
        jira_site_key: SITE_KEY.to_owned(),
        jira_issue_key: ISSUE_KEY.to_owned(),
        jira_worklog_id: WORKLOG_ID.to_owned(),
        source_hash: source_hash.to_owned(),
    })
}

fn plan(mutations: Vec<PlannedMutation>) -> SyncPlan {
    SyncPlan {
        entries: Vec::new(),
        mutations,
    }
}

fn insert_link(db: &Database, source_hash: &str) {
    db.insert_jira_worklog_link(&NewJiraWorklogLink {
        toggl_workspace_id: "700001",
        toggl_entry_id: "900001",
        jira_site_key: SITE_KEY,
        jira_issue_key: ISSUE_KEY,
        jira_worklog_id: Some(WORKLOG_ID),
        source_hash,
        rounded_duration_seconds: 1_800,
        status: "created",
    })
    .expect("link should insert");
}

#[tokio::test]
async fn executor_idempotent_create_discovers_existing_marker_before_post() {
    let (_file, db) = temp_database();
    let server = MockServer::start();
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
            .json_body(json!({ "key": MARKER_PROPERTY_KEY, "value": marker("sha256:create") }));
    });
    let create = server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(201).json_body(json!({ "id": "duplicate" }));
    });

    let report = execute_plan(
        &plan(vec![create_mutation(ISSUE_KEY, "sha256:create")]),
        &client(&server),
        &db,
        ExecutorOptions::default(),
    )
    .await
    .expect("executor should succeed");

    assert_eq!(report.succeeded, 1);
    assert_eq!(
        create.hits(),
        0,
        "existing marked worklog must prevent duplicate POST"
    );
    assert_eq!(db.count_rows("jira_worklog_links").unwrap(), 1);
    list.assert();
    property.assert();
}

#[tokio::test]
async fn executor_adopts_matching_external_worklog_before_post() {
    let (_file, db) = temp_database();
    let server = MockServer::start();
    let list = server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({
            "worklogs": [{
                "id": WORKLOG_ID,
                "started": "2024-05-02T01:00:00.000+0000",
                "timeSpentSeconds": 1800,
                "comment": {
                    "type":"doc",
                    "version":1,
                    "content":[{"type":"paragraph","content":[{"type":"text","text":"SAB-123 created entry"}]}]
                }
            }]
        }));
    });
    let property = server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(404)
            .json_body(json!({ "errorMessages": ["missing"] }));
    });
    let create = server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(201).json_body(json!({ "id": "duplicate" }));
    });

    let report = execute_plan(
        &plan(vec![create_mutation(ISSUE_KEY, "sha256:create")]),
        &client(&server),
        &db,
        ExecutorOptions::default(),
    )
    .await
    .expect("executor should adopt matching external worklog");

    assert_eq!(report.succeeded, 1);
    assert_eq!(report.statuses, vec![MutationStatus::AdoptedExternal]);
    assert_eq!(create.hits(), 0, "adoption must not duplicate POST");
    let stored = db
        .get_jira_worklog_link("700001", "900001", SITE_KEY)
        .expect("link lookup should succeed")
        .expect("adopted link should be stored");
    assert_eq!(stored.status, "adopted");
    list.assert();
    property.assert();
}

#[tokio::test]
async fn executor_adopts_legacy_external_worklog_with_offset_and_comment_variants() {
    let (_file, db) = temp_database();
    let server = MockServer::start();
    let list = server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({
            "worklogs": [{
                "id": WORKLOG_ID,
                "started": "2024-05-02T01:00:00.000+0200",
                "timeSpentSeconds": 1800,
                "comment": {
                    "type":"doc",
                    "version":1,
                    "content":[{
                        "type":"paragraph",
                        "content":[
                            {"type":"inlineCard","attrs":{"url":"https://example.test/browse/SAB-123"}},
                            {"type":"text","text":"    created entry"}
                        ]
                    }]
                }
            }]
        }));
    });
    let property = server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(404)
            .json_body(json!({ "errorMessages": ["missing"] }));
    });
    let create = server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(201).json_body(json!({ "id": "duplicate" }));
    });

    let report = execute_plan(
        &plan(vec![create_mutation(ISSUE_KEY, "sha256:create")]),
        &client(&server),
        &db,
        ExecutorOptions::default(),
    )
    .await
    .expect("executor should adopt legacy matching external worklog");

    assert_eq!(report.statuses, vec![MutationStatus::AdoptedExternal]);
    assert_eq!(create.hits(), 0, "legacy adoption must not duplicate POST");
    let stored = db
        .get_jira_worklog_link("700001", "900001", SITE_KEY)
        .expect("link lookup should succeed")
        .expect("adopted link should be stored");
    assert_eq!(stored.jira_worklog_id.as_deref(), Some(WORKLOG_ID));
    assert_eq!(stored.status, "adopted");
    list.assert();
    property.assert();
}

#[tokio::test]
async fn executor_crash_after_create_recovers_marked_worklog() {
    let (_file, db) = temp_database();
    let server = MockServer::start();
    let mut empty_list = server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({ "worklogs": [] }));
    });
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

    let crash = execute_plan(
        &plan(vec![create_mutation(ISSUE_KEY, "sha256:create")]),
        &client(&server),
        &db,
        ExecutorOptions {
            crash_after_remote_create: true,
            ..ExecutorOptions::default()
        },
    )
    .await
    .expect_err("test option simulates a crash before DB commit");
    assert!(crash.to_string().contains("simulated crash"));
    assert_eq!(db.count_rows("jira_worklog_links").unwrap(), 0);

    empty_list.delete();
    let recovered_list = server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200)
            .json_body(json!({ "worklogs": [{ "id": WORKLOG_ID }] }));
    });
    let recovered_property = server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200)
            .json_body(json!({ "key": MARKER_PROPERTY_KEY, "value": marker("sha256:create") }));
    });

    execute_plan(
        &plan(vec![create_mutation(ISSUE_KEY, "sha256:create")]),
        &client(&server),
        &db,
        ExecutorOptions::default(),
    )
    .await
    .expect("second run should recover marked worklog");

    assert_eq!(create.hits(), 1, "only the first run may POST a worklog");
    assert_eq!(db.count_rows("jira_worklog_links").unwrap(), 1);
    recovered_list.assert();
    recovered_property.assert();
}

#[tokio::test]
async fn executor_update_overwrites_manual_edit_when_marker_matches() {
    let (_file, db) = temp_database();
    insert_link(&db, "sha256:old");
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200)
            .json_body(json!({ "key": MARKER_PROPERTY_KEY, "value": marker("sha256:new") }));
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

    let report = execute_plan(
        &plan(vec![update_mutation("sha256:new")]),
        &client(&server),
        &db,
        ExecutorOptions::default(),
    )
    .await
    .expect("marked manual edit should be overwritten");

    assert_eq!(report.succeeded, 1);
    update.assert();
    set_property.assert();
}

#[tokio::test]
async fn executor_refuses_to_update_unmarked_worklog() {
    let (_file, db) = temp_database();
    insert_link(&db, "sha256:old");
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(404)
            .json_body(json!({ "errorMessages": ["missing property"] }));
    });
    let update = server.mock(|when, then| {
        when.method(Method::PUT).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}"
        ));
        then.status(200).json_body(json!({ "id": WORKLOG_ID }));
    });

    let report = execute_plan(
        &plan(vec![update_mutation("sha256:new")]),
        &client(&server),
        &db,
        ExecutorOptions::default(),
    )
    .await
    .expect("executor should report permanent unmanaged-worklog error");

    assert_eq!(report.succeeded, 0);
    assert_eq!(report.failed, 1);
    assert!(matches!(
        report.statuses[0],
        MutationStatus::UnmanagedWorklog
    ));
    assert_eq!(update.hits(), 0, "unmarked worklog must not be mutated");
}

#[tokio::test]
async fn executor_delete_verifies_marker_before_removing_link() {
    let (_file, db) = temp_database();
    insert_link(&db, "sha256:old");
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200)
            .json_body(json!({ "key": MARKER_PROPERTY_KEY, "value": marker("sha256:old") }));
    });
    let delete = server.mock(|when, then| {
        when.method(Method::DELETE).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}"
        ));
        then.status(204).body("");
    });

    execute_plan(
        &plan(vec![delete_mutation("sha256:old")]),
        &client(&server),
        &db,
        ExecutorOptions::default(),
    )
    .await
    .expect("marked delete should succeed");

    delete.assert();
}

#[tokio::test]
async fn executor_move_deletes_old_before_creating_new() {
    let (_file, db) = temp_database();
    insert_link(&db, "sha256:old");
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200)
            .json_body(json!({ "key": MARKER_PROPERTY_KEY, "value": marker("sha256:old") }));
    });
    let delete = server.mock(|when, then| {
        when.method(Method::DELETE).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}"
        ));
        then.status(204).body("");
    });
    server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{MOVED_ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({ "worklogs": [] }));
    });
    let create = server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{MOVED_ISSUE_KEY}/worklog"));
        then.status(201)
            .json_body(json!({ "id": MOVED_WORKLOG_ID }));
    });
    server.mock(|when, then| {
        when.method(Method::PUT).path(format!(
            "/rest/api/3/issue/{MOVED_ISSUE_KEY}/worklog/{MOVED_WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200).json_body(json!({}));
    });

    execute_plan(
        &plan(vec![
            delete_mutation("sha256:old"),
            create_mutation(MOVED_ISSUE_KEY, "sha256:moved"),
        ]),
        &client(&server),
        &db,
        ExecutorOptions::default(),
    )
    .await
    .expect("move should delete then create");

    delete.assert();
    create.assert();
}

#[tokio::test]
async fn executor_move_delete_permanent_failure_blocks_new_duplicate() {
    let (_file, db) = temp_database();
    insert_link(&db, "sha256:old");
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(404)
            .json_body(json!({ "errorMessages": ["missing property"] }));
    });
    let create = server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{MOVED_ISSUE_KEY}/worklog"));
        then.status(201)
            .json_body(json!({ "id": MOVED_WORKLOG_ID }));
    });

    let report = execute_plan(
        &plan(vec![
            delete_mutation("sha256:old"),
            create_mutation(MOVED_ISSUE_KEY, "sha256:moved"),
        ]),
        &client(&server),
        &db,
        ExecutorOptions::default(),
    )
    .await
    .expect("permanent delete failure should be reported, not panic");

    assert_eq!(report.succeeded, 0);
    assert_eq!(report.failed, 2);
    assert!(report
        .conflicts
        .contains(&ExecutorConflict::MoveDeleteFailed {
            toggl_workspace_id: "700001".to_owned(),
            toggl_entry_id: "900001".to_owned(),
        }));
    assert_eq!(
        create.hits(),
        0,
        "move must not create duplicate after old delete failure"
    );
}

#[tokio::test]
async fn executor_runs_independent_groups_in_parallel_and_reports_plan_order() {
    let (_file, db) = temp_database();
    let server = MockServer::start();
    for (issue_key, worklog_id) in [(ISSUE_KEY, WORKLOG_ID), (MOVED_ISSUE_KEY, MOVED_WORKLOG_ID)] {
        server.mock(|when, then| {
            when.method(Method::GET)
                .path(format!("/rest/api/3/issue/{issue_key}/worklog"));
            then.status(200)
                .delay(Duration::from_millis(150))
                .json_body(json!({ "worklogs": [] }));
        });
        server.mock(|when, then| {
            when.method(Method::POST)
                .path(format!("/rest/api/3/issue/{issue_key}/worklog"));
            then.status(201).json_body(json!({ "id": worklog_id }));
        });
        server.mock(|when, then| {
            when.method(Method::PUT).path(format!(
                "/rest/api/3/issue/{issue_key}/worklog/{worklog_id}/properties/{MARKER_PROPERTY_KEY}"
            ));
            then.status(200).json_body(json!({}));
        });
    }

    let started = Instant::now();
    let report = execute_plan(
        &plan(vec![
            create_mutation_for("700001", "900001", ISSUE_KEY, "sha256:first"),
            create_mutation_for("700001", "900002", MOVED_ISSUE_KEY, "sha256:second"),
        ]),
        &client(&server),
        &db,
        ExecutorOptions {
            max_parallel_groups: 2,
            failure_policy: ExecutorFailurePolicy::ContinueIndependent,
            ..ExecutorOptions::default()
        },
    )
    .await
    .expect("independent groups should complete");

    assert_eq!(report.succeeded, 2);
    assert_eq!(
        report.statuses,
        vec![MutationStatus::Created, MutationStatus::Created]
    );
    assert!(
        started.elapsed() < Duration::from_millis(280),
        "two independent 150ms preflight requests should overlap"
    );
}

#[tokio::test]
async fn executor_continue_independent_keeps_other_group_after_jira_error() {
    let (_file, db) = temp_database();
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(500)
            .json_body(json!({ "errorMessages": ["temporary Jira failure"] }));
    });
    server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{MOVED_ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({ "worklogs": [] }));
    });
    let create = server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{MOVED_ISSUE_KEY}/worklog"));
        then.status(201)
            .json_body(json!({ "id": MOVED_WORKLOG_ID }));
    });
    server.mock(|when, then| {
        when.method(Method::PUT).path(format!(
            "/rest/api/3/issue/{MOVED_ISSUE_KEY}/worklog/{MOVED_WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200).json_body(json!({}));
    });

    let report = execute_plan(
        &plan(vec![
            create_mutation_for("700001", "900001", ISSUE_KEY, "sha256:first"),
            create_mutation_for("700001", "900002", MOVED_ISSUE_KEY, "sha256:second"),
        ]),
        &client(&server),
        &db,
        ExecutorOptions {
            max_parallel_groups: 2,
            failure_policy: ExecutorFailurePolicy::ContinueIndependent,
            ..ExecutorOptions::default()
        },
    )
    .await
    .expect("Jira error in one group should not stop another group");

    assert_eq!(report.succeeded, 1);
    assert_eq!(report.failed, 1);
    assert!(matches!(report.statuses[0], MutationStatus::Error(_)));
    assert_eq!(report.statuses[1], MutationStatus::Created);
    create.assert();
}

#[tokio::test]
async fn executor_continue_independent_failed_move_delete_blocks_create() {
    let (_file, db) = temp_database();
    insert_link(&db, "sha256:old");
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200)
            .json_body(json!({ "key": MARKER_PROPERTY_KEY, "value": marker("sha256:old") }));
    });
    server.mock(|when, then| {
        when.method(Method::DELETE).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}"
        ));
        then.status(500)
            .json_body(json!({ "errorMessages": ["delete failed"] }));
    });
    let create = server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{MOVED_ISSUE_KEY}/worklog"));
        then.status(201)
            .json_body(json!({ "id": MOVED_WORKLOG_ID }));
    });

    let report = execute_plan(
        &plan(vec![
            delete_mutation("sha256:old"),
            create_mutation(MOVED_ISSUE_KEY, "sha256:moved"),
        ]),
        &client(&server),
        &db,
        ExecutorOptions {
            max_parallel_groups: 4,
            failure_policy: ExecutorFailurePolicy::ContinueIndependent,
            ..ExecutorOptions::default()
        },
    )
    .await
    .expect("Jira delete error should be contained to its group");

    assert_eq!(report.succeeded, 0);
    assert_eq!(report.failed, 2);
    assert!(matches!(report.statuses[0], MutationStatus::Error(_)));
    assert_eq!(report.statuses[1], MutationStatus::Conflict);
    assert_eq!(
        create.hits(),
        0,
        "move must not create duplicate after old delete failure"
    );
}
