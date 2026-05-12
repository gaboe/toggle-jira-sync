use std::{collections::HashMap, env, fs, path::PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::{
    cli::{SharedPaths, SyncArgs},
    commands::{
        config::{resolve_config_path, resolve_db_path},
        schedule,
    },
    config::AppConfig,
    db::Database,
    report::StatusReport,
};

#[derive(Debug, Clone, Serialize)]
pub struct AppStateSnapshot {
    pub status: StatusReport,
    pub schedule: ScheduleSnapshot,
    pub config: ConfigSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigOnlySnapshot {
    pub schedule: ScheduleSnapshot,
    pub config: ConfigSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScheduleSnapshot {
    pub enabled: bool,
    pub interval_minutes: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigSnapshot {
    pub path: String,
    pub toggl_workspace_id: i64,
    pub toggl_api_token_env: String,
    pub toggl_api_token_present: bool,
    pub toggl_api_token_value: Option<String>,
    pub sqlite_path: String,
    pub initial_backfill_days: u32,
    pub recovery_scan_days: u32,
    pub schedule_enabled: bool,
    pub schedule_interval_minutes: u32,
    pub jira_sites: Vec<JiraSiteSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeleteLocalDataResult {
    pub deleted: bool,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct JiraSiteSnapshot {
    pub key: String,
    pub base_url: String,
    pub email_env: String,
    pub api_token_env: String,
    pub email_present: bool,
    pub email_value: Option<String>,
    pub api_token_present: bool,
    pub api_token_value: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigUpdate {
    pub toggl_workspace_id: i64,
    pub toggl_api_token_env: String,
    pub toggl_api_token_value: Option<String>,
    pub sqlite_path: String,
    pub initial_backfill_days: u32,
    pub recovery_scan_days: u32,
    pub schedule_enabled: bool,
    pub schedule_interval_minutes: u32,
    pub jira_sites: Vec<JiraSiteUpdate>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JiraSiteUpdate {
    pub key: String,
    pub base_url: String,
    pub email_env: String,
    pub api_token_env: String,
    pub email_value: Option<String>,
    pub api_token_value: Option<String>,
    pub enabled: bool,
}

pub fn snapshot(paths: SharedPaths, limit: usize) -> anyhow::Result<AppStateSnapshot> {
    let (config_path, config, status) = status_report(paths, limit)?;
    let credentials = read_default_credentials().unwrap_or_default();
    Ok(AppStateSnapshot {
        schedule: ScheduleSnapshot {
            enabled: config.schedule.enabled,
            interval_minutes: config.schedule.interval_minutes,
        },
        config: ConfigSnapshot::from_config(config_path, &config, &credentials),
        status,
    })
}

pub fn config_snapshot(paths: SharedPaths) -> anyhow::Result<ConfigOnlySnapshot> {
    let config_path = resolve_config_path(paths.config)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let credentials = read_default_credentials().unwrap_or_default();
    Ok(ConfigOnlySnapshot {
        schedule: ScheduleSnapshot {
            enabled: config.schedule.enabled,
            interval_minutes: config.schedule.interval_minutes,
        },
        config: ConfigSnapshot::from_config(config_path, &config, &credentials),
    })
}

pub fn status_report(
    paths: SharedPaths,
    limit: usize,
) -> anyhow::Result<(PathBuf, AppConfig, StatusReport)> {
    let config_path = resolve_config_path(paths.config)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let db_path = resolve_db_path(
        paths.db,
        &config_path,
        config.runtime.sqlite_path.as_deref(),
        "status",
    )?;
    let database = Database::open(&db_path)
        .with_context(|| format!("failed to open SQLite DB {}", db_path.display()))?;
    database
        .run_migrations()
        .context("failed to run DB migrations")?;
    let report = StatusReport::from_rows(
        database
            .list_status_entries(limit)
            .context("failed to load status rows")?,
    );
    Ok((config_path, config, report))
}

pub async fn run_sync(
    paths: SharedPaths,
    dry_run: bool,
    cleanup_deleted: bool,
) -> anyhow::Result<()> {
    crate::commands::sync::run(SyncArgs {
        paths,
        dry_run,
        cleanup_deleted,
        json: false,
        quiet: true,
    })
    .await
}

pub fn update_schedule(paths: SharedPaths, enabled: bool) -> anyhow::Result<ScheduleSnapshot> {
    let config_path = resolve_config_path(paths.config)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    schedule::update_schedule_config(&config_path, None, Some(enabled))?;
    if enabled {
        schedule::install_default_job(&config_path, config.schedule.interval_minutes)?;
    } else {
        schedule::uninstall_job()?;
    }
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to reload config {}", config_path.display()))?;
    Ok(ScheduleSnapshot {
        enabled: config.schedule.enabled,
        interval_minutes: config.schedule.interval_minutes,
    })
}

pub fn save_config(paths: SharedPaths, update: ConfigUpdate) -> anyhow::Result<ConfigSnapshot> {
    let config_path = resolve_config_path(paths.config)?;
    let contents = render_config_update(&update);
    let config = AppConfig::from_toml_str(&contents).context("updated config failed validation")?;
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&config_path, contents)
        .with_context(|| format!("failed to write config {}", config_path.display()))?;
    let credentials = save_credentials_update(&update)?;
    Ok(ConfigSnapshot::from_config(
        config_path,
        &config,
        &credentials,
    ))
}

pub fn delete_local_data(paths: SharedPaths) -> anyhow::Result<DeleteLocalDataResult> {
    let config_path = resolve_config_path(paths.config)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let db_path = resolve_db_path(
        paths.db,
        &config_path,
        config.runtime.sqlite_path.as_deref(),
        "delete local data",
    )?;
    let deleted = if db_path.exists() {
        fs::remove_file(&db_path)
            .with_context(|| format!("failed to delete SQLite DB {}", db_path.display()))?;
        true
    } else {
        false
    };
    Ok(DeleteLocalDataResult {
        deleted,
        path: db_path.display().to_string(),
    })
}

pub fn jira_base_urls(config: &AppConfig) -> HashMap<String, String> {
    config
        .enabled_jira_sites()
        .into_iter()
        .map(|site| {
            (
                site.key.clone(),
                site.base_url.trim_end_matches('/').to_owned(),
            )
        })
        .collect()
}

impl ConfigSnapshot {
    fn from_config(
        path: PathBuf,
        config: &AppConfig,
        credentials: &HashMap<String, String>,
    ) -> Self {
        Self {
            path: path.display().to_string(),
            toggl_workspace_id: config.toggl.workspace_id,
            toggl_api_token_env: config.toggl.api_token_env.clone(),
            toggl_api_token_present: credential_present(credentials, &config.toggl.api_token_env),
            toggl_api_token_value: credentials.get(&config.toggl.api_token_env).cloned(),
            sqlite_path: config
                .runtime
                .sqlite_path
                .clone()
                .unwrap_or_else(|| "toggl-jira-sync.sqlite".to_owned()),
            initial_backfill_days: config.runtime.initial_backfill_days,
            recovery_scan_days: config.runtime.recovery_scan_days,
            schedule_enabled: config.schedule.enabled,
            schedule_interval_minutes: config.schedule.interval_minutes,
            jira_sites: config
                .jira
                .sites
                .iter()
                .map(|site| JiraSiteSnapshot {
                    key: site.key.clone(),
                    base_url: site.base_url.clone(),
                    email_env: site.email_env.clone(),
                    api_token_env: site.api_token_env.clone(),
                    email_present: credential_present(credentials, &site.email_env),
                    email_value: credentials.get(&site.email_env).cloned(),
                    api_token_present: credential_present(credentials, &site.api_token_env),
                    api_token_value: credentials.get(&site.api_token_env).cloned(),
                    enabled: site.enabled,
                })
                .collect(),
        }
    }
}

fn credential_present(credentials: &HashMap<String, String>, name: &str) -> bool {
    env::var_os(name).is_some() || credentials.contains_key(name)
}

fn save_credentials_update(update: &ConfigUpdate) -> anyhow::Result<HashMap<String, String>> {
    let mut credentials = read_default_credentials().unwrap_or_default();
    let mut changed = false;
    changed |= upsert_secret(
        &mut credentials,
        &update.toggl_api_token_env,
        update.toggl_api_token_value.as_deref(),
    );
    for site in &update.jira_sites {
        changed |= upsert_secret(
            &mut credentials,
            &site.email_env,
            site.email_value.as_deref(),
        );
        changed |= upsert_secret(
            &mut credentials,
            &site.api_token_env,
            site.api_token_value.as_deref(),
        );
    }
    if changed {
        write_default_credentials(&credentials)?;
    }
    Ok(credentials)
}

fn upsert_secret(
    credentials: &mut HashMap<String, String>,
    name: &str,
    value: Option<&str>,
) -> bool {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    credentials.insert(name.to_owned(), value.to_owned());
    true
}

fn default_credentials_path() -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(appdata) = env::var_os("APPDATA") {
            return Ok(PathBuf::from(appdata).join("toggl-jira-sync/credentials.env"));
        }
    }
    let home = env::var_os("HOME").context("HOME must be set to resolve credentials path")?;
    Ok(PathBuf::from(home).join(".config/toggl-jira-sync/credentials.env"))
}

fn read_default_credentials() -> anyhow::Result<HashMap<String, String>> {
    let path = default_credentials_path()?;
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read credentials {}", path.display()))?;
    let mut credentials = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            credentials.insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }
    Ok(credentials)
}

fn write_default_credentials(credentials: &HashMap<String, String>) -> anyhow::Result<()> {
    let path = default_credentials_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut lines = credentials
        .iter()
        .map(|(key, value)| format!("{key}={}", value.replace('\n', "")))
        .collect::<Vec<_>>();
    lines.sort();
    fs::write(&path, format!("{}\n", lines.join("\n")))
        .with_context(|| format!("failed to write credentials {}", path.display()))?;
    Ok(())
}

fn render_config_update(update: &ConfigUpdate) -> String {
    let mut contents = format!(
        r#"[toggl]
workspace_id = {workspace_id}
api_token_env = "{toggl_api_token_env}"

[runtime]
sqlite_path = "{sqlite_path}"
initial_backfill_days = {initial_backfill_days}
recovery_scan_days = {recovery_scan_days}

[schedule]
enabled = {schedule_enabled}
interval_minutes = {schedule_interval_minutes}

[jira]
"#,
        workspace_id = update.toggl_workspace_id,
        toggl_api_token_env = escape_toml_string(&update.toggl_api_token_env),
        sqlite_path = escape_toml_string(&update.sqlite_path),
        initial_backfill_days = update.initial_backfill_days,
        recovery_scan_days = update.recovery_scan_days,
        schedule_enabled = update.schedule_enabled,
        schedule_interval_minutes = update.schedule_interval_minutes,
    );
    for site in &update.jira_sites {
        contents.push_str(&format!(
            r#"
[[jira.sites]]
key = "{key}"
base_url = "{base_url}"
email_env = "{email_env}"
api_token_env = "{api_token_env}"
enabled = {enabled}
"#,
            key = escape_toml_string(&site.key),
            base_url = escape_toml_string(&site.base_url),
            email_env = escape_toml_string(&site.email_env),
            api_token_env = escape_toml_string(&site.api_token_env),
            enabled = site.enabled,
        ));
    }
    contents
}

fn escape_toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_config_writes_valid_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        let saved = save_config(
            SharedPaths {
                config: Some(config_path.clone()),
                db: None,
            },
            ConfigUpdate {
                toggl_workspace_id: 123,
                toggl_api_token_env: "TOGGL_API_TOKEN".to_owned(),
                toggl_api_token_value: None,
                sqlite_path: "toggl-jira-sync.sqlite".to_owned(),
                initial_backfill_days: 90,
                recovery_scan_days: 180,
                schedule_enabled: true,
                schedule_interval_minutes: 60,
                jira_sites: vec![JiraSiteUpdate {
                    key: "acme".to_owned(),
                    base_url: "https://acme.atlassian.net".to_owned(),
                    email_env: "ACME_JIRA_EMAIL".to_owned(),
                    api_token_env: "ACME_JIRA_API_TOKEN".to_owned(),
                    email_value: None,
                    api_token_value: None,
                    enabled: true,
                }],
            },
        )
        .expect("save config");

        assert_eq!(saved.path, config_path.display().to_string());
        assert_eq!(saved.jira_sites[0].key, "acme");
        AppConfig::from_path(config_path).expect("saved config parses");
    }

    #[test]
    fn delete_local_data_removes_resolved_sqlite_file_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let db_path = dir.path().join("toggl-jira-sync.sqlite");

        save_config(
            SharedPaths {
                config: Some(config_path.clone()),
                db: None,
            },
            ConfigUpdate {
                toggl_workspace_id: 123,
                toggl_api_token_env: "TOGGL_API_TOKEN".to_owned(),
                toggl_api_token_value: None,
                sqlite_path: "toggl-jira-sync.sqlite".to_owned(),
                initial_backfill_days: 90,
                recovery_scan_days: 180,
                schedule_enabled: true,
                schedule_interval_minutes: 60,
                jira_sites: vec![JiraSiteUpdate {
                    key: "acme".to_owned(),
                    base_url: "https://acme.atlassian.net".to_owned(),
                    email_env: "ACME_JIRA_EMAIL".to_owned(),
                    api_token_env: "ACME_JIRA_API_TOKEN".to_owned(),
                    email_value: None,
                    api_token_value: None,
                    enabled: true,
                }],
            },
        )
        .expect("save config");
        fs::write(&db_path, "sqlite bytes").expect("write db");

        let result = delete_local_data(SharedPaths {
            config: Some(config_path.clone()),
            db: None,
        })
        .expect("delete local data");

        assert!(result.deleted);
        assert_eq!(result.path, db_path.display().to_string());
        assert!(!db_path.exists());
        assert!(config_path.exists());
    }
}
