use std::{fs, process::Command};

use chrono::{DateTime, Local};
use tempfile::TempDir;
use toggl_jira_sync::db::{Database, NewJiraWorklogLink, NewTogglEntry};

fn binary() -> &'static str {
    Box::leak(
        std::env::var("CARGO_BIN_EXE_toggl-jira-sync")
            .expect("Cargo should provide the test binary path")
            .into_boxed_str(),
    )
}

fn default_config_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".config/toggl-jira-sync/config.toml")
}

#[test]
fn status_uses_db_override_without_credentials_or_api_calls() {
    let temp = TempDir::new().expect("temp dir should be created");
    let db_path = temp.path().join("ledger.sqlite");
    let config_db_path = temp.path().join("config-ledger.sqlite");
    let config_path = temp.path().join("config.toml");

    fs::write(
        &config_path,
        format!(
            r#"
[toggl]
workspace_id = 123
api_token_env = "TOGGL_STATUS_TEST_TOKEN"

[runtime]
sqlite_path = "{}"

[[jira.sites]]
key = "sabservis"
base_url = "https://example.atlassian.net"
email_env = "JIRA_STATUS_TEST_EMAIL"
api_token_env = "JIRA_STATUS_TEST_TOKEN"
"#,
            config_db_path.display()
        ),
    )
    .expect("config should be written");

    let db = Database::open(&db_path).expect("db should open");
    db.run_migrations().expect("migrations should run");
    db.upsert_toggl_entry(&NewTogglEntry {
        toggl_workspace_id: "123",
        toggl_entry_id: "456",
        description: Some("SAB-456 implementation"),
        extracted_issue_key: Some("SAB-456"),
        source_hash: "sha256:status",
        rounded_duration_seconds: 1800,
        status: "created",
        started_at: Some("2024-05-02T03:06:40Z"),
        stopped_at: Some("2024-05-02T03:36:40Z"),
    })
    .expect("toggl entry should insert");
    db.upsert_jira_worklog_link(&NewJiraWorklogLink {
        toggl_workspace_id: "123",
        toggl_entry_id: "456",
        jira_site_key: "sabservis",
        jira_issue_key: "SAB-456",
        jira_worklog_id: Some("10001"),
        source_hash: "sha256:status",
        rounded_duration_seconds: 1800,
        status: "created",
    })
    .expect("jira link should insert");
    drop(db);

    let output = Command::new(binary())
        .args([
            "status",
            "--config",
            config_path.to_str().expect("config path should be utf8"),
            "--db",
            db_path.to_str().expect("db path should be utf8"),
            "--json",
        ])
        .env_remove("TOGGL_STATUS_TEST_TOKEN")
        .env_remove("JIRA_STATUS_TEST_EMAIL")
        .env_remove("JIRA_STATUS_TEST_TOKEN")
        .output()
        .expect("status command should run");

    assert!(
        output.status.success(),
        "stderr was: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::metadata(&config_db_path).is_err(),
        "--db override should avoid opening runtime.sqlite_path"
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    assert_eq!(json["summary"]["synced_count"], 1);
    assert_eq!(json["entries"][0]["workspace"], "123");
    assert_eq!(json["entries"][0]["entry"], "456");
    assert_eq!(json["entries"][0]["issue_key"], "SAB-456");
    assert_eq!(json["entries"][0]["duration_seconds"], 1800);
    assert_eq!(json["entries"][0]["site"], "sabservis");
    assert_eq!(json["entries"][0]["worklog_id"], "10001");
    assert_eq!(json["entries"][0]["status"], "synced");
}

#[test]
fn status_hides_deleted_toggl_entries() {
    let temp = TempDir::new().expect("temp dir should be created");
    let db_path = temp.path().join("ledger.sqlite");
    let config_path = temp.path().join("config.toml");

    fs::write(
        &config_path,
        r#"
[toggl]
workspace_id = 123
api_token_env = "TOGGL_STATUS_TEST_TOKEN"

[[jira.sites]]
key = "sabservis"
base_url = "https://example.atlassian.net"
email_env = "JIRA_STATUS_TEST_EMAIL"
api_token_env = "JIRA_STATUS_TEST_TOKEN"
"#,
    )
    .expect("config should be written");

    let db = Database::open(&db_path).expect("db should open");
    db.run_migrations().expect("migrations should run");
    db.upsert_toggl_entry(&NewTogglEntry {
        toggl_workspace_id: "123",
        toggl_entry_id: "deleted-entry",
        description: Some("CORE-224 deleted work"),
        extracted_issue_key: Some("CORE-224"),
        source_hash: "sha256:deleted",
        rounded_duration_seconds: 0,
        status: "planned",
        started_at: Some("2026-05-13T13:10:00Z"),
        stopped_at: Some("2026-05-13T13:10:00Z"),
    })
    .expect("toggl entry should insert");
    db.mark_toggl_entry_deleted("123", "deleted-entry")
        .expect("toggl entry should be marked deleted");
    drop(db);

    let output = Command::new(binary())
        .args([
            "status",
            "--config",
            config_path.to_str().expect("config path should be utf8"),
            "--db",
            db_path.to_str().expect("db path should be utf8"),
            "--json",
        ])
        .output()
        .expect("status should run");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("deleted-entry"), "{stdout}");
    assert!(!stdout.contains("CORE-224"), "{stdout}");
}

