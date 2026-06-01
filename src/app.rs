use std::{collections::HashMap, env, fs, path::PathBuf};

#[cfg(unix)]
use std::io::Write;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppStateSnapshot {
    pub status: StatusReport,
    pub schedule: ScheduleSnapshot,
    pub config: ConfigSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigOnlySnapshot {
    pub schedule: ScheduleSnapshot,
    pub config: ConfigSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleSnapshot {
    pub enabled: bool,
    pub interval_minutes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleCommandStatus {
    pub enabled: bool,
    pub interval_minutes: u32,
    pub job_path: String,
    pub job_installed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub path: String,
    pub toggl_workspace_id: i64,
    pub toggl_api_token_env: String,
    pub toggl_api_token_present: bool,
    pub toggl_api_token_value: Option<String>,
    pub sqlite_path: String,
    pub initial_backfill_from_month: Option<String>,
    pub initial_backfill_days: u32,
    pub recovery_from_month: Option<String>,
    pub recovery_scan_days: u32,
    pub schedule_enabled: bool,
    pub schedule_interval_minutes: u32,
    pub jira_sites: Vec<JiraSiteSnapshot>,
}

impl ConfigOnlySnapshot {
    pub fn redacted(mut self) -> Self {
        self.config.redact_secrets();
        self
    }
}

impl AppStateSnapshot {
    pub fn redacted(mut self) -> Self {
        self.config.redact_secrets();
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteLocalDataResult {
    pub deleted: bool,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportConfigResult {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogFileResult {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigUpdate {
    pub toggl_workspace_id: i64,
    pub toggl_api_token_env: String,
    pub toggl_api_token_value: Option<String>,
    pub sqlite_path: String,
    pub initial_backfill_from_month: Option<String>,
    pub initial_backfill_days: u32,
    pub recovery_from_month: Option<String>,
    pub recovery_scan_days: u32,
    pub schedule_enabled: bool,
    pub schedule_interval_minutes: u32,
    pub jira_sites: Vec<JiraSiteUpdate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    snapshot_with_credentials(paths, limit, None)
}

pub fn snapshot_with_credentials(
    paths: SharedPaths,
    limit: usize,
    credentials_path: Option<PathBuf>,
) -> anyhow::Result<AppStateSnapshot> {
    let (config_path, config, status) = status_report(paths, limit)?;
    let credentials = read_credentials_for_config(&config_path, credentials_path.clone())?;
    let allow_process_env = credentials_path.is_none() && config_path == resolve_config_path(None)?;
    Ok(AppStateSnapshot {
        schedule: ScheduleSnapshot {
            enabled: config.schedule.enabled,
            interval_minutes: config.schedule.interval_minutes,
        },
        config: ConfigSnapshot::from_config(config_path, &config, &credentials, allow_process_env),
        status,
    })
}

pub fn config_snapshot(paths: SharedPaths) -> anyhow::Result<ConfigOnlySnapshot> {
    config_snapshot_with_credentials(paths, None)
}

pub fn config_snapshot_with_credentials(
    paths: SharedPaths,
    credentials_path: Option<PathBuf>,
) -> anyhow::Result<ConfigOnlySnapshot> {
    let config_path = resolve_config_path(paths.config)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let credentials = read_credentials_for_config(&config_path, credentials_path.clone())?;
    let allow_process_env = credentials_path.is_none() && config_path == resolve_config_path(None)?;
    Ok(ConfigOnlySnapshot {
        schedule: ScheduleSnapshot {
            enabled: config.schedule.enabled,
            interval_minutes: config.schedule.interval_minutes,
        },
        config: ConfigSnapshot::from_config(config_path, &config, &credentials, allow_process_env),
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
    run_sync_with_credentials(paths, dry_run, cleanup_deleted, None).await
}

pub async fn run_sync_with_credentials(
    paths: SharedPaths,
    dry_run: bool,
    cleanup_deleted: bool,
    credentials_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let args = SyncArgs {
        paths,
        dry_run,
        cleanup_deleted,
        json: false,
        quiet: true,
    };
    if let Some(credentials_path) = credentials_path {
        return crate::commands::sync::run_with_credentials(args, credentials_path).await;
    }
    crate::commands::sync::run_direct(SyncArgs { ..args }).await
}

pub async fn run_sync_with_isolated_credentials(
    paths: SharedPaths,
    dry_run: bool,
    cleanup_deleted: bool,
    credentials_path: PathBuf,
) -> anyhow::Result<()> {
    crate::commands::sync::run_with_isolated_credentials(
        SyncArgs {
            paths,
            dry_run,
            cleanup_deleted,
            json: false,
            quiet: true,
        },
        credentials_path,
    )
    .await
}

pub fn schedule_status(paths: SharedPaths) -> anyhow::Result<ScheduleCommandStatus> {
    let config_path = resolve_config_path(paths.config)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let job_path = schedule::job_path()?;
    Ok(ScheduleCommandStatus {
        enabled: config.schedule.enabled,
        interval_minutes: config.schedule.interval_minutes,
        job_installed: job_path.exists(),
        job_path: job_path.display().to_string(),
    })
}

pub fn install_schedule(paths: SharedPaths) -> anyhow::Result<ScheduleSnapshot> {
    let config_path = resolve_config_path(paths.config)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    schedule::install_default_job(&config_path, config.schedule.interval_minutes)?;
    Ok(ScheduleSnapshot {
        enabled: config.schedule.enabled,
        interval_minutes: config.schedule.interval_minutes,
    })
}

pub fn uninstall_schedule() -> anyhow::Result<()> {
    schedule::uninstall_job()
}

pub fn update_schedule(paths: SharedPaths, enabled: bool) -> anyhow::Result<ScheduleSnapshot> {
    set_schedule(paths, None, Some(enabled))
}

pub fn set_schedule(
    paths: SharedPaths,
    interval_minutes: Option<u32>,
    enabled: Option<bool>,
) -> anyhow::Result<ScheduleSnapshot> {
    let config_path = resolve_config_path(paths.config)?;
    schedule::update_schedule_config(&config_path, interval_minutes, enabled)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to reload config {}", config_path.display()))?;
    if config.schedule.enabled {
        schedule::install_default_job(&config_path, config.schedule.interval_minutes)?;
    } else {
        schedule::uninstall_job()?;
    }
    Ok(ScheduleSnapshot {
        enabled: config.schedule.enabled,
        interval_minutes: config.schedule.interval_minutes,
    })
}

pub fn save_config(paths: SharedPaths, update: ConfigUpdate) -> anyhow::Result<ConfigSnapshot> {
    save_config_with_credentials(paths, update, None)
}

pub fn save_config_with_credentials(
    paths: SharedPaths,
    update: ConfigUpdate,
    credentials_path: Option<PathBuf>,
) -> anyhow::Result<ConfigSnapshot> {
    let config_path = resolve_config_path(paths.config)?;
    let contents = render_config_update(&update);
    let config = AppConfig::from_toml_str(&contents).context("updated config failed validation")?;
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&config_path, contents)
        .with_context(|| format!("failed to write config {}", config_path.display()))?;
    let allow_process_env = credentials_path.is_none() && config_path == resolve_config_path(None)?;
    let credentials = save_credentials_update(&update, credentials_path)?;
    Ok(ConfigSnapshot::from_config(
        config_path,
        &config,
        &credentials,
        allow_process_env,
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

pub fn export_config(paths: SharedPaths) -> anyhow::Result<ExportConfigResult> {
    let config_path = resolve_config_path(paths.config)?;
    let downloads = downloads_dir().context("failed to resolve Downloads directory")?;
    export_config_to_dir(config_path, downloads)
}

pub fn log_file() -> anyhow::Result<LogFileResult> {
    let path = log_file_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
    if !path.exists() {
        fs::write(&path, "")
            .with_context(|| format!("failed to create log file {}", path.display()))?;
    }
    Ok(LogFileResult {
        path: path.display().to_string(),
    })
}

pub fn append_log(message: &str) {
    if let Ok(path) = log_file_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let line = format!("{} {message}\n", crate::time::current_rfc3339_utc());
        let _ = append_log_line(&path, &line);
    }
}

fn log_file_path() -> anyhow::Result<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = env::var_os("HOME").context("HOME must be set to resolve log path")?;
        return Ok(PathBuf::from(home).join("Library/Logs/Toggl Jira Sync/toggl-jira-sync.log"));
    }
    let home = env::var_os("HOME").context("HOME must be set to resolve log path")?;
    Ok(PathBuf::from(home).join(".local/state/toggl-jira-sync/app.log"))
}

fn append_log_line(path: &PathBuf, line: &str) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())
}

fn export_config_to_dir(
    config_path: PathBuf,
    downloads: PathBuf,
) -> anyhow::Result<ExportConfigResult> {
    let contents = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read config {}", config_path.display()))?;
    fs::create_dir_all(&downloads)
        .with_context(|| format!("failed to create {}", downloads.display()))?;
    let backup_path = downloads.join(format!(
        "toggl-jira-sync-config-{}.toml",
        crate::time::current_rfc3339_utc()
            .replace([':', '-'], "")
            .replace('T', "-")
            .replace('Z', "")
    ));
    fs::write(&backup_path, contents)
        .with_context(|| format!("failed to write config backup {}", backup_path.display()))?;
    Ok(ExportConfigResult {
        path: backup_path.display().to_string(),
    })
}

fn downloads_dir() -> anyhow::Result<PathBuf> {
    if cfg!(windows) {
        if let Some(user_profile) = env::var_os("USERPROFILE") {
            return Ok(PathBuf::from(user_profile).join("Downloads"));
        }
    }
    env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Downloads"))
        .context("HOME is not set")
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
    pub fn redact_secrets(&mut self) {
        self.toggl_api_token_value = None;
        for site in &mut self.jira_sites {
            site.email_value = None;
            site.api_token_value = None;
        }
    }

    fn from_config(
        path: PathBuf,
        config: &AppConfig,
        credentials: &HashMap<String, String>,
        allow_process_env: bool,
    ) -> Self {
        Self {
            path: path.display().to_string(),
            toggl_workspace_id: config.toggl.workspace_id,
            toggl_api_token_env: config.toggl.api_token_env.clone(),
            toggl_api_token_present: credential_present(
                credentials,
                &config.toggl.api_token_env,
                allow_process_env,
            ),
            toggl_api_token_value: credentials.get(&config.toggl.api_token_env).cloned(),
            sqlite_path: config
                .runtime
                .sqlite_path
                .clone()
                .unwrap_or_else(|| "toggl-jira-sync.sqlite".to_owned()),
            initial_backfill_from_month: config.runtime.initial_backfill_from_month.clone(),
            initial_backfill_days: config.runtime.initial_backfill_days,
            recovery_from_month: config.runtime.recovery_from_month.clone(),
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
                    email_present: credential_present(
                        credentials,
                        &site.email_env,
                        allow_process_env,
                    ),
                    email_value: credentials.get(&site.email_env).cloned(),
                    api_token_present: credential_present(
                        credentials,
                        &site.api_token_env,
                        allow_process_env,
                    ),
                    api_token_value: credentials.get(&site.api_token_env).cloned(),
                    enabled: site.enabled,
                })
                .collect(),
        }
    }
}

fn credential_present(
    credentials: &HashMap<String, String>,
    name: &str,
    allow_process_env: bool,
) -> bool {
    credentials.contains_key(name) || (allow_process_env && env::var_os(name).is_some())
}

fn save_credentials_update(
    update: &ConfigUpdate,
    credentials_path: Option<PathBuf>,
) -> anyhow::Result<HashMap<String, String>> {
    let mut credentials = match credentials_path.as_ref() {
        Some(path) => read_credentials_from_path(path.clone()).unwrap_or_default(),
        None => read_default_credentials().unwrap_or_default(),
    };
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
        if let Some(path) = credentials_path {
            write_credentials_to_path(&path, &credentials)?;
        } else {
            write_default_credentials(&credentials)?;
        }
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
    if is_placeholder_secret(value) {
        return false;
    }
    credentials.insert(name.to_owned(), value.to_owned());
    true
}

fn is_placeholder_secret(value: &str) -> bool {
    matches!(
        value,
        "secret-token"
            | "replace-with-your-toggl-token"
            | "form.toggl_api_token_value.value.trim()"
    )
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
    read_credentials_from_path(path)
}

fn read_credentials_for_config(
    config_path: &PathBuf,
    credentials_path: Option<PathBuf>,
) -> anyhow::Result<HashMap<String, String>> {
    if let Some(path) = credentials_path {
        return read_credentials_from_path(path);
    }
    if *config_path == resolve_config_path(None)? {
        return read_default_credentials();
    }
    Ok(HashMap::new())
}

fn read_credentials_from_path(path: PathBuf) -> anyhow::Result<HashMap<String, String>> {
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
    write_credentials_to_path(&path, credentials)
}

fn write_credentials_to_path(
    path: &PathBuf,
    credentials: &HashMap<String, String>,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut lines = credentials
        .iter()
        .map(|(key, value)| format!("{key}={}", value.replace('\n', "")))
        .collect::<Vec<_>>();
    lines.sort();
    write_credentials_contents(path, &format!("{}\n", lines.join("\n")))?;
    Ok(())
}

#[cfg(unix)]
fn write_credentials_contents(path: &PathBuf, contents: &str) -> anyhow::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to write credentials {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write credentials {}", path.display()))?;
    let mut permissions = file
        .metadata()
        .with_context(|| format!("failed to inspect credentials {}", path.display()))?
        .permissions();
    permissions.set_mode(0o600);
    file.set_permissions(permissions)
        .with_context(|| format!("failed to secure credentials {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_credentials_contents(path: &PathBuf, contents: &str) -> anyhow::Result<()> {
    fs::write(path, contents)
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
{initial_backfill_from_month}
initial_backfill_days = {initial_backfill_days}
{recovery_from_month}
recovery_scan_days = {recovery_scan_days}

[schedule]
enabled = {schedule_enabled}
interval_minutes = {schedule_interval_minutes}

[jira]
"#,
        workspace_id = update.toggl_workspace_id,
        toggl_api_token_env = escape_toml_string(&update.toggl_api_token_env),
        sqlite_path = escape_toml_string(&update.sqlite_path),
        initial_backfill_from_month = render_optional_string(
            "initial_backfill_from_month",
            update.initial_backfill_from_month.as_deref()
        ),
        initial_backfill_days = update.initial_backfill_days,
        recovery_from_month =
            render_optional_string("recovery_from_month", update.recovery_from_month.as_deref()),
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

fn render_optional_string(key: &str, value: Option<&str>) -> String {
    value
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("{key} = \"{}\"", escape_toml_string(value.trim())))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, NewJiraWorklogLink, NewTogglEntry};

    fn sample_update(sqlite_path: String) -> ConfigUpdate {
        ConfigUpdate {
            toggl_workspace_id: 123,
            toggl_api_token_env: "TOGGL_API_TOKEN".to_owned(),
            toggl_api_token_value: None,
            sqlite_path,
            initial_backfill_from_month: Some("05.2026".to_owned()),
            initial_backfill_days: 90,
            recovery_from_month: None,
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
        }
    }

    fn write_sample_config(config_path: &std::path::Path, sqlite_path: &str) {
        save_config(
            SharedPaths {
                config: Some(config_path.to_path_buf()),
                db: None,
            },
            sample_update(sqlite_path.to_owned()),
        )
        .expect("save config");
    }

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
                initial_backfill_from_month: Some("05.2026".to_owned()),
                initial_backfill_days: 90,
                recovery_from_month: None,
                recovery_scan_days: 180,
                schedule_enabled: true,
                schedule_interval_minutes: 60,
                jira_sites: vec![
                    JiraSiteUpdate {
                        key: "acme".to_owned(),
                        base_url: "https://acme.atlassian.net".to_owned(),
                        email_env: "ACME_JIRA_EMAIL".to_owned(),
                        api_token_env: "ACME_JIRA_API_TOKEN".to_owned(),
                        email_value: None,
                        api_token_value: None,
                        enabled: true,
                    },
                    JiraSiteUpdate {
                        key: "client".to_owned(),
                        base_url: "https://client.atlassian.net".to_owned(),
                        email_env: "CLIENT_JIRA_EMAIL".to_owned(),
                        api_token_env: "CLIENT_JIRA_API_TOKEN".to_owned(),
                        email_value: None,
                        api_token_value: None,
                        enabled: true,
                    },
                ],
            },
        )
        .expect("save config");

        assert_eq!(saved.path, config_path.display().to_string());
        assert_eq!(saved.jira_sites[0].key, "acme");
        assert_eq!(saved.jira_sites[1].key, "client");
        AppConfig::from_path(config_path).expect("saved config parses");
    }

    #[test]
    fn config_snapshot_uses_temp_home_credentials() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let credentials_path = dir.path().join("credentials.env");
        fs::write(
            &config_path,
            r#"[toggl]
workspace_id = 123
api_token_env = "TOGGL_API_TOKEN"

[runtime]
sqlite_path = "ledger.sqlite"

[[jira.sites]]
key = "acme"
base_url = "https://acme.atlassian.net"
email_env = "ACME_JIRA_EMAIL"
api_token_env = "ACME_JIRA_API_TOKEN"
"#,
        )
        .expect("write config");
        fs::write(
            &credentials_path,
            "TOGGL_API_TOKEN=toggl-secret\nACME_JIRA_EMAIL=dev@example.com\nACME_JIRA_API_TOKEN=jira-secret\n",
        )
        .expect("write credentials");

        let snapshot = config_snapshot_with_credentials(
            SharedPaths {
                config: Some(config_path.clone()),
                db: None,
            },
            Some(credentials_path.clone()),
        )
        .expect("config snapshot");

        assert_eq!(snapshot.config.path, config_path.display().to_string());
        assert!(snapshot.config.toggl_api_token_present);
        assert_eq!(
            snapshot.config.toggl_api_token_value.as_deref(),
            Some("toggl-secret")
        );
        assert_eq!(
            snapshot.config.jira_sites[0].email_value.as_deref(),
            Some("dev@example.com")
        );
        assert_eq!(
            snapshot.config.jira_sites[0].api_token_value.as_deref(),
            Some("jira-secret")
        );
        assert!(credentials_path.exists());
    }

    #[test]
    fn save_config_does_not_overwrite_existing_credentials_with_placeholders() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let credentials_path = dir.path().join("credentials.env");
        fs::write(
            &credentials_path,
            "TOGGL_API_TOKEN=real-token\nACME_JIRA_EMAIL=dev@example.com\nACME_JIRA_API_TOKEN=real-jira-token\n",
        )
        .expect("write credentials");

        let snapshot = save_config_with_credentials(
            SharedPaths {
                config: Some(config_path.clone()),
                db: None,
            },
            ConfigUpdate {
                toggl_workspace_id: 123,
                toggl_api_token_env: "TOGGL_API_TOKEN".to_owned(),
                toggl_api_token_value: Some("secret-token".to_owned()),
                sqlite_path: "ledger.sqlite".to_owned(),
                initial_backfill_from_month: None,
                initial_backfill_days: 90,
                recovery_from_month: None,
                recovery_scan_days: 180,
                schedule_enabled: true,
                schedule_interval_minutes: 60,
                jira_sites: vec![JiraSiteUpdate {
                    key: "acme".to_owned(),
                    base_url: "https://acme.atlassian.net".to_owned(),
                    email_env: "ACME_JIRA_EMAIL".to_owned(),
                    api_token_env: "ACME_JIRA_API_TOKEN".to_owned(),
                    email_value: Some("dev@example.com".to_owned()),
                    api_token_value: Some("form.toggl_api_token_value.value.trim()".to_owned()),
                    enabled: true,
                }],
            },
            Some(credentials_path.clone()),
        )
        .expect("save config");

        assert_eq!(
            snapshot.toggl_api_token_value.as_deref(),
            Some("real-token")
        );
        assert_eq!(
            snapshot.jira_sites[0].api_token_value.as_deref(),
            Some("real-jira-token")
        );
        let credentials = fs::read_to_string(credentials_path).expect("credentials");
        assert!(credentials.contains("TOGGL_API_TOKEN=real-token"));
        assert!(credentials.contains("ACME_JIRA_API_TOKEN=real-jira-token"));
        assert!(!credentials.contains("TOGGL_API_TOKEN=secret-token"));
        assert!(!credentials.contains("form.toggl_api_token_value.value.trim()"));
    }

    #[test]
    fn explicit_config_snapshot_does_not_read_default_credentials() {
        let dir = tempfile::tempdir().expect("tempdir");
        let previous_home = env::var_os("HOME");
        let previous_token = env::var_os("TOGGL_API_TOKEN");
        env::set_var("HOME", dir.path());
        env::remove_var("TOGGL_API_TOKEN");
        let default_credentials = dir.path().join(".config/toggl-jira-sync/credentials.env");
        fs::create_dir_all(default_credentials.parent().expect("parent")).expect("mkdir");
        fs::write(&default_credentials, "TOGGL_API_TOKEN=default-secret\n").expect("credentials");

        let config_path = dir.path().join("explicit.toml");
        fs::write(
            &config_path,
            r#"[toggl]
workspace_id = 123
api_token_env = "TOGGL_API_TOKEN"

[[jira.sites]]
key = "acme"
base_url = "https://acme.atlassian.net"
email_env = "ACME_JIRA_EMAIL"
api_token_env = "ACME_JIRA_API_TOKEN"
"#,
        )
        .expect("config");

        let snapshot = config_snapshot(SharedPaths {
            config: Some(config_path),
            db: None,
        })
        .expect("snapshot");

        assert!(!snapshot.config.toggl_api_token_present);
        assert_eq!(snapshot.config.toggl_api_token_value, None);

        if let Some(home) = previous_home {
            env::set_var("HOME", home);
        } else {
            env::remove_var("HOME");
        }
        if let Some(token) = previous_token {
            env::set_var("TOGGL_API_TOKEN", token);
        }
    }

    #[test]
    fn snapshot_and_status_report_share_serializable_status_contract() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let db_path = dir.path().join("ledger.sqlite");
        write_sample_config(&config_path, db_path.to_str().expect("db path utf8"));

        let database = Database::open(&db_path).expect("open db");
        database.run_migrations().expect("migrations");
        database
            .upsert_toggl_entry(&NewTogglEntry {
                toggl_workspace_id: "123",
                toggl_entry_id: "456",
                description: Some("ACME-456 implementation"),
                extracted_issue_key: Some("ACME-456"),
                source_hash: "sha256:status",
                rounded_duration_seconds: 1800,
                status: "created",
                started_at: Some("2026-05-02T03:06:40Z"),
                stopped_at: Some("2026-05-02T03:36:40Z"),
            })
            .expect("insert toggl entry");
        database
            .upsert_jira_worklog_link(&NewJiraWorklogLink {
                toggl_workspace_id: "123",
                toggl_entry_id: "456",
                jira_site_key: "acme",
                jira_issue_key: "ACME-456",
                jira_worklog_id: Some("10001"),
                source_hash: "sha256:status",
                rounded_duration_seconds: 1800,
                status: "created",
            })
            .expect("insert worklog link");

        let app_snapshot = snapshot(
            SharedPaths {
                config: Some(config_path.clone()),
                db: Some(db_path.clone()),
            },
            10,
        )
        .expect("app snapshot");
        let (_, _, report) = status_report(
            SharedPaths {
                config: Some(config_path),
                db: Some(db_path),
            },
            10,
        )
        .expect("status report");

        assert_eq!(app_snapshot.status, report);
        assert_eq!(app_snapshot.status.summary.synced_count, 1);
        assert_eq!(
            app_snapshot.status.entries[0].issue_key.as_deref(),
            Some("ACME-456")
        );
        serde_json::to_value(&app_snapshot).expect("snapshot serializes");
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
                initial_backfill_from_month: None,
                initial_backfill_days: 90,
                recovery_from_month: None,
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

    #[test]
    fn export_config_writes_backup_to_downloads_without_credentials() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let downloads_path = dir.path().join("Downloads");

        save_config(
            SharedPaths {
                config: Some(config_path.clone()),
                db: None,
            },
            ConfigUpdate {
                toggl_workspace_id: 123,
                toggl_api_token_env: "TOGGL_API_TOKEN".to_owned(),
                toggl_api_token_value: Some("secret-token".to_owned()),
                sqlite_path: "toggl-jira-sync.sqlite".to_owned(),
                initial_backfill_from_month: Some("05.2026".to_owned()),
                initial_backfill_days: 90,
                recovery_from_month: Some("05.2026".to_owned()),
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

        let exported =
            export_config_to_dir(config_path, downloads_path.clone()).expect("export config");
        let backup = PathBuf::from(exported.path);
        let contents = fs::read_to_string(&backup).expect("backup content");

        assert_eq!(backup.parent(), Some(downloads_path.as_path()));
        assert!(contents.contains("initial_backfill_from_month = \"05.2026\""));
        assert!(!contents.contains("secret-token"));
    }
}
