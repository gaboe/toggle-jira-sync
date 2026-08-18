use std::{fs, process::Command};

fn binary() -> &'static str {
    Box::leak(
        std::env::var("CARGO_BIN_EXE_toggl-jira-sync")
            .expect("Cargo should provide the test binary path")
            .into_boxed_str(),
    )
}

fn write_config(path: &std::path::Path) {
    write_config_with_schedule(path, true);
}

fn write_config_with_schedule(path: &std::path::Path, schedule_enabled: bool) {
    fs::write(
        path,
        format!(
            r#"
[toggl]
workspace_id = 123456
api_token_env = "TOGGL_API_TOKEN"

[runtime]
sqlite_path = "toggl-jira-sync.sqlite"

[schedule]
enabled = {schedule_enabled}
interval_minutes = 60

[jira]

[[jira.sites]]
key = "sabservis"
base_url = "https://sabservis.atlassian.net"
email_env = "SABSERVIS_JIRA_EMAIL"
api_token_env = "SABSERVIS_JIRA_API_TOKEN"
enabled = true
"#
        ),
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

#[test]
fn startup_reinstalls_missing_job_for_default_config() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config_dir = temp.path().join(".config/toggl-jira-sync");
    fs::create_dir_all(&config_dir).expect("config dir");
    write_config(&config_dir.join("config.toml"));

    let output = Command::new(binary())
        .args(["schedule", "status"])
        .env("HOME", temp.path())
        .env("APPDATA", config_dir.parent().expect("appdata parent"))
        .env("TOGGL_JIRA_SYNC_SKIP_SCHEDULER_LOAD", "1")
        .output()
        .expect("schedule status should run");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("job installed: true"), "{stdout}");
}

#[test]
fn startup_removes_existing_job_when_schedule_is_disabled() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config_dir = temp.path().join(".config/toggl-jira-sync");
    fs::create_dir_all(&config_dir).expect("config dir");
    let config_path = config_dir.join("config.toml");
    write_config(&config_path);

    let install = Command::new(binary())
        .args(["schedule", "install"])
        .env("HOME", temp.path())
        .env("APPDATA", config_dir.parent().expect("appdata parent"))
        .env("TOGGL_JIRA_SYNC_SKIP_SCHEDULER_LOAD", "1")
        .output()
        .expect("schedule install should run");
    assert!(install.status.success());

    write_config_with_schedule(&config_path, false);

    let status = Command::new(binary())
        .args(["schedule", "status"])
        .env("HOME", temp.path())
        .env("APPDATA", config_dir.parent().expect("appdata parent"))
        .env("TOGGL_JIRA_SYNC_SKIP_SCHEDULER_LOAD", "1")
        .output()
        .expect("schedule status should run");
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("job installed: false"));
}

#[cfg(target_os = "linux")]
#[test]
fn schedule_status_requires_linux_timer_and_service() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = temp.path().join("config.toml");
    write_config(&config);
    let job_dir = temp.path().join(".config/systemd/user");
    fs::create_dir_all(&job_dir).expect("job dir");
    fs::write(job_dir.join("toggl-jira-sync.timer"), "timer").expect("timer");

    let status = Command::new(binary())
        .args([
            "schedule",
            "--config",
            config.to_str().expect("config path utf8"),
            "status",
        ])
        .env("HOME", temp.path())
        .output()
        .expect("schedule status should run");
    assert!(status.status.success());
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("job installed: false"),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );

    fs::write(job_dir.join("toggl-jira-sync.service"), "service").expect("service");
    let status = Command::new(binary())
        .args([
            "schedule",
            "--config",
            config.to_str().expect("config path utf8"),
            "status",
        ])
        .env("HOME", temp.path())
        .output()
        .expect("schedule status should run");
    assert!(status.status.success());
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("job installed: true"),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn startup_reconciles_existing_job_without_reinstall_message() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config_dir = temp.path().join(".config/toggl-jira-sync");
    fs::create_dir_all(&config_dir).expect("config dir");
    write_config(&config_dir.join("config.toml"));

    let install = Command::new(binary())
        .args(["schedule", "install"])
        .env("HOME", temp.path())
        .env("APPDATA", config_dir.parent().expect("appdata parent"))
        .env("TOGGL_JIRA_SYNC_SKIP_SCHEDULER_LOAD", "1")
        .output()
        .expect("schedule install should run");
    assert!(install.status.success());

    let status = Command::new(binary())
        .args(["schedule", "status"])
        .env("HOME", temp.path())
        .env("APPDATA", config_dir.parent().expect("appdata parent"))
        .env("TOGGL_JIRA_SYNC_SKIP_SCHEDULER_LOAD", "1")
        .output()
        .expect("schedule status should run");
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("job installed: true"));
    assert!(!String::from_utf8_lossy(&status.stderr).contains("reinstalled"));
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