#[test]
fn status_human_output_formats_time_and_duration_readably() {
    let temp = TempDir::new().expect("temp dir should be created");
    let db_path = temp.path().join("ledger.sqlite");
    let config_path = temp.path().join("config.toml");

    fs::write(
        &config_path,
        format!(
            r#"
[toggl]
workspace_id = 123
api_token_env = "TOGGL_STATUS_TEST_TOKEN"

[runtime]
sqlite_path = "{}"

[[jira.sites]]
key = "sabservis"
base_url = "https://example.atlassian.net"
email_env = "JIRA_STATUS_TEST_EMAIL"
api_token_env = "JIRA_STATUS_TEST_TOKEN"
"#,
            db_path.display()
        ),
    )
    .expect("config should be written");

    let db = Database::open(&db_path).expect("db should open");
    db.run_migrations().expect("migrations should run");
    db.upsert_toggl_entry(&NewTogglEntry {
        toggl_workspace_id: "123",
        toggl_entry_id: "456",
        description: Some("SAB-456 implementation"),
        extracted_issue_key: Some("SAB-456"),
        source_hash: "sha256:status",
        rounded_duration_seconds: 5400,
        status: "created",
        started_at: Some("2024-05-02T03:06:40Z"),
        stopped_at: Some("2024-05-02T04:36:40Z"),
    })
    .expect("toggl entry should insert");
    drop(db);

    let output = Command::new(binary())
        .args([
            "status",
            "--config",
            config_path.to_str().expect("config path should be utf8"),
        ])
        .output()
        .expect("status command should run");

    assert!(
        output.status.success(),
        "stderr was: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("date        start  end    duration"),
        "{stdout}"
    );
    let start = DateTime::parse_from_rfc3339("2024-05-02T03:06:40Z")
        .unwrap()
        .with_timezone(&Local);
    let end = DateTime::parse_from_rfc3339("2024-05-02T04:36:40Z")
        .unwrap()
        .with_timezone(&Local);
    let expected = format!(
        "{}  {}  {}    1h 30m  SAB-456",
        start.format("%Y-%m-%d"),
        start.format("%H:%M"),
        end.format("%H:%M")
    );
    assert!(stdout.contains(&expected), "{stdout}");
}

