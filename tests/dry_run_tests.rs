#[allow(dead_code)]
mod support;

use std::{fs, process::Command};

use httpmock::{Method, MockServer};
use regex::Regex;
use serde_json::Value;
use support::{fixture_text, temp_db, toggl_auth, FixtureName};
use toggl_jira_sync::{
    db::{
        Database, NewJiraIssueSiteCache, NewJiraWorklogLink, NewSyncAttempt, NewSyncRun,
        NewTogglEntry,
    },
    report::{DryRunReport, PlannedAction},
    sync::planner::IssueSiteMapping,
    toggl::{SkipReason, TogglFetchResult, TogglTimeEntry},
};

fn binary() -> &'static str {
    Box::leak(
        std::env::var("CARGO_BIN_EXE_toggl-jira-sync")
            .expect("Cargo should provide the test binary path")
            .into_boxed_str(),
    )
}

fn write_config(
    path: &std::path::Path,
    toggl_base_url: &str,
    sab_jira_base_url: &str,
    blogic_jira_base_url: &str,
) {
    fs::write(
        path,
        format!(
            r#"
[toggl]
workspace_id = 700001
api_token_env = "TOGGL_API_TOKEN"
base_url = "{}"

[rate_limits]
jira_global_write_delay_ms = 0
jira_same_issue_write_delay_ms = 0

[jira]

[[jira.sites]]
key = "sabservis"
base_url = "{}"
email_env = "SABSERVIS_JIRA_EMAIL"
api_token_env = "SABSERVIS_JIRA_API_TOKEN"
enabled = true

[[jira.sites]]
key = "blogic"
base_url = "{}"
email_env = "BLOGIC_JIRA_EMAIL"
api_token_env = "BLOGIC_JIRA_API_TOKEN"
enabled = true
"#,
            toggl_base_url, sab_jira_base_url, blogic_jira_base_url,
        ),
    )
    .expect("write config");
}

fn mock_issue_discovery(sab_jira: &MockServer, blogic_jira: &MockServer) {
    for issue_key in ["SAB-123", "SAB-789", "SAB-555"] {
        sab_jira.mock(|when, then| {
            when.method(Method::GET)
                .path(format!("/rest/api/3/issue/{issue_key}"));
            then.status(200)
                .json_body(serde_json::json!({ "key": issue_key }));
        });
        blogic_jira.mock(|when, then| {
            when.method(Method::GET)
                .path(format!("/rest/api/3/issue/{issue_key}"));
            then.status(404).json_body(serde_json::json!({}));
        });
    }

    sab_jira.mock(|when, then| {
        when.method(Method::GET)
            .path("/rest/api/3/issue/BLOGIC-456");
        then.status(404).json_body(serde_json::json!({}));
    });
    blogic_jira.mock(|when, then| {
        when.method(Method::GET)
            .path("/rest/api/3/issue/BLOGIC-456");
        then.status(200)
            .json_body(serde_json::json!({ "key": "BLOGIC-456" }));
    });
}

fn insert_blogic_link(db_path: &std::path::Path, source_hash: &str) {
    let database = Database::open(db_path).expect("open sqlite db");
    database.run_migrations().expect("migrations should run");
    database
        .insert_jira_worklog_link(&NewJiraWorklogLink {
            toggl_workspace_id: "700001",
            toggl_entry_id: "900002",
            jira_site_key: "blogic",
            jira_issue_key: "BLOGIC-456",
            jira_worklog_id: Some("20002"),
            source_hash,
            rounded_duration_seconds: 4_500,
            status: "created",
        })
        .expect("seed existing Jira link");
}

fn seed_issue_site_cache(db_path: &std::path::Path) {
    let database = Database::open(db_path).expect("open sqlite db");
    database.run_migrations().expect("migrations should run");
    for (issue_key, jira_site_key) in [
        ("SAB-123", "sabservis"),
        ("SAB-789", "sabservis"),
        ("SAB-555", "sabservis"),
        ("BLOGIC-456", "blogic"),
    ] {
        database
            .upsert_jira_issue_site_cache(&NewJiraIssueSiteCache {
                issue_key,
                jira_site_key,
                source: "test_seed",
            })
            .expect("seed issue site cache");
    }
}

