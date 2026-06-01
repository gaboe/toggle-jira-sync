use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

use toggl_jira_sync::config::{AppConfig, ConfigError};

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

fn default_credentials_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".config/toggl-jira-sync/credentials.env")
}

#[test]
fn config_valid_multisite_fixture_applies_typed_defaults() {
    let config =
        AppConfig::from_path("tests/fixtures/multisite.toml").expect("config should parse");

    assert_eq!(config.toggl.workspace_id, 123456);
    assert_eq!(config.toggl.api_token_env, "TOGGL_API_TOKEN");
    assert_eq!(config.runtime.initial_backfill_days, 90);
    assert_eq!(config.runtime.recovery_scan_days, 180);
    assert_eq!(config.rate_limits.toggl_max_rps, 1.0);
    assert_eq!(config.rate_limits.jira_global_write_delay_ms, 150);
    assert_eq!(config.rate_limits.jira_same_issue_write_delay_ms, 2000);
    assert_eq!(config.rate_limits.jira_max_parallel_groups, 4);
    assert_eq!(config.enabled_jira_sites().len(), 2);
}

#[test]
fn config_missing_env_var_names_are_rejected() {
    let temp = tempfile::NamedTempFile::new().expect("temp file");
    fs::write(
        temp.path(),
        r#"
[toggl]
workspace_id = 123456
api_token_env = ""

[jira]

[[jira.sites]]
key = "sabservis"
base_url = "https://sabservis.atlassian.net"
email_env = ""
api_token_env = "SABSERVIS_JIRA_API_TOKEN"
enabled = true
"#,
    )
    .expect("write config");

    let error = AppConfig::from_path(temp.path()).unwrap_err();

    assert!(
        matches!(&error, ConfigError::Validation(message) if message.contains("toggl.api_token_env must be set") && message.contains("jira.sites[sabservis].email_env must be set")),
        "unexpected error: {error}"
    );
}

#[test]
fn config_literal_secret_values_in_env_fields_are_rejected() {
    let temp = tempfile::NamedTempFile::new().expect("temp file");
    fs::write(
        temp.path(),
        r#"
[toggl]
workspace_id = 123456
api_token_env = "token with spaces"

[jira]

[[jira.sites]]
key = "sabservis"
base_url = "https://sabservis.atlassian.net"
email_env = "user@example.com"
api_token_env = "SABSERVIS_JIRA_API_TOKEN"
enabled = true
"#,
    )
    .expect("write config");

    let error = AppConfig::from_path(temp.path()).unwrap_err();

    assert!(
        matches!(&error, ConfigError::Validation(message) if message.contains("toggl.api_token_env must be an environment variable name") && message.contains("jira.sites[sabservis].email_env must be an environment variable name")),
        "unexpected error: {error}"
    );
}

#[test]
fn config_non_https_jira_base_urls_are_rejected() {
    let temp = tempfile::NamedTempFile::new().expect("temp file");
    fs::write(
        temp.path(),
        r#"
[toggl]
workspace_id = 123456
api_token_env = "TOGGL_API_TOKEN"

[jira]

[[jira.sites]]
key = "sabservis"
base_url = "http://sabservis.atlassian.net"
email_env = "SABSERVIS_JIRA_EMAIL"
api_token_env = "SABSERVIS_JIRA_API_TOKEN"
enabled = true
"#,
    )
    .expect("write config");

    let error = AppConfig::from_path(temp.path()).unwrap_err();

    assert!(
        matches!(&error, ConfigError::Validation(message) if message.contains("jira.sites[sabservis].base_url must start with https://")),
        "unexpected error: {error}"
    );
}

#[test]
fn config_duplicate_enabled_site_keys_are_rejected() {
    let temp = tempfile::NamedTempFile::new().expect("temp file");
    fs::write(
        temp.path(),
        r#"
[toggl]
workspace_id = 123456
api_token_env = "TOGGL_API_TOKEN"

[jira]

[[jira.sites]]
key = "sabservis"
base_url = "https://sabservis.atlassian.net"
email_env = "SABSERVIS_JIRA_EMAIL"
api_token_env = "SABSERVIS_JIRA_API_TOKEN"
enabled = true

[[jira.sites]]
key = "sabservis"
base_url = "https://other-sab.atlassian.net"
email_env = "OTHER_SAB_JIRA_EMAIL"
api_token_env = "OTHER_SAB_JIRA_API_TOKEN"
enabled = true
"#,
    )
    .expect("write config");

    let error = AppConfig::from_path(temp.path()).unwrap_err();

    assert!(
        matches!(&error, ConfigError::Validation(message) if message.contains("jira site key sabservis is configured for multiple enabled sites")),
        "unexpected error: {error}"
    );
}

