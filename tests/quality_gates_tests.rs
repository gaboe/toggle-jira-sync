use std::fs;

use toggl_jira_sync::config::AppConfig;

#[test]
fn gitignore_excludes_local_databases_secrets_and_evidence() {
    let gitignore = fs::read_to_string(".gitignore").expect(".gitignore should exist");

    for pattern in ["*.sqlite", "*.sqlite3", ".env", ".sisyphus/evidence/"] {
        assert!(
            gitignore.lines().any(|line| line.trim() == pattern),
            ".gitignore missing {pattern}: {gitignore}"
        );
    }
}

#[test]
fn config_example_parses_and_uses_env_var_placeholders_only() {
    let contents =
        fs::read_to_string("config.example.toml").expect("config.example.toml should exist");
    let config = AppConfig::from_toml_str(&contents).expect("config.example.toml should parse");

    assert_eq!(config.toggl.api_token_env, "TOGGL_API_TOKEN");
    assert_eq!(config.runtime.initial_backfill_days, 90);
    assert_eq!(config.runtime.recovery_scan_days, 180);
    assert_eq!(config.rate_limits.toggl_max_rps, 1.0);
    assert_eq!(config.rate_limits.jira_global_write_delay_ms, 150);
    assert_eq!(config.rate_limits.jira_same_issue_write_delay_ms, 2000);
    assert_eq!(config.rate_limits.jira_max_parallel_groups, 4);
    assert_eq!(config.enabled_jira_sites().len(), 2);

    for forbidden_literal in ["@", "secret", "password"] {
        assert!(
            !contents.contains(forbidden_literal),
            "config.example.toml should not contain {forbidden_literal}: {contents}"
        );
    }
}

#[test]
fn ci_workflow_runs_fmt_clippy_and_tests_without_credentials() {
    let workflow =
        fs::read_to_string(".github/workflows/ci.yml").expect("ci workflow should exist");

    for expected in [
        "cargo fmt --check",
        "cargo clippy --all-targets --all-features -- -D warnings",
        "cargo test --all-features",
    ] {
        assert!(
            workflow.contains(expected),
            "workflow missing `{expected}`: {workflow}"
        );
    }

    for forbidden in [
        "TOGGL_API_TOKEN:",
        "SABSERVIS_JIRA_API_TOKEN:",
        "BLOGIC_JIRA_API_TOKEN:",
        "secrets.",
    ] {
        assert!(
            !workflow.contains(forbidden),
            "workflow should not require credentials via `{forbidden}`: {workflow}"
        );
    }
}
