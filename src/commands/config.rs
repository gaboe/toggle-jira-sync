use std::{
    collections::HashMap,
    env, fs,
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{anyhow, bail, Context};

use crate::{
    cli::{ConfigArgs, ConfigCommand, ConfigSetupArgs, ConfigShowArgs, ConfigValidateArgs},
    commands::schedule,
    config::{AppConfig, ConfigError},
    toggl::{TogglClient, TogglWorkspace},
};

const TOGGL_API_TOKEN_ENV: &str = "TOGGL_API_TOKEN";
const TOGGL_DEFAULT_BASE_URL: &str = "https://api.track.toggl.com";
const DEFAULT_CONFIG_DIR: &str = ".config/toggl-jira-sync";
const DEFAULT_CONFIG_FILE: &str = "config.toml";
const DEFAULT_CREDENTIALS_FILE: &str = "credentials.env";
const DEFAULT_SQLITE_PATH: &str = "toggl-jira-sync.sqlite";

pub async fn run(args: ConfigArgs) -> anyhow::Result<()> {
    match args.command {
        ConfigCommand::Setup(setup) => setup_config(setup).await,
        ConfigCommand::Show(show) => show_config(show),
        ConfigCommand::Validate(validate) => validate_config(validate),
    }
}

async fn setup_config(args: ConfigSetupArgs) -> anyhow::Result<()> {
    let config_path = resolve_config_path(args.config)?;
    let credentials_path = resolve_credentials_path(args.credentials)?;

    let input = SetupInput::prompt().await?;
    let site_env_prefix = env_prefix_for_site_key(&input.site_key)?;
    let jira_email_env = format!("{site_env_prefix}_JIRA_EMAIL");
    let jira_api_token_env = format!("{site_env_prefix}_JIRA_API_TOKEN");

    let config_text = render_config(&input, &jira_email_env, &jira_api_token_env);
    AppConfig::from_toml_str(&config_text).context("generated config failed validation")?;

    write_file(&config_path, &config_text)
        .with_context(|| format!("failed to write config file {}", config_path.display()))?;

    let credentials_text = render_credentials(&input, &jira_email_env, &jira_api_token_env);
    write_secret_file(&credentials_path, &credentials_text).with_context(|| {
        format!(
            "failed to write credentials file {}",
            credentials_path.display()
        )
    })?;

    println!("config saved: {}", config_path.display());
    println!("credentials saved: {}", credentials_path.display());
    if env::var_os("TJS_SKIP_SCHEDULE_INSTALL").is_none() {
        schedule::install_default_job(&config_path, input.schedule_interval_minutes)
            .context("failed to install hourly sync OS job")?;
        println!(
            "schedule installed: every {} minutes",
            input.schedule_interval_minutes
        );
    }

    Ok(())
}

fn show_config(args: ConfigShowArgs) -> anyhow::Result<()> {
    let uses_default_config_path = args.config.is_none();
    let config_path = resolve_config_path(args.config)?;
    let config = load_config_for_show(&config_path, uses_default_config_path)?;
    let credentials_path = resolve_credentials_path(args.credentials)?;
    let credentials = read_credentials(&credentials_path)
        .with_context(|| format!("failed to read credentials {}", credentials_path.display()))?;

    println!("config: {}", config_path.display());
    println!("toggl:");
    println!("  workspace_id: {}", config.toggl.workspace_id);
    print_credential_line(
        "  ",
        &config.toggl.api_token_env,
        credentials.get(&config.toggl.api_token_env),
        args.show_secrets,
    );
    println!("runtime:");
    println!(
        "  sqlite_path: {}",
        config.runtime.sqlite_path.as_deref().unwrap_or("<default>")
    );
    println!("schedule:");
    println!("  enabled: {}", config.schedule.enabled);
    println!("  interval_minutes: {}", config.schedule.interval_minutes);
    println!("jira:");
    for site in &config.jira.sites {
        println!("  site: {}", site.key);
        println!("    enabled: {}", site.enabled);
        println!("    base_url: {}", site.base_url);
        print_credential_line(
            "    ",
            &site.email_env,
            credentials.get(&site.email_env),
            args.show_secrets,
        );
        print_credential_line(
            "    ",
            &site.api_token_env,
            credentials.get(&site.api_token_env),
            args.show_secrets,
        );
    }

    Ok(())
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

fn validate_config(args: ConfigValidateArgs) -> anyhow::Result<()> {
    let config_path = args
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

    println!("config valid: {enabled_site_count} Jira sites enabled");

    Ok(())
}

#[derive(Debug)]
struct SetupInput {
    toggl_workspace_id: i64,
    toggl_api_token: String,
    site_key: String,
    jira_base_url: String,
    jira_email: String,
    jira_api_token: String,
    sqlite_path: String,
    schedule_interval_minutes: u32,
}

impl SetupInput {
    async fn prompt() -> anyhow::Result<Self> {
        let toggl_api_token = read_required("Toggl API token")?;
        let stdin_is_interactive = io::stdin().is_terminal();
        let discovered_workspaces = if stdin_is_interactive {
            Some(
                TogglClient::list_workspaces(TOGGL_DEFAULT_BASE_URL, &toggl_api_token)
                    .await
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
        let jira_base_url = read_required("Jira site URL")?;
        let site_key = derive_jira_site_key(&jira_base_url)?;
        println!("Using Jira site key: {site_key}");
        let jira_email = read_required("Jira email")?;
        let jira_api_token = read_required("Jira API token")?;
        let sqlite_path = DEFAULT_SQLITE_PATH.to_owned();
        let schedule_interval_minutes = 60;

        Ok(Self {
            toggl_workspace_id,
            toggl_api_token,
            site_key,
            jira_base_url,
            jira_email,
            jira_api_token,
            sqlite_path,
            schedule_interval_minutes,
        })
    }
}

pub(crate) fn resolve_config_path(path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    path.map(Ok)
        .unwrap_or_else(|| default_config_dir().map(|dir| dir.join(DEFAULT_CONFIG_FILE)))
}

#[derive(Debug, Default)]
pub(crate) struct LocalCredentials {
    values: HashMap<String, String>,
}

impl LocalCredentials {
    pub(crate) fn get_secret(&self, name: &str) -> anyhow::Result<String> {
        std::env::var(name)
            .or_else(|_| {
                self.values
                    .get(name)
                    .cloned()
                    .ok_or(std::env::VarError::NotPresent)
            })
            .with_context(|| format!("missing env var {name}"))
    }

    pub(crate) fn contains_secret(&self, name: &str) -> bool {
        std::env::var_os(name).is_some() || self.values.contains_key(name)
    }
}

pub(crate) fn load_default_credentials() -> anyhow::Result<LocalCredentials> {
    let credentials_path = resolve_credentials_path(None)?;
    if !credentials_path.exists() {
        return Ok(LocalCredentials::default());
    }

    let values = read_credentials(&credentials_path)
        .with_context(|| format!("failed to read credentials {}", credentials_path.display()))?
        .into_iter()
        .collect();

    Ok(LocalCredentials { values })
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

fn resolve_credentials_path(path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    path.map(Ok)
        .unwrap_or_else(|| default_config_dir().map(|dir| dir.join(DEFAULT_CREDENTIALS_FILE)))
}

fn default_config_dir() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME")
        .ok_or_else(|| anyhow!("HOME must be set to resolve default config paths"))?;
    Ok(PathBuf::from(home).join(DEFAULT_CONFIG_DIR))
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

fn render_config(input: &SetupInput, jira_email_env: &str, jira_api_token_env: &str) -> String {
    format!(
        "[toggl]\nworkspace_id = {}\napi_token_env = \"{}\"\n\n[runtime]\nsqlite_path = \"{}\"\n\n[schedule]\nenabled = true\ninterval_minutes = {}\n\n[jira]\n\n[[jira.sites]]\nkey = \"{}\"\nbase_url = \"{}\"\nemail_env = \"{}\"\napi_token_env = \"{}\"\nenabled = true\n",
        input.toggl_workspace_id,
        TOGGL_API_TOKEN_ENV,
        toml_escape(&input.sqlite_path),
        input.schedule_interval_minutes,
        toml_escape(&input.site_key),
        toml_escape(&input.jira_base_url),
        jira_email_env,
        jira_api_token_env,
    )
}

fn render_credentials(
    input: &SetupInput,
    jira_email_env: &str,
    jira_api_token_env: &str,
) -> String {
    format!(
        "{}={}\n{}={}\n{}={}\n",
        TOGGL_API_TOKEN_ENV,
        env_escape(&input.toggl_api_token),
        jira_email_env,
        env_escape(&input.jira_email),
        jira_api_token_env,
        env_escape(&input.jira_api_token),
    )
}

fn write_file(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    fs::write(path, contents)
}

fn write_secret_file(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    #[cfg(unix)]
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents.as_bytes())?;
        let mut permissions = file.metadata()?.permissions();
        permissions.set_mode(0o600);
        file.set_permissions(permissions)?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        fs::write(path, contents)
    }
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

fn print_credential_line(indent: &str, name: &str, value: Option<&String>, show_secrets: bool) {
    match (value, show_secrets) {
        (Some(value), true) => println!("{indent}{name}: {value}"),
        (Some(_), false) => println!("{indent}{name}: present (<redacted>)"),
        (None, _) => println!("{indent}{name}: missing"),
    }
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn env_escape(value: &str) -> String {
    value.replace('\n', "")
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