#[test]
fn dry_run_outputs_parseable_json_report() {
    let report = DryRunReport::from_fetch_result_with_resolved_sites(
        TogglFetchResult {
            entries: vec![
                TogglTimeEntry {
                    workspace_id: "700001".to_owned(),
                    entry_id: "900001".to_owned(),
                    start: "2024-05-02T01:00:00Z".to_owned(),
                    duration_seconds: 1800,
                    description: Some("SAB-123 created entry".to_owned()),
                    updated_at: "2024-05-02T03:06:40Z".to_owned(),
                    deleted_at: None,
                    skip_reason: None,
                },
                TogglTimeEntry {
                    workspace_id: "700001".to_owned(),
                    entry_id: "900003".to_owned(),
                    start: "2024-05-01T10:00:00Z".to_owned(),
                    duration_seconds: 1200,
                    description: Some("SAB-789 deleted entry".to_owned()),
                    updated_at: "2024-05-02T03:06:40Z".to_owned(),
                    deleted_at: Some("2024-05-02T03:06:40Z".to_owned()),
                    skip_reason: None,
                },
                TogglTimeEntry {
                    workspace_id: "700001".to_owned(),
                    entry_id: "900004".to_owned(),
                    start: "2024-05-02T03:00:00Z".to_owned(),
                    duration_seconds: -1,
                    description: Some("SAB-555 running entry".to_owned()),
                    updated_at: "2024-05-02T03:06:40Z".to_owned(),
                    deleted_at: None,
                    skip_reason: Some(SkipReason::RunningEntry),
                },
            ],
            skipped: Vec::new(),
        },
        vec![
            IssueSiteMapping {
                issue_key: "SAB-123".to_owned(),
                jira_site_key: "sabservis".to_owned(),
            },
            IssueSiteMapping {
                issue_key: "SAB-555".to_owned(),
                jira_site_key: "sabservis".to_owned(),
            },
            IssueSiteMapping {
                issue_key: "SAB-789".to_owned(),
                jira_site_key: "sabservis".to_owned(),
            },
        ],
        Vec::new(),
    );

    let json = report.to_json_string().expect("report should serialize");
    let parsed: Value = serde_json::from_str(&json).expect("report JSON should parse");

    assert_eq!(parsed["summary"]["create_count"], 1);
    assert_eq!(parsed["summary"]["delete_count"], 1);
    assert_eq!(parsed["summary"]["update_count"], 0);
    assert_eq!(parsed["summary"]["move_count"], 0);
    assert_eq!(parsed["summary"]["no_op_count"], 0);
    assert_eq!(parsed["summary"]["skipped_count"], 1);
    assert_eq!(parsed["summary"]["errors_count"], 0);
    assert_eq!(parsed["summary"]["rate_limit_retries_avoided_count"], 0);
    assert_eq!(
        parsed["entries"].as_array().expect("entries array").len(),
        3
    );
    assert_eq!(parsed["entries"][0]["action"], "create");
    assert_eq!(parsed["entries"][1]["action"], "delete");
    assert_eq!(parsed["entries"][2]["action"], "skipped");
}

#[test]
fn dry_run_reports_multiple_issue_keys_as_error_without_mutation_action() {
    let report = DryRunReport::from_fetch_result_with_resolved_sites(
        TogglFetchResult {
            entries: vec![TogglTimeEntry {
                workspace_id: "700001".to_owned(),
                entry_id: "900009".to_owned(),
                start: "2024-05-02T01:00:00Z".to_owned(),
                duration_seconds: 1800,
                description: Some("SAB-123 and BLOGIC-456 conflict".to_owned()),
                updated_at: "2024-05-02T03:06:40Z".to_owned(),
                deleted_at: None,
                skip_reason: None,
            }],
            skipped: Vec::new(),
        },
        vec![
            IssueSiteMapping {
                issue_key: "SAB-123".to_owned(),
                jira_site_key: "sabservis".to_owned(),
            },
            IssueSiteMapping {
                issue_key: "BLOGIC-456".to_owned(),
                jira_site_key: "blogic".to_owned(),
            },
        ],
        Vec::new(),
    );

    assert_eq!(report.summary.errors_count, 1);
    assert_eq!(report.entries[0].action, PlannedAction::Error);
    assert!(report.entries[0]
        .reason
        .as_deref()
        .expect("error reason")
        .contains("multiple issue keys"));
}

