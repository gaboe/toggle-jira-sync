use std::{fs, process::Command};

fn binary() -> &'static str {
    Box::leak(
        std::env::var("CARGO_BIN_EXE_toggl-jira-sync")
            .expect("Cargo should provide the test binary path")
            .into_boxed_str(),
    )
}

fn write_config(path: &std::path::Path) {
    fs::write(
        path,
        r#"
[toggl]
workspace_id = 123456
api_token_env = "TOGGL_API_TOKEN"

[runtime]
sqlite_path = "toggl-jira-sync.sqlite"

[schedule]
enabled = true
interval_minutes = 60

[jira]

[[jira.sites]]
key = "sabservis"
base_url = "https://sabservis.atlassian.net"
email_env = "SABSERVIS_JIRA_EMAIL"
api_token_env = "SABSERVIS_JIRA_API_TOKEN"
enabled = true
"#,
    )
    .expect("write config");
}

#[test]
fn schedule_status_reports_config_and_job_path() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = temp.path().join("config.toml");
    write_config(&config);

    let output = Command::new(binary())
        .args([
            "schedule",
            "--config",
            config.to_str().expect("config path utf8"),
            "status",
        ])
        .env("HOME", temp.path())
        .output()
        .expect("schedule status should run");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("schedule enabled: true"), "{stdout}");
    assert!(stdout.contains("interval minutes: 60"), "{stdout}");
    assert!(stdout.contains("job installed: false"), "{stdout}");
}

#[test]
fn schedule_set_updates_config() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = temp.path().join("config.toml");
    write_config(&config);

    let output = Command::new(binary())
        .args([
            "schedule",
            "--config",
            config.to_str().expect("config path utf8"),
            "set",
            "--interval-minutes",
            "30",
            "--disabled",
        ])
        .env("HOME", temp.path())
        .output()
        .expect("schedule set should run");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config_text = fs::read_to_string(&config).expect("config should be readable");
    assert!(
        config_text.contains("interval_minutes = 30"),
        "{config_text}"
    );
    assert!(config_text.contains("enabled = false"), "{config_text}");
}

#[test]
fn schedule_install_writes_os_job_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = temp.path().join("config.toml");
    write_config(&config);

    let output = Command::new(binary())
        .args([
            "schedule",
            "--config",
            config.to_str().expect("config path utf8"),
            "install",
        ])
        .env("HOME", temp.path())
        .env("APPDATA", temp.path())
        .env("TOGGL_JIRA_SYNC_SKIP_SCHEDULER_LOAD", "1")
        .output()
        .expect("schedule install should run");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let files = walk_files(temp.path());
    assert!(
        files
            .iter()
            .any(|path| path.contains("toggl-jira-sync") || path.contains("com.toggl-jira-sync")),
        "expected scheduler file in {files:?}"
    );
    for file in files {
        let contents = fs::read_to_string(&file).expect("scheduler file should be readable");
        if contents.contains(" sync ") {
            assert!(contents.contains("--cleanup-deleted"), "{contents}");
        }
    }
}

fn walk_files(path: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    for entry in fs::read_dir(path).expect("read dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_files(&path));
        } else {
            files.push(path.display().to_string());
        }
    }
    files
}