#[test]
fn status_reports_running_entries_separately_from_missing_issue_key() {
    let temp = TempDir::new().expect("temp dir should be created");
    let db_path = temp.path().join("ledger.sqlite");
    let config_path = temp.path().join("config.toml");

    fs::write(
        &config_path,
        format!(
            r#"
[toggl]
workspace_id = 123
api_token_env = "TOGGL_STATUS_TEST_TOKEN"

[runtime]
sqlite_path = "{}"

[[jira.sites]]
key = "sabservis"
base_url = "https://example.atlassian.net"
email_env = "JIRA_STATUS_TEST_EMAIL"
api_token_env = "JIRA_STATUS_TEST_TOKEN"
"#,
            db_path.display()
        ),
    )
    .expect("config should be written");

    let db = Database::open(&db_path).expect("db should open");
    db.run_migrations().expect("migrations should run");
    db.upsert_toggl_entry(&NewTogglEntry {
        toggl_workspace_id: "123",
        toggl_entry_id: "running",
        description: None,
        extracted_issue_key: None,
        source_hash: "sha256:running",
        rounded_duration_seconds: 0,
        status: "planned",
        started_at: Some("2024-05-02T03:06:40Z"),
        stopped_at: None,
    })
    .expect("toggl entry should insert");
    drop(db);

    let output = Command::new(binary())
        .args([
            "status",
            "--config",
            config_path.to_str().expect("config path should be utf8"),
        ])
        .output()
        .expect("status command should run");

    assert!(
        output.status.success(),
        "stderr was: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("running entry"), "{stdout}");
    assert!(!stdout.contains("missing issue key"), "{stdout}");
}

#[test]
fn status_without_config_flag_uses_default_home_config() {
    let temp = TempDir::new().expect("temp dir should be created");
    let home_dir = temp.path();
    let config_path = default_config_path(home_dir);
    let db_path = temp.path().join("ledger.sqlite");
    fs::create_dir_all(config_path.parent().expect("config parent should exist"))
        .expect("config directory should be created");
    fs::write(
        &config_path,
        format!(
            r#"
[toggl]
workspace_id = 123
api_token_env = "TOGGL_STATUS_TEST_TOKEN"

[runtime]
sqlite_path = "{}"

[[jira.sites]]
key = "sabservis"
base_url = "https://example.atlassian.net"
email_env = "JIRA_STATUS_TEST_EMAIL"
api_token_env = "JIRA_STATUS_TEST_TOKEN"
"#,
            db_path.display()
        ),
    )
    .expect("config should be written");

    let db = Database::open(&db_path).expect("db should open");
    db.run_migrations().expect("migrations should run");
    drop(db);

    let output = Command::new(binary())
        .args(["status", "--json"])
        .env("HOME", home_dir)
        .env_remove("TOGGL_STATUS_TEST_TOKEN")
        .env_remove("JIRA_STATUS_TEST_EMAIL")
        .env_remove("JIRA_STATUS_TEST_TOKEN")
        .output()
        .expect("status command should run");

    assert!(
        output.status.success(),
        "stderr was: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    assert_eq!(json["summary"]["total_count"], 0);
}

#[test]
fn status_resolves_relative_runtime_sqlite_path_from_config_directory() {
    let temp = TempDir::new().expect("temp dir should be created");
    let home_dir = temp.path();
    let config_path = default_config_path(home_dir);
    let db_path = config_path
        .parent()
        .expect("config parent")
        .join("toggl-jira-sync.sqlite");
    fs::create_dir_all(config_path.parent().expect("config parent should exist"))
        .expect("config directory should be created");
    fs::write(
        &config_path,
        r#"
[toggl]
workspace_id = 123
api_token_env = "TOGGL_STATUS_TEST_TOKEN"

[runtime]
sqlite_path = "toggl-jira-sync.sqlite"

[[jira.sites]]
key = "sabservis"
base_url = "https://example.atlassian.net"
email_env = "JIRA_STATUS_TEST_EMAIL"
api_token_env = "JIRA_STATUS_TEST_TOKEN"
"#,
    )
    .expect("config should be written");

    let db = Database::open(&db_path).expect("db should open");
    db.run_migrations().expect("migrations should run");
    db.upsert_toggl_entry(&NewTogglEntry {
        toggl_workspace_id: "123",
        toggl_entry_id: "456",
        description: Some("SAB-456 implementation"),
        extracted_issue_key: Some("SAB-456"),
        source_hash: "sha256:status",
        rounded_duration_seconds: 1800,
        status: "created",
        started_at: Some("2024-05-02T03:06:40Z"),
        stopped_at: Some("2024-05-02T03:36:40Z"),
    })
    .expect("toggl entry should insert");
    drop(db);

    let output = Command::new(binary())
        .args(["status", "--json"])
        .env("HOME", home_dir)
        .current_dir(temp.path())
        .output()
        .expect("status command should run");

    assert!(
        output.status.success(),
        "stderr was: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be json");
    assert_eq!(json["summary"]["total_count"], 1);
    assert_eq!(json["entries"][0]["entry"], "456");
}

#[test]
fn status_reports_lock_error_when_another_process_holds_turso_db() {
    let temp = TempDir::new().expect("temp dir should be created");
    let db_path = temp.path().join("ledger.sqlite");
    let config_path = temp.path().join("config.toml");

    fs::write(
        &config_path,
        r#"
[toggl]
workspace_id = 123
api_token_env = "TOGGL_STATUS_TEST_TOKEN"

[[jira.sites]]
key = "sabservis"
base_url = "https://example.atlassian.net"
email_env = "JIRA_STATUS_TEST_EMAIL"
api_token_env = "JIRA_STATUS_TEST_TOKEN"
"#,
    )
    .expect("config should be written");

    let db = Database::open(&db_path).expect("db should open");
    db.run_migrations().expect("migrations should run");

    let output = Command::new(binary())
        .args([
            "status",
            "--config",
            config_path.to_str().expect("config path should be utf8"),
            "--db",
            db_path.to_str().expect("db path should be utf8"),
            "--json",
        ])
        .output()
        .expect("status command should run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to open local DB"), "{stderr}");
    assert!(stderr.contains("Locking error"), "{stderr}");
}