#[test]
fn dry_run_reports_unresolved_issue_site_as_skip_without_mutation_action() {
    let report = DryRunReport::from_fetch_result_with_resolved_sites(
        TogglFetchResult {
            entries: vec![TogglTimeEntry {
                workspace_id: "700001".to_owned(),
                entry_id: "900010".to_owned(),
                start: "2024-05-02T01:00:00Z".to_owned(),
                duration_seconds: 1800,
                description: Some("BLT-121 belongs elsewhere".to_owned()),
                updated_at: "2024-05-02T03:06:40Z".to_owned(),
                deleted_at: None,
                skip_reason: None,
            }],
            skipped: Vec::new(),
        },
        vec![IssueSiteMapping {
            issue_key: "SAB-123".to_owned(),
            jira_site_key: "sabservis".to_owned(),
        }],
        Vec::new(),
    );

    assert_eq!(report.summary.skipped_count, 1);
    assert_eq!(report.summary.errors_count, 0);
    assert_eq!(report.entries[0].action, PlannedAction::Skipped);
    assert_eq!(
        report.entries[0].reason.as_deref(),
        Some("Jira issue BLT-121 was not found on any enabled Jira site")
    );
}

#[test]
fn dry_run_never_calls_jira_mutation_endpoints() {
    let toggl = MockServer::start();
    let sab_jira = MockServer::start();
    let blogic_jira = MockServer::start();
    let db = temp_db();
    let config = tempfile::NamedTempFile::new().expect("config file");
    write_config(
        config.path(),
        &toggl.base_url(),
        &sab_jira.base_url(),
        &blogic_jira.base_url(),
    );

    let toggl_mock = toggl.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/v9/me/time_entries")
            .header(
                "authorization",
                toggl_auth("fake-toggl-token-do-not-log").header_value(),
            );
        then.status(200)
            .header("content-type", "application/json")
            .body(fixture_text(FixtureName::TogglTimeEntries));
    });
    let sab_mutations = sab_jira.mock(|when, then| {
        when.any_request()
            .matches(|request| matches!(request.method.as_str(), "POST" | "PUT" | "DELETE"));
        then.status(500).body("dry-run must not mutate Jira");
    });
    let blogic_mutations = blogic_jira.mock(|when, then| {
        when.any_request()
            .matches(|request| matches!(request.method.as_str(), "POST" | "PUT" | "DELETE"));
        then.status(500).body("dry-run must not mutate Jira");
    });
    mock_issue_discovery(&sab_jira, &blogic_jira);

    let output = Command::new(binary())
        .args([
            "sync",
            "--dry-run",
            "--config",
            config.path().to_str().expect("config path UTF-8"),
            "--db",
            db.path().to_str().expect("db path UTF-8"),
            "--json",
        ])
        .env("TOGGL_API_TOKEN", "fake-toggl-token-do-not-log")
        .env("SABSERVIS_JIRA_EMAIL", "sync@example.test")
        .env("SABSERVIS_JIRA_API_TOKEN", "fake-jira-token-do-not-log")
        .env("BLOGIC_JIRA_EMAIL", "sync@example.test")
        .env("BLOGIC_JIRA_API_TOKEN", "fake-jira-token-do-not-log")
        .output()
        .expect("run dry-run command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    toggl_mock.assert();
    assert_eq!(sab_mutations.hits(), 0, "dry-run must not mutate SAB Jira");
    assert_eq!(
        blogic_mutations.hits(),
        0,
        "dry-run must not mutate BLOGIC Jira"
    );

    let parsed: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(parsed["mode"], "dry_run");
    assert_eq!(parsed["summary"]["create_count"], 2);
    assert_eq!(parsed["summary"]["delete_count"], 1);
    assert_eq!(parsed["summary"]["skipped_count"], 1);
    assert!(parsed["entries"].as_array().expect("entries array").len() >= 4);
}

