use std::{fmt, sync::Mutex, time::Duration};

use reqwest::Url;
use serde::Deserialize;

use crate::{
    config::AppConfig,
    time::{initial_backfill_since, month_start_since},
};

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum TogglError {
    InvalidConfig(String),
    Http(reqwest::Error),
    HttpStatus {
        status: reqwest::StatusCode,
        url: String,
        body: String,
    },
    PacingStatePoisoned,
}

impl fmt::Display for TogglError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => formatter.write_str(message),
            Self::Http(error) => write!(formatter, "toggl http error: {error}"),
            Self::HttpStatus { status, url, body } => {
                write!(formatter, "toggl http error: {status} for url ({url})")?;
                if !body.trim().is_empty() {
                    write!(formatter, ": {}", body.trim())?;
                }
                Ok(())
            }
            Self::PacingStatePoisoned => formatter.write_str("toggl pacing state is poisoned"),
        }
    }
}

impl std::error::Error for TogglError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            Self::InvalidConfig(_) | Self::HttpStatus { .. } | Self::PacingStatePoisoned => None,
        }
    }
}

impl From<reqwest::Error> for TogglError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

trait TogglResponseExt {
    async fn error_for_status_with_body(self) -> TogglResult<reqwest::Response>;
}

impl TogglResponseExt for reqwest::Response {
    async fn error_for_status_with_body(self) -> TogglResult<reqwest::Response> {
        let status = self.status();
        if status.is_success() {
            return Ok(self);
        }

        let url = self.url().to_string();
        let body = self.text().await.unwrap_or_default();
        Err(TogglError::HttpStatus { status, url, body })
    }
}

pub type TogglResult<T> = Result<T, TogglError>;

#[derive(Clone)]
pub struct TogglClientConfig {
    pub base_url: String,
    pub workspace_id: i64,
    pub api_token: String,
    pub max_rps: f64,
    pub initial_backfill_from_month: Option<String>,
    pub initial_backfill_days: u32,
}

impl TogglClientConfig {
    pub fn from_app_config(
        config: &AppConfig,
        api_token: String,
        base_url: String,
    ) -> TogglResult<Self> {
        Ok(Self {
            base_url,
            workspace_id: config.toggl.workspace_id,
            api_token,
            max_rps: config.rate_limits.toggl_max_rps,
            initial_backfill_from_month: config.runtime.initial_backfill_from_month.clone(),
            initial_backfill_days: config.runtime.initial_backfill_days,
        })
    }

    pub fn initial_backfill_since(&self, now_unix_seconds: i64) -> i64 {
        if let Some(month) = self
            .initial_backfill_from_month
            .as_deref()
            .and_then(month_start_since)
        {
            return month;
        }
        initial_backfill_since(now_unix_seconds, self.initial_backfill_days)
    }
}

