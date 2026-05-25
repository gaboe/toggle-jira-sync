use std::{
    collections::HashMap,
    env, fs,
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};

use crate::{
    app::{ConfigUpdate, JiraSiteUpdate},
    cli::{ConfigArgs, ConfigCommand, ConfigSetupArgs, ConfigShowArgs, ConfigValidateArgs},
    commands::schedule,
    config::{AppConfig, ConfigError},
    local_api::LocalServer,
    toggl::TogglWorkspace,
};

const TOGGL_API_TOKEN_ENV: &str = "TOGGL_API_TOKEN";
const TOGGL_DEFAULT_BASE_URL: &str = "https://api.track.toggl.com";
const DEFAULT_CONFIG_DIR: &str = ".config/toggl-jira-sync";
#[cfg(windows)]
const DEFAULT_WINDOWS_CONFIG_DIR: &str = "toggl-jira-sync";
const DEFAULT_CONFIG_FILE: &str = "config.toml";
const DEFAULT_CREDENTIALS_FILE: &str = "credentials.env";
const DEFAULT_SQLITE_PATH: &str = "toggl-jira-sync.sqlite";

pub async fn run(args: ConfigArgs) -> anyhow::Result<()> {
    match args.command {
        ConfigCommand::Setup(setup) => setup_config(setup).await,
        ConfigCommand::Show(show) => show_config(show).await,
        ConfigCommand::Validate(validate) => validate_config(validate).await,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigShowReport {
    pub lines: Vec<String>,
}

impl ConfigShowReport {
    pub fn print(&self) {
        for line in &self.lines {
            println!("{line}");
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigValidateReport {
    pub enabled_site_count: usize,
}

impl ConfigValidateReport {
    pub fn print(&self) {
        println!(
            "config valid: {} Jira sites enabled",
            self.enabled_site_count
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSetupWriteRequest {
    pub update: ConfigUpdate,
    pub install_schedule: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSetupWriteReport {
    pub config_path: String,
    pub credentials_path: String,
    pub schedule_installed: bool,
    pub schedule_interval_minutes: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ConfigDiscoverTogglWorkspacesRequest {
    pub base_url: String,
    pub api_token: String,
}

impl std::fmt::Debug for ConfigDiscoverTogglWorkspacesRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfigDiscoverTogglWorkspacesRequest")
            .field("base_url", &self.base_url)
            .field("api_token", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigDiscoverTogglWorkspacesReport {
    pub workspaces: Vec<TogglWorkspace>,
}

async fn setup_config(args: ConfigSetupArgs) -> anyhow::Result<()> {
    let config_path = resolve_config_path(args.config)?;
    let credentials_path = resolve_credentials_path(args.credentials)?;

    let server = LocalServer::start(
        crate::cli::SharedPaths {
            config: Some(config_path.clone()),
            db: None,
        },
        Some(credentials_path.clone()),
        200,
    )
    .await?;
    let client = server.client();

    let mut input = SetupInput::prompt(&client).await?;
    for site in &mut input.jira_sites {
        let site_env_prefix = env_prefix_for_site_key(&site.site_key)?;
        site.jira_email_env = format!("{site_env_prefix}_JIRA_EMAIL");
        site.jira_api_token_env = format!("{site_env_prefix}_JIRA_API_TOKEN");
    }

    let request = ConfigSetupWriteRequest {
        update: input.to_config_update(),
        install_schedule: env::var_os("TJS_SKIP_SCHEDULE_INSTALL").is_none(),
    };
    let report = client.config_setup_write(&request).await?;

    println!("config saved: {}", report.config_path);
    println!("credentials saved: {}", report.credentials_path);
    if report.schedule_installed {
        println!(
            "schedule installed: every {} minutes",
            report.schedule_interval_minutes
        );
    }

    Ok(())
}

async fn show_config(args: ConfigShowArgs) -> anyhow::Result<()> {
    let server = LocalServer::start(
        crate::cli::SharedPaths {
            config: args.config,
            db: None,
        },
        args.credentials,
        200,
    )
    .await?;
    let report = server.client().config_show(args.show_secrets).await?;
    report.print();

    Ok(())
}

pub(crate) fn config_show_report(
    paths: crate::cli::SharedPaths,
    credentials_path: Option<PathBuf>,
    show_secrets: bool,
) -> anyhow::Result<ConfigShowReport> {
    let uses_default_config_path = paths.config.is_none();
    let config_path = resolve_config_path(paths.config)?;
    let config = load_config_for_show(&config_path, uses_default_config_path)?;
    let credentials_path = resolve_credentials_path(credentials_path)?;
    let credentials = read_credentials(&credentials_path)
        .with_context(|| format!("failed to read credentials {}", credentials_path.display()))?;

    let mut lines = Vec::new();
    lines.push(format!("config: {}", config_path.display()));
    lines.push("toggl:".to_owned());
    lines.push(format!("  workspace_id: {}", config.toggl.workspace_id));
    push_credential_line(
        &mut lines,
        "  ",
        &config.toggl.api_token_env,
        credentials.get(&config.toggl.api_token_env),
        show_secrets,
    );
    lines.push("runtime:".to_owned());
    lines.push(format!(
        "  sqlite_path: {}",
        config.runtime.sqlite_path.as_deref().unwrap_or("<default>")
    ));
    lines.push("schedule:".to_owned());
    lines.push(format!("  enabled: {}", config.schedule.enabled));
    lines.push(format!(
        "  interval_minutes: {}",
        config.schedule.interval_minutes
    ));
    lines.push("jira:".to_owned());
    for site in &config.jira.sites {
        lines.push(format!("  site: {}", site.key));
        lines.push(format!("    enabled: {}", site.enabled));
        lines.push(format!("    base_url: {}", site.base_url));
        push_credential_line(
            &mut lines,
            "    ",
            &site.email_env,
            credentials.get(&site.email_env),
            show_secrets,
        );
        push_credential_line(
            &mut lines,
            "    ",
            &site.api_token_env,
            credentials.get(&site.api_token_env),
            show_secrets,
        );
    }

    Ok(ConfigShowReport { lines })
}

fn load_config_for_show(
    config_path: &Path,
    uses_default_config_path: bool,
) -> anyhow::Result<AppConfig> {
    AppConfig::from_path(config_path).map_err(|error| match error {
        ConfigError::Read(_message) if uses_default_config_path && !config_path.exists() => {
            anyhow!(
                "Config not found: {}\nRun: tjs config setup",
                config_path.display()
            )
        }
        ConfigError::Read(message) => {
            anyhow!("failed to load config {}\n{message}", config_path.display())
        }
        ConfigError::Parse(message) | ConfigError::Validation(message) => {
            anyhow!("failed to load config {}\n{message}", config_path.display())
        }
    })
}

async fn validate_config(args: ConfigValidateArgs) -> anyhow::Result<()> {
    let server = LocalServer::start(
        crate::cli::SharedPaths {
            config: args.config,
            db: None,
        },
        None,
        200,
    )
    .await?;
    let report = server.client().config_validate().await?;
    report.print();

    Ok(())
}

pub(crate) fn config_validate_report(
    paths: crate::cli::SharedPaths,
) -> anyhow::Result<ConfigValidateReport> {
    let config_path = paths
        .config
        .ok_or_else(|| anyhow!("--config is required for config validate"))?;
    let config = AppConfig::from_path(&config_path).map_err(|error| match error {
        ConfigError::Read(message) if !config_path.exists() => {
            anyhow!("Config not found: {}\n{message}", config_path.display())
        }
        ConfigError::Read(message)
        | ConfigError::Parse(message)
        | ConfigError::Validation(message) => {
            anyhow!(
                "config validation failed for {}\n{message}",
                config_path.display()
            )
        }
    })?;
    let enabled_site_count = config.enabled_jira_sites().len();

    Ok(ConfigValidateReport { enabled_site_count })
}

pub(crate) fn config_setup_write(
    paths: crate::cli::SharedPaths,
    credentials_path: Option<PathBuf>,
    request: ConfigSetupWriteRequest,
) -> anyhow::Result<ConfigSetupWriteReport> {
    let config_path = resolve_config_path(paths.config.clone())?;
    let credentials_path = resolve_credentials_path(credentials_path)?;
    let install_schedule = request.install_schedule;
    let schedule_interval_minutes = request.update.schedule_interval_minutes;
    crate::app::save_config_with_credentials(paths, request.update, Some(credentials_path.clone()))
        .context("failed to save setup config")?;
    if install_schedule {
        schedule::install_default_job(&config_path, schedule_interval_minutes)
            .context("failed to install hourly sync OS job")?;
    }
    Ok(ConfigSetupWriteReport {
        config_path: config_path.display().to_string(),
        credentials_path: credentials_path.display().to_string(),
        schedule_installed: install_schedule,
        schedule_interval_minutes,
    })
}

#[derive(Debug)]
struct SetupInput {
    toggl_workspace_id: i64,
    toggl_api_token: String,
    jira_sites: Vec<SetupJiraSiteInput>,
    sqlite_path: String,
    schedule_interval_minutes: u32,
}

#[derive(Debug)]
struct SetupJiraSiteInput {
    site_key: String,
    jira_base_url: String,
    jira_email: String,
    jira_api_token: String,
    jira_email_env: String,
    jira_api_token_env: String,
}

impl SetupInput {
    async fn prompt(client: &crate::local_api::LocalApiClient) -> anyhow::Result<Self> {
        let toggl_api_token = read_required("Toggl API token")?;
        let stdin_is_interactive = io::stdin().is_terminal();
        let discovered_workspaces = if stdin_is_interactive {
            Some(
                client
                    .config_discover_toggl_workspaces(TOGGL_DEFAULT_BASE_URL, &toggl_api_token)
                    .await
                    .map(|report| report.workspaces)
                    .map_err(|error| error.to_string()),
            )
        } else {
            None
        };
        let toggl_workspace_id = {
            let mut stdin = io::stdin().lock();
            let mut stdout = io::stdout();
            prompt_toggl_workspace_id(
                &mut stdin,
                &mut stdout,
                &toggl_api_token,
                stdin_is_interactive,
                |_| {
                    discovered_workspaces.unwrap_or_else(|| {
                        Err(
                            "workspace discovery is unavailable in non-interactive setup"
                                .to_owned(),
                        )
                    })
                },
            )?
        };
        let mut jira_sites = Vec::new();
        loop {
            let jira_base_url = read_required("Jira site URL")?;
            let site_key = derive_jira_site_key(&jira_base_url)?;
            println!("Using Jira site key: {site_key}");
            let jira_email = read_required("Jira email")?;
            let jira_api_token = read_required("Jira API token")?;
            jira_sites.push(SetupJiraSiteInput {
                site_key,
                jira_base_url,
                jira_email,
                jira_api_token,
                jira_email_env: String::new(),
                jira_api_token_env: String::new(),
            });

            if !read_yes_no("Add another Jira site?", false)? {
                break;
            }
        }
        let sqlite_path = DEFAULT_SQLITE_PATH.to_owned();
        let schedule_interval_minutes = 60;

        Ok(Self {
            toggl_workspace_id,
            toggl_api_token,
            jira_sites,
            sqlite_path,
            schedule_interval_minutes,
        })
    }

    fn to_config_update(&self) -> ConfigUpdate {
        ConfigUpdate {
            toggl_workspace_id: self.toggl_workspace_id,
            toggl_api_token_env: TOGGL_API_TOKEN_ENV.to_owned(),
            toggl_api_token_value: Some(self.toggl_api_token.clone()),
            sqlite_path: self.sqlite_path.clone(),
            initial_backfill_from_month: None,
            initial_backfill_days: 90,
            recovery_from_month: None,
            recovery_scan_days: 180,
            schedule_enabled: true,
            schedule_interval_minutes: self.schedule_interval_minutes,
            jira_sites: self
                .jira_sites
                .iter()
                .map(|site| JiraSiteUpdate {
                    key: site.site_key.clone(),
                    base_url: site.jira_base_url.clone(),
                    email_env: site.jira_email_env.clone(),
                    api_token_env: site.jira_api_token_env.clone(),
                    email_value: Some(site.jira_email.clone()),
                    api_token_value: Some(site.jira_api_token.clone()),
                    enabled: true,
                })
                .collect(),
        }
    }
}

pub(crate) fn resolve_config_path(path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    path.map(Ok)
        .unwrap_or_else(|| default_config_dir().map(|dir| dir.join(DEFAULT_CONFIG_FILE)))
}

#[derive(Debug, Default)]
pub(crate) struct LocalCredentials {
    values: HashMap<String, String>,
    allow_process_env: bool,
}

impl LocalCredentials {
    pub(crate) fn process_env() -> Self {
        Self {
            values: HashMap::new(),
            allow_process_env: true,
        }
    }

    pub(crate) fn get_secret(&self, name: &str) -> anyhow::Result<String> {
        self.values
            .get(name)
            .cloned()
            .or_else(|| {
                self.allow_process_env
                    .then(|| std::env::var(name).ok())
                    .flatten()
            })
            .ok_or(std::env::VarError::NotPresent)
            .with_context(|| format!("missing env var {name}"))
    }

    pub(crate) fn contains_secret(&self, name: &str) -> bool {
        self.values.contains_key(name)
            || (self.allow_process_env && std::env::var_os(name).is_some())
    }
}

pub(crate) fn load_default_credentials() -> anyhow::Result<LocalCredentials> {
    let credentials_path = resolve_credentials_path(None)?;
    load_credentials_from_path_with_env(&credentials_path, true)
}

pub(crate) fn load_credentials_from_path(
    credentials_path: &Path,
) -> anyhow::Result<LocalCredentials> {
    load_credentials_from_path_with_env(credentials_path, true)
}

pub(crate) fn load_isolated_credentials_from_path(
    credentials_path: &Path,
) -> anyhow::Result<LocalCredentials> {
    load_credentials_from_path_with_env(credentials_path, false)
}

fn load_credentials_from_path_with_env(
    credentials_path: &Path,
    allow_process_env: bool,
) -> anyhow::Result<LocalCredentials> {
    if !credentials_path.exists() {
        return Ok(LocalCredentials {
            values: HashMap::new(),
            allow_process_env,
        });
    }

    let values = read_credentials(credentials_path)
        .with_context(|| format!("failed to read credentials {}", credentials_path.display()))?
        .into_iter()
        .collect();

    Ok(LocalCredentials {
        values,
        allow_process_env,
    })
}

pub(crate) fn resolve_db_path(
    explicit_db_path: Option<PathBuf>,
    config_path: &Path,
    sqlite_path: Option<&str>,
    command_name: &str,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = explicit_db_path {
        return Ok(path);
    }

    let path = sqlite_path
        .ok_or_else(|| anyhow!("--db or runtime.sqlite_path is required for {command_name}"))?;
    let path = PathBuf::from(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(config_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .join(path))
    }
}

pub(crate) fn resolve_credentials_path(path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    path.map(Ok)
        .unwrap_or_else(|| default_config_dir().map(|dir| dir.join(DEFAULT_CREDENTIALS_FILE)))
}

fn default_config_dir() -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(appdata) = env::var_os("APPDATA") {
            return Ok(PathBuf::from(appdata).join(DEFAULT_WINDOWS_CONFIG_DIR));
        }
        if let Some(user_profile) = env::var_os("USERPROFILE") {
            return Ok(PathBuf::from(user_profile)
                .join("AppData")
                .join("Roaming")
                .join(DEFAULT_WINDOWS_CONFIG_DIR));
        }
    }

    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(DEFAULT_CONFIG_DIR));
    }

    #[cfg(windows)]
    bail!("APPDATA, USERPROFILE, or HOME must be set to resolve default config paths");
    #[cfg(not(windows))]
    bail!("HOME must be set to resolve default config paths");
}

fn read_required(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}: ");
    io::stdout().flush().context("failed to flush prompt")?;

    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .with_context(|| format!("failed to read {prompt}"))?;
    let value = value.trim().to_owned();
    if value.is_empty() {
        bail!("{prompt} must be set");
    }

    Ok(value)
}

fn read_yes_no(prompt: &str, default: bool) -> anyhow::Result<bool> {
    let default_hint = if default { "Y/n" } else { "y/N" };
    print!("{prompt} [{default_hint}]: ");
    io::stdout().flush().context("failed to flush prompt")?;

    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .with_context(|| format!("failed to read {prompt}"))?;
    match value.trim().to_ascii_lowercase().as_str() {
        "" => Ok(default),
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => bail!("{prompt} must be y or n"),
    }
}

fn read_required_from<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    prompt: &str,
) -> anyhow::Result<String> {
    write!(writer, "{prompt}: ").context("failed to write prompt")?;
    writer.flush().context("failed to flush prompt")?;

    let mut value = String::new();
    reader
        .read_line(&mut value)
        .with_context(|| format!("failed to read {prompt}"))?;
    let value = value.trim().to_owned();
    if value.is_empty() {
        bail!("{prompt} must be set");
    }

    Ok(value)
}

fn prompt_toggl_workspace_id<R, W, F>(
    reader: &mut R,
    writer: &mut W,
    api_token: &str,
    stdin_is_interactive: bool,
    discover_workspaces: F,
) -> anyhow::Result<i64>
where
    R: BufRead,
    W: Write,
    F: FnOnce(&str) -> Result<Vec<TogglWorkspace>, String>,
{
    if !stdin_is_interactive {
        writeln!(
            writer,
            "Workspace discovery is skipped for piped or non-interactive setup."
        )
        .context("failed to write workspace discovery fallback message")?;
        return read_manual_toggl_workspace_id(reader, writer);
    }

    match discover_workspaces(api_token) {
        Ok(workspaces) if workspaces.len() == 1 => {
            let workspace = &workspaces[0];
            writeln!(
                writer,
                "Using Toggl workspace: {} ({})",
                workspace.name, workspace.id
            )
            .context("failed to write workspace selection")?;
            Ok(workspace.id)
        }
        Ok(workspaces) if workspaces.len() > 1 => {
            select_toggl_workspace(reader, writer, &workspaces)
        }
        Ok(_) => {
            writeln!(
                writer,
                "No Toggl workspaces were found for that token. You can enter the workspace id manually."
            )
            .context("failed to write workspace fallback message")?;
            read_manual_toggl_workspace_id(reader, writer)
        }
        Err(error) => {
            writeln!(
                writer,
                "Could not discover Toggl workspaces automatically ({error}). You can enter the workspace id manually."
            )
            .context("failed to write workspace discovery error")?;
            read_manual_toggl_workspace_id(reader, writer)
        }
    }
}

fn select_toggl_workspace<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    workspaces: &[TogglWorkspace],
) -> anyhow::Result<i64> {
    writeln!(writer, "Found Toggl workspaces:").context("failed to write workspace list header")?;
    for (index, workspace) in workspaces.iter().enumerate() {
        writeln!(
            writer,
            "{}) {} ({})",
            index + 1,
            workspace.name,
            workspace.id
        )
        .context("failed to write workspace list item")?;
    }

    let selection = read_required_from(
        reader,
        writer,
        &format!(
            "Select Toggl workspace [1-{}] or enter workspace id",
            workspaces.len()
        ),
    )?;
    let parsed = selection
        .parse::<i64>()
        .context("Toggl workspace selection must be a number or workspace id")?;

    if (1..=workspaces.len() as i64).contains(&parsed) {
        Ok(workspaces[(parsed - 1) as usize].id)
    } else {
        Ok(parsed)
    }
}

fn read_manual_toggl_workspace_id<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
) -> anyhow::Result<i64> {
    read_required_from(
        reader,
        writer,
        "Toggl workspace id (only needed if workspace discovery is skipped)",
    )?
    .parse::<i64>()
    .context("Toggl workspace id must be an integer")
}

fn env_prefix_for_site_key(site_key: &str) -> anyhow::Result<String> {
    let prefix = site_key
        .chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() {
                char.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_owned();

    if prefix.is_empty() {
        bail!("Site key must contain at least one letter or digit");
    }

    Ok(prefix)
}

fn derive_jira_site_key(jira_base_url: &str) -> anyhow::Result<String> {
    let without_scheme = jira_base_url
        .trim()
        .strip_prefix("https://")
        .or_else(|| jira_base_url.trim().strip_prefix("http://"))
        .unwrap_or_else(|| jira_base_url.trim());
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('.');
    let tenant = host.strip_suffix(".atlassian.net").unwrap_or(host);
    let key = tenant
        .chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() {
                char.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if key.is_empty() {
        bail!("Jira site URL must contain a usable host name");
    }

    Ok(key)
}

fn read_credentials(path: &Path) -> anyhow::Result<HashMap<String, String>> {
    let contents = fs::read_to_string(path)?;
    let mut credentials = HashMap::new();

    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            bail!("credentials line {} must use KEY=value format", index + 1);
        };

        credentials.insert(key.trim().to_owned(), value.trim().to_owned());
    }

    Ok(credentials)
}

fn push_credential_line(
    lines: &mut Vec<String>,
    indent: &str,
    name: &str,
    value: Option<&String>,
    show_secrets: bool,
) {
    match (value, show_secrets) {
        (Some(value), true) => lines.push(format!("{indent}{name}: {value}")),
        (Some(_), false) => lines.push(format!("{indent}{name}: present (<redacted>)")),
        (None, _) => lines.push(format!("{indent}{name}: missing")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toggl::TogglWorkspace;

    #[test]
    fn workspace_prompt_auto_selects_single_discovered_workspace() {
        let mut input = io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        let workspace_id =
            prompt_toggl_workspace_id(&mut input, &mut output, "fake-token", true, |_| {
                Ok(vec![TogglWorkspace {
                    id: 700001,
                    name: "Engineering".to_owned(),
                }])
            })
            .expect("single workspace should be selected automatically");

        let output = String::from_utf8(output).expect("prompt output should be utf-8");
        assert_eq!(workspace_id, 700001);
        assert!(
            output.contains("Using Toggl workspace: Engineering (700001)"),
            "{output}"
        );
    }

    #[test]
    fn workspace_prompt_lists_multiple_workspaces_and_accepts_number_selection() {
        let mut input = io::Cursor::new(b"2\n".to_vec());
        let mut output = Vec::new();

        let workspace_id =
            prompt_toggl_workspace_id(&mut input, &mut output, "fake-token", true, |_| {
                Ok(vec![
                    TogglWorkspace {
                        id: 700001,
                        name: "Engineering".to_owned(),
                    },
                    TogglWorkspace {
                        id: 700002,
                        name: "Operations".to_owned(),
                    },
                ])
            })
            .expect("numbered workspace selection should succeed");

        let output = String::from_utf8(output).expect("prompt output should be utf-8");
        assert_eq!(workspace_id, 700002);
        assert!(output.contains("1) Engineering (700001)"), "{output}");
        assert!(output.contains("2) Operations (700002)"), "{output}");
        assert!(
            output.contains("Select Toggl workspace [1-2] or enter workspace id:"),
            "{output}"
        );
    }

    #[test]
    fn workspace_prompt_falls_back_to_manual_id_when_discovery_fails() {
        let mut input = io::Cursor::new(b"123456\n".to_vec());
        let mut output = Vec::new();

        let workspace_id =
            prompt_toggl_workspace_id(&mut input, &mut output, "fake-token", true, |_| {
                Err("mock discovery failed".to_owned())
            })
            .expect("manual fallback should accept workspace id");

        let output = String::from_utf8(output).expect("prompt output should be utf-8");
        assert_eq!(workspace_id, 123456);
        assert!(
            output.contains("Could not discover Toggl workspaces automatically"),
            "{output}"
        );
        assert!(
            output.contains("Toggl workspace id (only needed if workspace discovery is skipped):"),
            "{output}"
        );
    }

    #[test]
    fn workspace_prompt_skips_discovery_when_stdin_is_not_interactive() {
        let mut input = io::Cursor::new(b"123456\n".to_vec());
        let mut output = Vec::new();
        let mut discovery_called = false;

        let workspace_id =
            prompt_toggl_workspace_id(&mut input, &mut output, "fake-token", false, |_| {
                discovery_called = true;
                Ok(vec![TogglWorkspace {
                    id: 700001,
                    name: "Engineering".to_owned(),
                }])
            })
            .expect("manual fallback should accept workspace id");

        let output = String::from_utf8(output).expect("prompt output should be utf-8");
        assert_eq!(workspace_id, 123456);
        assert!(!discovery_called, "piped setup must not call Toggl");
        assert!(
            output.contains("Workspace discovery is skipped for piped or non-interactive setup"),
            "{output}"
        );
    }

    #[test]
    fn workspace_discovery_request_debug_redacts_token() {
        let request = ConfigDiscoverTogglWorkspacesRequest {
            base_url: "https://api.track.toggl.com".to_owned(),
            api_token: "secret-token".to_owned(),
        };
        let debug = format!("{request:?}");

        assert!(debug.contains("https://api.track.toggl.com"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(!debug.contains("secret-token"), "{debug}");
    }

    #[test]
    fn jira_site_key_is_derived_from_atlassian_url() {
        assert_eq!(
            derive_jira_site_key("https://sabservis.atlassian.net").expect("site key"),
            "sabservis"
        );
        assert_eq!(
            derive_jira_site_key("https://sabservis.atlassian.net/").expect("site key"),
            "sabservis"
        );
        assert_eq!(
            derive_jira_site_key("https://team-name.atlassian.net/path").expect("site key"),
            "team-name"
        );
    }
}