#[test]
fn config_unknown_fields_are_rejected() {
    let temp = tempfile::NamedTempFile::new().expect("temp file");
    fs::write(
        temp.path(),
        r#"
[toggl]
workspace_id = 123456
api_token_env = "TOGGL_API_TOKEN"
literal_api_token = "do-not-accept"

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

    let error = AppConfig::from_path(temp.path()).unwrap_err();

    assert!(
        matches!(&error, ConfigError::Parse(message) if message.contains("unknown field") && message.contains("literal_api_token")),
        "unexpected error: {error}"
    );
}

#[test]
fn config_validate_cli_accepts_valid_multisite_fixture() {
    let output = Command::new(binary())
        .args([
            "config",
            "validate",
            "--config",
            "tests/fixtures/multisite.toml",
        ])
        .output()
        .expect("failed to run config validate");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("2 Jira sites enabled"),
        "stdout was: {stdout}"
    );
}

#[test]
fn config_validate_cli_accepts_duplicate_prefix_fixture() {
    let output = Command::new(binary())
        .args([
            "config",
            "validate",
            "--config",
            "tests/fixtures/multisite.toml",
        ])
        .output()
        .expect("failed to run config validate");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn config_setup_accepts_stdin_and_writes_config_and_credentials() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config_path = temp_dir.path().join("config.toml");
    let credentials_path = temp_dir.path().join("local.credentials.env");

    let mut child = Command::new(binary())
        .args([
            "config",
            "setup",
            "--config",
            config_path.to_str().expect("utf-8 config path"),
            "--credentials",
            credentials_path.to_str().expect("utf-8 credentials path"),
        ])
        .env("TJS_SKIP_SCHEDULE_INSTALL", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run config setup");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"fake-toggl-token\n123456\nhttps://sabservis.atlassian.net\nuser@example.com\nfake-jira-token\n")
        .expect("write stdin");

    let output = child.wait_with_output().expect("setup output");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config_text = fs::read_to_string(&config_path).expect("config file");
    assert!(
        config_text.contains("workspace_id = 123456"),
        "{config_text}"
    );
    assert!(
        config_text.contains("api_token_env = \"TOGGL_API_TOKEN\""),
        "{config_text}"
    );
    assert!(
        config_text.contains("email_env = \"SABSERVIS_JIRA_EMAIL\""),
        "{config_text}"
    );
    assert!(config_text.contains("key = \"sabservis\""), "{config_text}");
    assert!(
        config_text.contains("base_url = \"https://sabservis.atlassian.net\""),
        "{config_text}"
    );
    assert!(
        config_text.contains("api_token_env = \"SABSERVIS_JIRA_API_TOKEN\""),
        "{config_text}"
    );
    assert!(!config_text.contains("issue_key_prefixes"), "{config_text}");
    assert!(
        config_text.contains("sqlite_path = \"toggl-jira-sync.sqlite\""),
        "{config_text}"
    );
    assert!(config_text.contains("[schedule]"), "{config_text}");
    assert!(config_text.contains("enabled = true"), "{config_text}");
    assert!(
        config_text.contains("interval_minutes = 60"),
        "{config_text}"
    );
    AppConfig::from_path(&config_path).expect("generated config should validate");

    let credentials_text = fs::read_to_string(&credentials_path).expect("credentials file");
    assert!(
        credentials_text.contains("TOGGL_API_TOKEN=fake-toggl-token"),
        "{credentials_text}"
    );
    assert!(
        credentials_text.contains("SABSERVIS_JIRA_EMAIL=user@example.com"),
        "{credentials_text}"
    );
    assert!(
        credentials_text.contains("SABSERVIS_JIRA_API_TOKEN=fake-jira-token"),
        "{credentials_text}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = fs::metadata(&credentials_path)
            .expect("credentials metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "credentials file must be user-only readable");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Toggl API token:"), "{stdout}");
    assert!(
        stdout.contains("Toggl workspace id (only needed if workspace discovery is skipped):"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("Toggl workspace id:"),
        "raw workspace prompt should not be shown: {stdout}"
    );
    assert!(stdout.contains("Jira site URL:"), "{stdout}");
    assert!(
        stdout.contains("Using Jira site key: sabservis"),
        "{stdout}"
    );
    assert!(!stdout.contains("Site key:"), "{stdout}");
    assert!(!stdout.contains("Jira base URL:"), "{stdout}");
    assert!(!stdout.contains("Issue key prefixes"), "{stdout}");
    assert!(!stdout.contains("SQLite path:"), "{stdout}");
}

#[test]
fn config_setup_accepts_multiple_jira_sites_from_stdin() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config_path = temp_dir.path().join("config.toml");
    let credentials_path = temp_dir.path().join("local.credentials.env");

    let mut child = Command::new(binary())
        .args([
            "config",
            "setup",
            "--config",
            config_path.to_str().expect("utf-8 config path"),
            "--credentials",
            credentials_path.to_str().expect("utf-8 credentials path"),
        ])
        .env("TJS_SKIP_SCHEDULE_INSTALL", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run config setup");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"fake-toggl-token\n123456\nhttps://sabservis.atlassian.net\nuser@example.com\nfake-jira-token\ny\nhttps://blogic.atlassian.net\nblogic@example.com\nfake-blogic-token\nn\n")
        .expect("write stdin");

    let output = child.wait_with_output().expect("setup output");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config = AppConfig::from_path(&config_path).expect("generated config should validate");
    assert_eq!(config.enabled_jira_sites().len(), 2);

    let config_text = fs::read_to_string(&config_path).expect("config file");
    assert!(config_text.contains("key = \"sabservis\""), "{config_text}");
    assert!(config_text.contains("key = \"blogic\""), "{config_text}");

    let credentials_text = fs::read_to_string(&credentials_path).expect("credentials file");
    assert!(
        credentials_text.contains("SABSERVIS_JIRA_API_TOKEN=fake-jira-token"),
        "{credentials_text}"
    );
    assert!(
        credentials_text.contains("BLOGIC_JIRA_API_TOKEN=fake-blogic-token"),
        "{credentials_text}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Add another Jira site? [y/N]:"), "{stdout}");
}

#[test]
fn config_setup_without_paths_uses_default_home_based_locations() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let home_dir = temp_dir.path();
    let config_path = default_config_path(home_dir);
    let credentials_path = default_credentials_path(home_dir);

    let mut child = Command::new(binary())
        .args(["config", "setup"])
        .env("HOME", home_dir)
        .env("TJS_SKIP_SCHEDULE_INSTALL", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run config setup");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"fake-toggl-token\n123456\nhttps://sabservis.atlassian.net\nuser@example.com\nfake-jira-token\n")
        .expect("write stdin");

    let output = child.wait_with_output().expect("setup output");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("config saved: {}", config_path.display())),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "credentials saved: {}",
            credentials_path.display()
        )),
        "{stdout}"
    );

    let config_text = fs::read_to_string(&config_path).expect("config file");
    assert!(
        config_text.contains("sqlite_path = \"toggl-jira-sync.sqlite\""),
        "{config_text}"
    );
    assert!(
        config_text.contains("interval_minutes = 60"),
        "{config_text}"
    );

    let credentials_text = fs::read_to_string(&credentials_path).expect("credentials file");
    assert!(
        credentials_text.contains("TOGGL_API_TOKEN=fake-toggl-token"),
        "{credentials_text}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = fs::metadata(&credentials_path)
            .expect("credentials metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "credentials file must be user-only readable");
    }
}

