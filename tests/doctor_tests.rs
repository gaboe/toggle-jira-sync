use std::{fs, process::Command};

fn binary() -> &'static str {
    Box::leak(
        std::env::var("CARGO_BIN_EXE_toggl-jira-sync")
            .expect("Cargo should provide the test binary path")
            .into_boxed_str(),
    )
}

#[test]
fn doctor_redacts_secret_values() {
    let db = tempfile::NamedTempFile::new().expect("temp sqlite file");
    let output = Command::new(binary())
        .env("TOGGL_API_TOKEN", "toggl-secret-token")
        .env("SABSERVIS_JIRA_EMAIL", "sab@example.test")
        .env("SABSERVIS_JIRA_API_TOKEN", "sab-secret-token")
        .env("BLOGIC_JIRA_EMAIL", "blogic@example.test")
        .env("BLOGIC_JIRA_API_TOKEN", "blogic-secret-token")
        .args([
            "doctor",
            "--config",
            "tests/fixtures/multisite.toml",
            "--db",
        ])
        .arg(db.path())
        .output()
        .expect("failed to run doctor");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "config: ok",
        "database: ok",
        "migrations: ok",
        "TOGGL_API_TOKEN: set",
        "SABSERVIS_JIRA_EMAIL: set",
        "SABSERVIS_JIRA_API_TOKEN: set",
        "BLOGIC_JIRA_EMAIL: set",
        "BLOGIC_JIRA_API_TOKEN: set",
        "https://sabservis.atlassian.net: ok",
        "https://blogic.atlassian.net: ok",
    ] {
        assert!(stdout.contains(expected), "stdout was: {stdout}");
    }

    for secret in [
        "toggl-secret-token",
        "sab@example.test",
        "sab-secret-token",
        "blogic@example.test",
        "blogic-secret-token",
    ] {
        assert!(
            !stdout.contains(secret),
            "stdout exposed {secret}: {stdout}"
        );
    }
}

#[test]
fn doctor_reports_missing_credential_env_vars_without_secret_values() {
    let db = tempfile::NamedTempFile::new().expect("temp sqlite file");
    let output = Command::new(binary())
        .env_remove("TOGGL_API_TOKEN")
        .env_remove("SABSERVIS_JIRA_EMAIL")
        .env_remove("SABSERVIS_JIRA_API_TOKEN")
        .env_remove("BLOGIC_JIRA_EMAIL")
        .env_remove("BLOGIC_JIRA_API_TOKEN")
        .args([
            "doctor",
            "--config",
            "tests/fixtures/multisite.toml",
            "--db",
        ])
        .arg(db.path())
        .output()
        .expect("failed to run doctor");

    assert!(
        !output.status.success(),
        "doctor should fail when credentials are missing"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("TOGGL_API_TOKEN: missing"),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains("SABSERVIS_JIRA_API_TOKEN: missing"),
        "stdout was: {stdout}"
    );
    assert!(!stdout.contains("secret-token"), "stdout was: {stdout}");
}

#[test]
fn docs_include_cron_and_launchd_examples() {
    let docs = fs::read_to_string("docs/scheduling.md").expect("scheduling docs should exist");

    for expected in [
        "cron",
        "launchd",
        "sync --dry-run",
        "*/15 * * * *",
        "StartInterval",
        "900",
        "--config /Users/you/.config/toggl-jira-sync/config.toml",
        "--db /Users/you/Library/Application Support/toggl-jira-sync/state.sqlite",
    ] {
        assert!(docs.contains(expected), "docs missing {expected}: {docs}");
    }
}