impl fmt::Debug for TogglClientConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TogglClientConfig")
            .field("base_url", &self.base_url)
            .field("workspace_id", &self.workspace_id)
            .field("api_token", &"<redacted>")
            .field("max_rps", &self.max_rps)
            .field(
                "initial_backfill_from_month",
                &self.initial_backfill_from_month,
            )
            .field("initial_backfill_days", &self.initial_backfill_days)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TogglTimeEntry {
    pub workspace_id: String,
    pub entry_id: String,
    pub start: String,
    pub duration_seconds: i64,
    pub description: Option<String>,
    pub updated_at: String,
    pub deleted_at: Option<String>,
    pub skip_reason: Option<SkipReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TogglWorkspace {
    pub id: i64,
    pub name: String,
}

impl TogglTimeEntry {
    pub fn is_running(&self) -> bool {
        self.duration_seconds < 0
    }

    pub fn can_plan_jira_mutation(&self) -> bool {
        self.skip_reason != Some(SkipReason::RunningEntry)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TogglFetchResult {
    pub entries: Vec<TogglTimeEntry>,
    pub skipped: Vec<TogglFetchSkip>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TogglFetchSkip {
    pub workspace_id: String,
    pub entry_id: Option<String>,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    RunningEntry,
    PacingWindow,
}

pub struct TogglClient {
    http: reqwest::Client,
    base_url: Url,
    workspace_id: String,
    workspace_id_raw: i64,
    api_token: String,
    min_request_interval: Duration,
    initial_backfill_days: u32,
    last_request_at: Mutex<Option<std::time::Instant>>,
}

impl TogglClient {
    pub fn new(config: TogglClientConfig) -> TogglResult<Self> {
        if config.workspace_id <= 0 {
            return Err(TogglError::InvalidConfig(
                "toggl workspace_id must be greater than 0".to_owned(),
            ));
        }

        if !config.max_rps.is_finite() || config.max_rps <= 0.0 {
            return Err(TogglError::InvalidConfig(
                "toggl max_rps must be finite and greater than 0".to_owned(),
            ));
        }

        let base_url = Url::parse(&config.base_url).map_err(|error| {
            TogglError::InvalidConfig(format!("invalid Toggl base URL: {error}"))
        })?;
        validate_base_url(&base_url)?;

        Ok(Self {
            http: toggl_http_client(),
            base_url,
            workspace_id: config.workspace_id.to_string(),
            workspace_id_raw: config.workspace_id,
            api_token: config.api_token,
            min_request_interval: Duration::from_secs_f64(1.0 / config.max_rps),
            initial_backfill_days: config.initial_backfill_days,
            last_request_at: Mutex::new(None),
        })
    }

    pub async fn list_workspaces(
        base_url: &str,
        api_token: &str,
    ) -> TogglResult<Vec<TogglWorkspace>> {
        let base_url = Url::parse(base_url).map_err(|error| {
            TogglError::InvalidConfig(format!("invalid Toggl base URL: {error}"))
        })?;
        validate_base_url(&base_url)?;

        let url = base_url.join("/api/v9/me/workspaces").map_err(|error| {
            TogglError::InvalidConfig(format!("invalid Toggl workspaces URL: {error}"))
        })?;

        let raw_workspaces = toggl_http_client()
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .basic_auth(api_token, Some("api_token"))
            .send()
            .await?
            .error_for_status_with_body()
            .await?
            .json::<Vec<RawTogglWorkspace>>()
            .await?;

        Ok(raw_workspaces
            .into_iter()
            .map(|workspace| TogglWorkspace {
                id: workspace.id,
                name: workspace.name,
            })
            .collect())
    }

    pub async fn fetch_initial_backfill(
        &self,
        now_unix_seconds: i64,
    ) -> TogglResult<TogglFetchResult> {
        self.fetch_time_entries_since(self.initial_backfill_since(now_unix_seconds))
            .await
    }

    pub async fn fetch_bounded_backfill_since(
        &self,
        requested_since_unix_seconds: i64,
        now_unix_seconds: i64,
    ) -> TogglResult<TogglFetchResult> {
        self.fetch_time_entries_since(
            requested_since_unix_seconds.max(self.initial_backfill_since(now_unix_seconds)),
        )
        .await
    }

    pub async fn fetch_time_entries_since(
        &self,
        since_unix_seconds: i64,
    ) -> TogglResult<TogglFetchResult> {
        if self.should_skip_for_pacing()? {
            return Ok(TogglFetchResult {
                entries: Vec::new(),
                skipped: vec![TogglFetchSkip {
                    workspace_id: self.workspace_id.clone(),
                    entry_id: None,
                    reason: SkipReason::PacingWindow,
                }],
            });
        }

        let url = self
            .base_url
            .join("/api/v9/me/time_entries")
            .map_err(|error| {
                TogglError::InvalidConfig(format!("invalid Toggl time entries URL: {error}"))
            })?;
        let since = since_unix_seconds.to_string();

        let raw_entries = self
            .http
            .get(url)
            .query(&[("since", since.as_str())])
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .basic_auth(&self.api_token, Some("api_token"))
            .send()
            .await?
            .error_for_status_with_body()
            .await?
            .json::<Vec<RawTogglTimeEntry>>()
            .await?;

        Ok(normalize_entries(raw_entries, self.workspace_id_raw))
    }

    fn initial_backfill_since(&self, now_unix_seconds: i64) -> i64 {
        initial_backfill_since(now_unix_seconds, self.initial_backfill_days)
    }

    fn should_skip_for_pacing(&self) -> TogglResult<bool> {
        let now = std::time::Instant::now();
        let mut last_request_at = self
            .last_request_at
            .lock()
            .map_err(|_| TogglError::PacingStatePoisoned)?;

        if last_request_at.is_some_and(|last_request_at| {
            now.duration_since(last_request_at) < self.min_request_interval
        }) {
            return Ok(true);
        }

        *last_request_at = Some(now);
        Ok(false)
    }
}

impl fmt::Debug for TogglClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TogglClient")
            .field("base_url", &self.base_url)
            .field("workspace_id", &self.workspace_id)
            .field("api_token", &"<redacted>")
            .field("min_request_interval", &self.min_request_interval)
            .field("initial_backfill_days", &self.initial_backfill_days)
            .finish_non_exhaustive()
    }
}

fn validate_base_url(base_url: &Url) -> TogglResult<()> {
    if !base_url.username().is_empty() || base_url.password().is_some() {
        return Err(TogglError::InvalidConfig(
            "Toggl base URL must not contain embedded credentials".to_owned(),
        ));
    }

    if base_url.scheme() == "https" || is_localhost_url(base_url) {
        return Ok(());
    }

    Err(TogglError::InvalidConfig(
        "Toggl base URL must use https unless it is localhost".to_owned(),
    ))
}

fn is_localhost_url(base_url: &Url) -> bool {
    matches!(
        base_url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1")
    )
}

fn toggl_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .build()
        .expect("Toggl HTTP client config should be valid")
}

#[derive(Debug, Deserialize)]
struct RawTogglTimeEntry {
    id: i64,
    workspace_id: i64,
    description: Option<String>,
    start: String,
    duration: i64,
    #[serde(default)]
    updated_at: Option<String>,
    deleted_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTogglWorkspace {
    id: i64,
    name: String,
}

fn normalize_entries(
    raw_entries: Vec<RawTogglTimeEntry>,
    expected_workspace_id: i64,
) -> TogglFetchResult {
    let mut skipped = Vec::new();
    let entries = raw_entries
        .into_iter()
        .filter(|raw| raw.workspace_id == expected_workspace_id)
        .map(|raw| {
            let workspace_id = raw.workspace_id.to_string();
            let entry_id = raw.id.to_string();
            let skip_reason = (raw.duration < 0).then_some(SkipReason::RunningEntry);
            let updated_at = raw.updated_at.clone().unwrap_or_else(|| raw.start.clone());

            if let Some(reason) = skip_reason {
                skipped.push(TogglFetchSkip {
                    workspace_id: workspace_id.clone(),
                    entry_id: Some(entry_id.clone()),
                    reason,
                });
            }

            TogglTimeEntry {
                workspace_id,
                entry_id,
                start: raw.start,
                duration_seconds: raw.duration,
                description: raw.description,
                updated_at,
                deleted_at: raw.deleted_at,
                skip_reason,
            }
        })
        .collect();

    TogglFetchResult { entries, skipped }
}