#[test]
fn dry_run_uses_existing_sqlite_links_when_planning() {
    let toggl = MockServer::start();
    let sab_jira = MockServer::start();
    let blogic_jira = MockServer::start();
    let db = temp_db();
    let config = tempfile::NamedTempFile::new().expect("config file");
    write_config(
        config.path(),
        &toggl.base_url(),
        &sab_jira.base_url(),
        &blogic_jira.base_url(),
    );
    insert_blogic_link(db.path(), "sha256:old-blogic-link");
    mock_issue_discovery(&sab_jira, &blogic_jira);

    toggl.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/v9/me/time_entries")
            .header(
                "authorization",
                toggl_auth("fake-toggl-token-do-not-log").header_value(),
            );
        then.status(200)
            .header("content-type", "application/json")
            .body(fixture_text(FixtureName::TogglTimeEntries));
    });

    let output = Command::new(binary())
        .args([
            "sync",
            "--dry-run",
            "--config",
            config.path().to_str().expect("config path UTF-8"),
            "--db",
            db.path().to_str().expect("db path UTF-8"),
            "--json",
        ])
        .env("TOGGL_API_TOKEN", "fake-toggl-token-do-not-log")
        .env("SABSERVIS_JIRA_EMAIL", "sync@example.test")
        .env("SABSERVIS_JIRA_API_TOKEN", "fake-jira-token-do-not-log")
        .env("BLOGIC_JIRA_EMAIL", "sync@example.test")
        .env("BLOGIC_JIRA_API_TOKEN", "fake-jira-token-do-not-log")
        .output()
        .expect("run dry-run command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(parsed["summary"]["create_count"], 1);
    assert_eq!(parsed["summary"]["update_count"], 1);
    assert_eq!(parsed["entries"][1]["action"], "update");
}

#[test]
fn dry_run_uses_issue_site_cache_without_discovery_gets() {
    let toggl = MockServer::start();
    let sab_jira = MockServer::start();
    let blogic_jira = MockServer::start();
    let db = temp_db();
    seed_issue_site_cache(db.path());
    let config = tempfile::NamedTempFile::new().expect("config file");
    write_config(
        config.path(),
        &toggl.base_url(),
        &sab_jira.base_url(),
        &blogic_jira.base_url(),
    );

    toggl.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/v9/me/time_entries")
            .header(
                "authorization",
                toggl_auth("fake-toggl-token-do-not-log").header_value(),
            );
        then.status(200)
            .header("content-type", "application/json")
            .body(fixture_text(FixtureName::TogglTimeEntries));
    });
    let sab_discovery = sab_jira.mock(|when, then| {
        when.method(Method::GET)
            .path_matches(Regex::new(r"^/rest/api/3/issue/[^/]+$").expect("regex"));
        then.status(500).body("cache hit must avoid discovery GET");
    });
    let blogic_discovery = blogic_jira.mock(|when, then| {
        when.method(Method::GET)
            .path_matches(Regex::new(r"^/rest/api/3/issue/[^/]+$").expect("regex"));
        then.status(500).body("cache hit must avoid discovery GET");
    });

    let output = Command::new(binary())
        .args([
            "sync",
            "--dry-run",
            "--config",
            config.path().to_str().expect("config path UTF-8"),
            "--db",
            db.path().to_str().expect("db path UTF-8"),
            "--json",
        ])
        .env("TOGGL_API_TOKEN", "fake-toggl-token-do-not-log")
        .env("SABSERVIS_JIRA_EMAIL", "sync@example.test")
        .env("SABSERVIS_JIRA_API_TOKEN", "fake-jira-token-do-not-log")
        .env("BLOGIC_JIRA_EMAIL", "sync@example.test")
        .env("BLOGIC_JIRA_API_TOKEN", "fake-jira-token-do-not-log")
        .output()
        .expect("run dry-run command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(sab_discovery.hits(), 0);
    assert_eq!(blogic_discovery.hits(), 0);
}

