use std::{collections::HashMap, fmt, fs, path::Path};

use serde::{Deserialize, Serialize};

const DEFAULT_INITIAL_BACKFILL_DAYS: u32 = 90;
const DEFAULT_RECOVERY_SCAN_DAYS: u32 = 180;
const DEFAULT_TOGGL_MAX_RPS: f64 = 1.0;
const DEFAULT_JIRA_GLOBAL_WRITE_DELAY_MS: u64 = 150;
const DEFAULT_JIRA_SAME_ISSUE_WRITE_DELAY_MS: u64 = 2_000;

#[derive(Debug)]
pub enum ConfigError {
    Read(String),
    Parse(String),
    Validation(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(message) | Self::Parse(message) | Self::Validation(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    toggl: TogglConfig,
    #[serde(default)]
    runtime: RuntimeConfig,
    #[serde(default)]
    rate_limits: RateLimitConfig,
    #[serde(default)]
    schedule: ScheduleConfig,
    jira: JiraConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TogglConfig {
    pub workspace_id: i64,
    pub api_token_env: String,
    #[serde(default = "default_toggl_base_url")]
    pub base_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub sqlite_path: Option<String>,
    #[serde(default)]
    pub initial_backfill_from_month: Option<String>,
    #[serde(default = "default_initial_backfill_days")]
    pub initial_backfill_days: u32,
    #[serde(default)]
    pub recovery_from_month: Option<String>,
    #[serde(default = "default_recovery_scan_days")]
    pub recovery_scan_days: u32,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            sqlite_path: None,
            initial_backfill_from_month: None,
            initial_backfill_days: DEFAULT_INITIAL_BACKFILL_DAYS,
            recovery_from_month: None,
            recovery_scan_days: DEFAULT_RECOVERY_SCAN_DAYS,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    #[serde(default = "default_toggl_max_rps")]
    pub toggl_max_rps: f64,
    #[serde(default = "default_jira_global_write_delay_ms")]
    pub jira_global_write_delay_ms: u64,
    #[serde(default = "default_jira_same_issue_write_delay_ms")]
    pub jira_same_issue_write_delay_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    #[serde(default = "default_schedule_enabled")]
    pub enabled: bool,
    #[serde(default = "default_schedule_interval_minutes")]
    pub interval_minutes: u32,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            enabled: default_schedule_enabled(),
            interval_minutes: default_schedule_interval_minutes(),
        }
    }
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            toggl_max_rps: DEFAULT_TOGGL_MAX_RPS,
            jira_global_write_delay_ms: DEFAULT_JIRA_GLOBAL_WRITE_DELAY_MS,
            jira_same_issue_write_delay_ms: DEFAULT_JIRA_SAME_ISSUE_WRITE_DELAY_MS,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JiraConfig {
    pub sites: Vec<JiraSiteConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JiraSiteConfig {
    pub key: String,
    pub base_url: String,
    pub email_env: String,
    pub api_token_env: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppConfig {
    pub toggl: TogglConfig,
    pub runtime: RuntimeConfig,
    pub rate_limits: RateLimitConfig,
    pub schedule: ScheduleConfig,
    pub jira: JiraConfig,
}

impl AppConfig {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|error| {
            ConfigError::Read(format!("failed to read config {}: {error}", path.display()))
        })?;

        Self::from_toml_str(&contents)
    }

    pub fn from_toml_str(contents: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(contents)
            .map_err(|error| ConfigError::Parse(format!("failed to parse config: {error}")))?;

        let config = Self {
            toggl: raw.toggl,
            runtime: raw.runtime,
            rate_limits: raw.rate_limits,
            schedule: raw.schedule,
            jira: raw.jira,
        };

        config.validate()?;
        Ok(config)
    }

    pub fn enabled_jira_sites(&self) -> Vec<&JiraSiteConfig> {
        self.jira.sites.iter().filter(|site| site.enabled).collect()
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();

        push_env_var_reference(
            &mut errors,
            "toggl.api_token_env",
            &self.toggl.api_token_env,
        );

        if self.toggl.workspace_id <= 0 {
            errors.push("toggl.workspace_id must be greater than 0".to_owned());
        }

        if self.runtime.initial_backfill_days == 0 {
            errors.push("runtime.initial_backfill_days must be greater than 0".to_owned());
        }

        if let Some(month) = self.runtime.initial_backfill_from_month.as_deref() {
            validate_month_reference(&mut errors, "runtime.initial_backfill_from_month", month);
        }

        if self.runtime.recovery_scan_days == 0 {
            errors.push("runtime.recovery_scan_days must be greater than 0".to_owned());
        }

        if let Some(month) = self.runtime.recovery_from_month.as_deref() {
            validate_month_reference(&mut errors, "runtime.recovery_from_month", month);
        }

        if self.rate_limits.toggl_max_rps <= 0.0 {
            errors.push("rate_limits.toggl_max_rps must be greater than 0".to_owned());
        }

        if self.schedule.interval_minutes == 0 {
            errors.push("schedule.interval_minutes must be greater than 0".to_owned());
        }

        if self.jira.sites.is_empty() {
            errors.push("jira.sites must contain at least one site".to_owned());
        }

        self.validate_sites(&mut errors);

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Validation(errors.join("; ")))
        }
    }