#[test]
fn config_show_redacts_credentials_by_default_and_reveals_with_flag() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config_path = temp_dir.path().join("config.toml");
    let credentials_path = temp_dir.path().join("credentials.env");
    fs::write(
        &config_path,
        r#"
[toggl]
workspace_id = 123456
api_token_env = "TOGGL_API_TOKEN"

[runtime]
sqlite_path = "/tmp/tjs.sqlite"

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
    fs::write(
        &credentials_path,
        "TOGGL_API_TOKEN=fake-toggl-token\nSABSERVIS_JIRA_EMAIL=user@example.com\nSABSERVIS_JIRA_API_TOKEN=fake-jira-token\n",
    )
    .expect("write credentials");

    let output = Command::new(binary())
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().expect("utf-8 config path"),
            "--credentials",
            credentials_path.to_str().expect("utf-8 credentials path"),
        ])
        .output()
        .expect("failed to run config show");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("site: sabservis"), "{stdout}");
    assert!(
        stdout.contains("base_url: https://sabservis.atlassian.net"),
        "{stdout}"
    );
    assert!(!stdout.contains("issue_key_prefixes"), "{stdout}");
    assert!(stdout.contains("sqlite_path: /tmp/tjs.sqlite"), "{stdout}");
    assert!(
        stdout.contains("TOGGL_API_TOKEN: present (<redacted>)"),
        "{stdout}"
    );
    assert!(!stdout.contains("fake-toggl-token"), "{stdout}");
    assert!(!stdout.contains("fake-jira-token"), "{stdout}");

    let output = Command::new(binary())
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().expect("utf-8 config path"),
            "--credentials",
            credentials_path.to_str().expect("utf-8 credentials path"),
            "--show-secrets",
        ])
        .output()
        .expect("failed to run config show --show-secrets");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("TOGGL_API_TOKEN: fake-toggl-token"),
        "{stdout}"
    );
    assert!(
        stdout.contains("SABSERVIS_JIRA_API_TOKEN: fake-jira-token"),
        "{stdout}"
    );
}