#[test]
fn dry_run_retries_historical_error_entries_not_returned_by_toggl_fetch() {
    let toggl = MockServer::start();
    let sab_jira = MockServer::start();
    let blogic_jira = MockServer::start();
    let db = temp_db();
    let config = tempfile::NamedTempFile::new().expect("config file");
    write_config(
        config.path(),
        &toggl.base_url(),
        &sab_jira.base_url(),
        &blogic_jira.base_url(),
    );

    toggl.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/v9/me/time_entries")
            .header(
                "authorization",
                toggl_auth("fake-toggl-token-do-not-log").header_value(),
            );
        then.status(200)
            .header("content-type", "application/json")
            .body("[]");
    });
    sab_jira.mock(|when, then| {
        when.method(Method::GET).path("/rest/api/3/issue/SAB-123");
        then.status(200)
            .json_body(serde_json::json!({ "key": "SAB-123" }));
    });
    blogic_jira.mock(|when, then| {
        when.method(Method::GET).path("/rest/api/3/issue/SAB-123");
        then.status(404).json_body(serde_json::json!({}));
    });

    let database = Database::open(db.path()).expect("open sqlite db");
    database.run_migrations().expect("migrations should run");
    let sync_run_id = database
        .insert_sync_run(&NewSyncRun {
            run_id: "historical-error-run",
            mode: "sync",
            status: "completed",
        })
        .expect("seed sync run");
    database
        .upsert_toggl_entry(&NewTogglEntry {
            toggl_workspace_id: "700001",
            toggl_entry_id: "old-error-entry",
            description: Some("SAB-123 fixed description"),
            extracted_issue_key: Some("SAB-123"),
            source_hash: "sha256:old-error-entry",
            rounded_duration_seconds: 1800,
            status: "error",
            started_at: Some("2024-05-02T01:00:00Z"),
            stopped_at: Some("2024-05-02T01:30:00Z"),
        })
        .expect("seed old error entry");
    database
        .insert_sync_attempt(&NewSyncAttempt {
            sync_run_id: Some(sync_run_id),
            toggl_workspace_id: "700001",
            toggl_entry_id: "old-error-entry",
            jira_site_key: None,
            jira_issue_key: Some("SAB-123"),
            jira_worklog_id: None,
            status: "error",
            error_message: Some("old resolver error"),
        })
        .expect("seed old error attempt");
    drop(database);

    let output = Command::new(binary())
        .args([
            "sync",
            "--dry-run",
            "--config",
            config.path().to_str().expect("config path UTF-8"),
            "--db",
            db.path().to_str().expect("db path UTF-8"),
            "--json",
        ])
        .env("TOGGL_API_TOKEN", "fake-toggl-token-do-not-log")
        .env("SABSERVIS_JIRA_EMAIL", "sync@example.test")
        .env("SABSERVIS_JIRA_API_TOKEN", "fake-jira-token-do-not-log")
        .env("BLOGIC_JIRA_EMAIL", "sync@example.test")
        .env("BLOGIC_JIRA_API_TOKEN", "fake-jira-token-do-not-log")
        .output()
        .expect("run dry-run command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(parsed["summary"]["create_count"], 1);
    assert_eq!(parsed["entries"][0]["toggl_entry_id"], "old-error-entry");
    assert_eq!(parsed["entries"][0]["action"], "create");
}