    fn validate_sites(&self, errors: &mut Vec<String>) {
        let mut enabled_site_keys: HashMap<&str, usize> = HashMap::new();

        for site in &self.jira.sites {
            let site_label = if site.key.trim().is_empty() {
                "<missing>"
            } else {
                site.key.as_str()
            };

            push_required(errors, &format!("jira.sites[{site_label}].key"), &site.key);
            push_required(
                errors,
                &format!("jira.sites[{site_label}].base_url"),
                &site.base_url,
            );
            push_https_url(
                errors,
                &format!("jira.sites[{site_label}].base_url"),
                &site.base_url,
            );
            push_env_var_reference(
                errors,
                &format!("jira.sites[{site_label}].email_env"),
                &site.email_env,
            );
            push_env_var_reference(
                errors,
                &format!("jira.sites[{site_label}].api_token_env"),
                &site.api_token_env,
            );

            if site.enabled {
                *enabled_site_keys.entry(site.key.as_str()).or_default() += 1;
            }
        }

        for (site_key, count) in enabled_site_keys {
            if count > 1 {
                errors.push(format!(
                    "jira site key {site_key} is configured for multiple enabled sites"
                ));
            }
        }
    }
}

fn push_required(errors: &mut Vec<String>, field: &str, value: &str) {
    if value.trim().is_empty() {
        errors.push(format!("{field} must be set"));
    }
}

fn push_env_var_reference(errors: &mut Vec<String>, field: &str, value: &str) {
    push_required(errors, field, value);

    if !value.trim().is_empty() && !is_env_var_name(value) {
        errors.push(format!(
            "{field} must be an environment variable name using letters, digits, and underscores"
        ));
    }
}

fn validate_month_reference(errors: &mut Vec<String>, field: &str, value: &str) {
    if crate::time::month_start_since(value).is_none() {
        errors.push(format!("{field} must use MM.YYYY, for example 05.2026"));
    }
}

fn is_env_var_name(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
}

fn push_https_url(errors: &mut Vec<String>, field: &str, value: &str) {
    if !value.trim().is_empty() && !value.starts_with("https://") && !is_local_http_url(value) {
        errors.push(format!("{field} must start with https://"));
    }
}

fn is_local_http_url(value: &str) -> bool {
    value.starts_with("http://127.0.0.1:") || value.starts_with("http://localhost:")
}

fn default_toggl_base_url() -> String {
    "https://api.track.toggl.com".to_owned()
}

fn default_enabled() -> bool {
    true
}

fn default_initial_backfill_days() -> u32 {
    DEFAULT_INITIAL_BACKFILL_DAYS
}

fn default_recovery_scan_days() -> u32 {
    DEFAULT_RECOVERY_SCAN_DAYS
}

fn default_toggl_max_rps() -> f64 {
    DEFAULT_TOGGL_MAX_RPS
}

fn default_jira_global_write_delay_ms() -> u64 {
    DEFAULT_JIRA_GLOBAL_WRITE_DELAY_MS
}

fn default_jira_same_issue_write_delay_ms() -> u64 {
    DEFAULT_JIRA_SAME_ISSUE_WRITE_DELAY_MS
}

fn default_schedule_enabled() -> bool {
    true
}

fn default_schedule_interval_minutes() -> u32 {
    60
}