#[test]
fn config_show_without_paths_uses_default_home_based_locations() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let home_dir = temp_dir.path();
    let config_path = default_config_path(home_dir);
    let credentials_path = default_credentials_path(home_dir);
    fs::create_dir_all(
        config_path
            .parent()
            .expect("default config path should have a parent"),
    )
    .expect("create config directory");
    fs::write(
        &config_path,
        r#"
[toggl]
workspace_id = 123456
api_token_env = "TOGGL_API_TOKEN"

[runtime]
sqlite_path = "/tmp/tjs.sqlite"

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
    fs::write(
        &credentials_path,
        "TOGGL_API_TOKEN=fake-toggl-token\nSABSERVIS_JIRA_EMAIL=user@example.com\nSABSERVIS_JIRA_API_TOKEN=fake-jira-token\n",
    )
    .expect("write credentials");

    let output = Command::new(binary())
        .args(["config", "show"])
        .env("HOME", home_dir)
        .output()
        .expect("failed to run config show");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("config: {}", config_path.display())),
        "{stdout}"
    );
    assert!(
        stdout.contains("TOGGL_API_TOKEN: present (<redacted>)"),
        "{stdout}"
    );
    assert!(!stdout.contains("fake-toggl-token"), "{stdout}");
    assert!(!stdout.contains("fake-jira-token"), "{stdout}");
}

#[test]
fn config_show_without_paths_reports_friendly_error_when_default_config_is_missing() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let home_dir = temp_dir.path();
    let config_path = default_config_path(home_dir);

    let output = Command::new(binary())
        .args(["config", "show"])
        .env("HOME", home_dir)
        .output()
        .expect("failed to run config show");

    assert!(
        !output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("Config not found: {}", config_path.display())),
        "{stderr}"
    );
    assert!(stderr.contains("Run: tjs config setup"), "{stderr}");
    assert!(!stderr.contains("Error:"), "{stderr}");
    assert!(!stderr.contains("Caused by:"), "{stderr}");
    assert!(!stderr.contains("Details:"), "{stderr}");
}

#[test]
fn config_validate_without_config_flag_reports_actionable_error_without_anyhow_noise() {
    let output = Command::new(binary())
        .args(["config", "validate"])
        .output()
        .expect("failed to run config validate");

    assert!(
        !output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--config is required for config validate"),
        "{stderr}"
    );
    assert!(!stderr.contains("Error:"), "{stderr}");
    assert!(!stderr.contains("Caused by:"), "{stderr}");
}

#[test]
fn config_validate_with_malformed_config_keeps_useful_parse_details_without_anyhow_noise() {
    let temp = tempfile::NamedTempFile::new().expect("temp file");
    fs::write(
        temp.path(),
        r#"
[toggl]
workspace_id = 123456
api_token_env = "TOGGL_API_TOKEN"
literal_api_token = "do-not-accept"

[jira]

[[jira.sites]]
key = "sabservis"
base_url = "https://sabservis.atlassian.net"
email_env = "SABSERVIS_JIRA_EMAIL"
api_token_env = "SABSERVIS_JIRA_API_TOKEN"
enabled = true
"#,
    )
    .expect("write malformed config");

    let output = Command::new(binary())
        .args([
            "config",
            "validate",
            "--config",
            temp.path().to_str().expect("utf-8 config path"),
        ])
        .output()
        .expect("failed to run config validate");

    assert!(
        !output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("config validation failed for"), "{stderr}");
    assert!(stderr.contains("failed to parse config"), "{stderr}");
    assert!(stderr.contains("unknown field"), "{stderr}");
    assert!(stderr.contains("literal_api_token"), "{stderr}");
    assert!(!stderr.contains("Error:"), "{stderr}");
    assert!(!stderr.contains("Caused by:"), "{stderr}");
}

#[test]
fn gitignore_ignores_local_credentials_files() {
    let gitignore = fs::read_to_string(".gitignore").expect(".gitignore");

    assert!(
        gitignore
            .lines()
            .any(|line| line.trim() == "credentials.env"),
        ".gitignore should ignore credentials.env"
    );
    assert!(
        gitignore
            .lines()
            .any(|line| line.trim() == "*.credentials.env"),
        ".gitignore should ignore *.credentials.env"
    );
}