#[test]
fn dry_run_plans_completed_local_entries_not_returned_by_toggl_fetch() {
    let toggl = MockServer::start();
    let sab_jira = MockServer::start();
    let blogic_jira = MockServer::start();
    let db = temp_db();
    let config = tempfile::NamedTempFile::new().expect("config file");
    write_config(
        config.path(),
        &toggl.base_url(),
        &sab_jira.base_url(),
        &blogic_jira.base_url(),
    );

    toggl.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/v9/me/time_entries")
            .header(
                "authorization",
                toggl_auth("fake-toggl-token-do-not-log").header_value(),
            );
        then.status(200)
            .header("content-type", "application/json")
            .body("[]");
    });
    sab_jira.mock(|when, then| {
        when.method(Method::GET).path("/rest/api/3/issue/SAB-123");
        then.status(200)
            .json_body(serde_json::json!({ "key": "SAB-123" }));
    });
    blogic_jira.mock(|when, then| {
        when.method(Method::GET).path("/rest/api/3/issue/SAB-123");
        then.status(404).json_body(serde_json::json!({}));
    });

    let database = Database::open(db.path()).expect("open sqlite db");
    database.run_migrations().expect("migrations should run");
    database
        .upsert_toggl_entry(&NewTogglEntry {
            toggl_workspace_id: "700001",
            toggl_entry_id: "local-planned-entry",
            description: Some("SAB-123 local planned entry"),
            extracted_issue_key: Some("SAB-123"),
            source_hash: "sha256:local-planned-entry",
            rounded_duration_seconds: 1_800,
            status: "planned",
            started_at: Some("2024-05-02T01:00:00Z"),
            stopped_at: Some("2024-05-02T01:30:00Z"),
        })
        .expect("seed local planned entry");
    drop(database);

    let output = Command::new(binary())
        .args([
            "sync",
            "--dry-run",
            "--config",
            config.path().to_str().expect("config path UTF-8"),
            "--db",
            db.path().to_str().expect("db path UTF-8"),
            "--json",
        ])
        .env("TOGGL_API_TOKEN", "fake-toggl-token-do-not-log")
        .env("SABSERVIS_JIRA_EMAIL", "sync@example.test")
        .env("SABSERVIS_JIRA_API_TOKEN", "fake-jira-token-do-not-log")
        .env("BLOGIC_JIRA_EMAIL", "sync@example.test")
        .env("BLOGIC_JIRA_API_TOKEN", "fake-jira-token-do-not-log")
        .output()
        .expect("run dry-run command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(parsed["summary"]["create_count"], 1);
    assert_eq!(
        parsed["entries"][0]["toggl_entry_id"],
        "local-planned-entry"
    );
    assert_eq!(parsed["entries"][0]["action"], "create");
}

#[test]
fn non_dry_run_sync_executes_planned_jira_writes_and_persists_links() {
    let toggl = MockServer::start();
    let sab_jira = MockServer::start();
    let blogic_jira = MockServer::start();
    let db = temp_db();
    let config = tempfile::NamedTempFile::new().expect("config file");
    write_config(
        config.path(),
        &toggl.base_url(),
        &sab_jira.base_url(),
        &blogic_jira.base_url(),
    );
    mock_issue_discovery(&sab_jira, &blogic_jira);

    toggl.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/v9/me/time_entries")
            .header(
                "authorization",
                toggl_auth("fake-toggl-token-do-not-log").header_value(),
            );
        then.status(200)
            .header("content-type", "application/json")
            .body(fixture_text(FixtureName::TogglTimeEntries));
    });
    for (issue_key, worklog_id) in [("SAB-123", "10001"), ("BLOGIC-456", "20002")] {
        let jira = if issue_key.starts_with("SAB-") {
            &sab_jira
        } else {
            &blogic_jira
        };
        jira.mock(|when, then| {
            when.method(Method::GET)
                .path(format!("/rest/api/3/issue/{issue_key}/worklog"));
            then.status(200)
                .json_body(serde_json::json!({ "worklogs": [] }));
        });
        jira.mock(|when, then| {
            when.method(Method::POST)
                .path(format!("/rest/api/3/issue/{issue_key}/worklog"));
            then.status(201)
                .json_body(serde_json::json!({ "id": worklog_id }));
        });
        jira.mock(|when, then| {
            when.method(Method::PUT).path(format!(
                "/rest/api/3/issue/{issue_key}/worklog/{worklog_id}/properties/com.toggl_jira_sync"
            ));
            then.status(200).json_body(serde_json::json!({}));
        });
    }

    let output = Command::new(binary())
        .args([
            "sync",
            "--config",
            config.path().to_str().expect("config path UTF-8"),
            "--db",
            db.path().to_str().expect("db path UTF-8"),
            "--json",
        ])
        .env("TOGGL_API_TOKEN", "fake-toggl-token-do-not-log")
        .env("SABSERVIS_JIRA_EMAIL", "sync@example.test")
        .env("SABSERVIS_JIRA_API_TOKEN", "fake-jira-token-do-not-log")
        .env("BLOGIC_JIRA_EMAIL", "sync@example.test")
        .env("BLOGIC_JIRA_API_TOKEN", "fake-jira-token-do-not-log")
        .output()
        .expect("run sync command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let database = Database::open(db.path()).expect("open sqlite db after sync");
    database.run_migrations().expect("migrations should run");
    assert_eq!(database.count_rows("jira_worklog_links").unwrap(), 2);
}
