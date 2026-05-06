use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::{header::HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const MARKER_PROPERTY_KEY: &str = "com.toggl_jira_sync";

pub trait Sleeper: Clone + Send + Sync + 'static {
    fn sleep<'a>(&'a self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    fn sleep<'a>(&'a self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JiraWritePacing {
    pub global_write_delay: Duration,
    pub same_issue_write_delay: Duration,
}

#[derive(Debug, Clone, Default)]
struct JiraWritePacingState {
    has_written: bool,
    written_issue_keys: HashSet<String>,
}

#[derive(Debug, Clone, Default)]
pub struct JiraWritePacingScope {
    state: Arc<Mutex<JiraWritePacingState>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TogglSyncMarker {
    pub toggl_workspace_id: i64,
    pub toggl_entry_id: i64,
    pub source_hash: String,
    pub synced_at: String,
    pub tool_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorklogDraft {
    pub started: String,
    pub time_spent_seconds: i64,
    pub comment: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Worklog {
    pub id: String,
    #[serde(default, rename = "issueId")]
    pub issue_id: Option<String>,
    #[serde(default, rename = "timeSpentSeconds")]
    pub time_spent_seconds: Option<i64>,
    #[serde(default)]
    pub started: Option<String>,
    #[serde(default)]
    pub comment: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WorklogList {
    #[serde(default)]
    pub worklogs: Vec<Worklog>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerVerification {
    Missing,
    Mismatch {
        expected_workspace_id: i64,
        expected_entry_id: i64,
        actual_workspace_id: i64,
        actual_entry_id: i64,
    },
}

#[derive(Debug)]
pub enum JiraError {
    Http(reqwest::Error),
    Decode(serde_json::Error),
    AuthenticationFailed,
    AuthenticationCaptchaRequired,
    PermissionDenied,
    IssueNotFound,
    TimeTrackingDisabled,
    RateLimited {
        retry_after: Duration,
        message: String,
    },
    ValidationFailed(String),
    MarkerVerificationFailed(MarkerVerification),
}

impl fmt::Display for JiraError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(error) => write!(formatter, "Jira HTTP error: {error}"),
            Self::Decode(error) => write!(formatter, "Jira response decode error: {error}"),
            Self::AuthenticationFailed => formatter.write_str("Jira authentication failed"),
            Self::AuthenticationCaptchaRequired => {
                formatter.write_str("Jira authentication blocked by CAPTCHA")
            }
            Self::PermissionDenied => formatter.write_str("Jira permission denied"),
            Self::IssueNotFound => formatter.write_str("Jira issue or worklog not found"),
            Self::TimeTrackingDisabled => formatter.write_str("Jira time tracking is disabled"),
            Self::RateLimited {
                retry_after,
                message,
            } => {
                write!(
                    formatter,
                    "Jira rate limited for {}s: {message}",
                    retry_after.as_secs()
                )
            }
            Self::ValidationFailed(message) => {
                write!(formatter, "Jira validation failed: {message}")
            }
            Self::MarkerVerificationFailed(MarkerVerification::Missing) => {
                formatter.write_str("Jira worklog is missing Toggl sync marker")
            }
            Self::MarkerVerificationFailed(MarkerVerification::Mismatch { .. }) => {
                formatter.write_str("Jira worklog marker does not match requested Toggl entry")
            }
        }
    }
}

impl std::error::Error for JiraError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            Self::Decode(error) => Some(error),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for JiraError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

impl From<serde_json::Error> for JiraError {
    fn from(error: serde_json::Error) -> Self {
        Self::Decode(error)
    }
}

#[derive(Debug, Clone)]
pub struct JiraClient<S = TokioSleeper> {
    base_url: String,
    email: String,
    api_token: String,
    http: reqwest::Client,
    sleeper: S,
    write_pacing: JiraWritePacing,
    write_pacing_state: Arc<Mutex<JiraWritePacingState>>,
}

impl JiraClient<TokioSleeper> {
    pub fn from_credentials(base_url: String, email: String, api_token: String) -> Self {
        Self::new(base_url, email, api_token, TokioSleeper)
    }
}

impl<S: Sleeper> JiraClient<S> {
    pub fn new(base_url: String, email: String, api_token: String, sleeper: S) -> Self {
        Self::new_with_pacing(
            base_url,
            email,
            api_token,
            sleeper,
            JiraWritePacing::default(),
        )
    }

    pub fn new_with_pacing(
        base_url: String,
        email: String,
        api_token: String,
        sleeper: S,
        write_pacing: JiraWritePacing,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            email,
            api_token,
            http: reqwest::Client::new(),
            sleeper,
            write_pacing,
            write_pacing_state: JiraWritePacingScope::default().state,
        }
    }

    pub fn new_with_pacing_scope(
        base_url: String,
        email: String,
        api_token: String,
        sleeper: S,
        write_pacing: JiraWritePacing,
        write_pacing_scope: JiraWritePacingScope,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            email,
            api_token,
            http: reqwest::Client::new(),
            sleeper,
            write_pacing,
            write_pacing_state: write_pacing_scope.state,
        }
    }

    pub async fn create_worklog(
        &self,
        issue_key: &str,
        draft: &WorklogDraft,
        marker: &TogglSyncMarker,
    ) -> Result<Worklog, JiraError> {
        let body = worklog_body(draft);
        let url = self.issue_worklogs_url(issue_key);
        self.before_jira_write(issue_key).await;
        let response = self
            .send_with_retry(|| self.http.post(&url).json(&body))
            .await?;
        let worklog = decode_success::<Worklog>(response).await?;
        self.set_marker_property_or_comment_fallback(issue_key, &worklog.id, draft, marker)
            .await?;
        Ok(worklog)
    }

    pub async fn set_marker_property(
        &self,
        issue_key: &str,
        worklog_id: &str,
        marker: &TogglSyncMarker,
    ) -> Result<(), JiraError> {
        let url = self.property_url(issue_key, worklog_id);
        self.before_jira_write(issue_key).await;
        let response = self
            .send_with_retry(|| self.http.put(&url).json(marker))
            .await?;
        expect_empty_success(response).await
    }

    pub async fn set_marker_property_or_comment_fallback(
        &self,
        issue_key: &str,
        worklog_id: &str,
        draft: &WorklogDraft,
        marker: &TogglSyncMarker,
    ) -> Result<(), JiraError> {
        match self
            .set_marker_property(issue_key, worklog_id, marker)
            .await
        {
            Ok(()) => Ok(()),
            Err(JiraError::RateLimited {
                retry_after,
                message,
            }) => Err(JiraError::RateLimited {
                retry_after,
                message,
            }),
            Err(_) => {
                self.write_comment_fallback_marker(issue_key, worklog_id, draft, marker)
                    .await
            }
        }
    }

    pub async fn read_marker_property(
        &self,
        issue_key: &str,
        worklog_id: &str,
    ) -> Result<TogglSyncMarker, JiraError> {
        let url = self.property_url(issue_key, worklog_id);
        let response = self.send_with_retry(|| self.http.get(&url)).await?;
        let property = decode_success::<WorklogProperty>(response).await?;
        Ok(property.value)
    }

    pub async fn list_issue_worklogs(&self, issue_key: &str) -> Result<Vec<Worklog>, JiraError> {
        let url = self.issue_worklogs_url(issue_key);
        let response = self.send_with_retry(|| self.http.get(&url)).await?;
        Ok(decode_success::<WorklogList>(response).await?.worklogs)
    }

    pub async fn issue_exists(&self, issue_key: &str) -> Result<bool, JiraError> {
        let url = self.issue_url(issue_key);
        let response = self.send_with_retry(|| self.http.get(&url)).await?;
        match response.status() {
            StatusCode::OK => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            status if status.is_success() => Ok(true),
            status => {
                let headers = response.headers().clone();
                Err(map_error_response(
                    status,
                    &headers,
                    response_error_text(response).await,
                ))
            }
        }
    }

    pub async fn update_marked_worklog(
        &self,
        issue_key: &str,
        worklog_id: &str,
        draft: &WorklogDraft,
        expected_marker: &TogglSyncMarker,
    ) -> Result<Worklog, JiraError> {
        self.verify_marker(issue_key, worklog_id, expected_marker)
            .await?;
        let body = worklog_body(draft);
        let url = self.worklog_url(issue_key, worklog_id);
        self.before_jira_write(issue_key).await;
        let response = self
            .send_with_retry(|| self.http.put(&url).json(&body))
            .await?;
        decode_success(response).await
    }

    pub async fn delete_marked_worklog(
        &self,
        issue_key: &str,
        worklog_id: &str,
        expected_marker: &TogglSyncMarker,
    ) -> Result<(), JiraError> {
        self.verify_marker(issue_key, worklog_id, expected_marker)
            .await?;
        let url = self.worklog_url(issue_key, worklog_id);
        self.before_jira_write(issue_key).await;
        let response = self.send_with_retry(|| self.http.delete(&url)).await?;
        expect_empty_success(response).await
    }

    async fn write_comment_fallback_marker(
        &self,
        issue_key: &str,
        worklog_id: &str,
        draft: &WorklogDraft,
        marker: &TogglSyncMarker,
    ) -> Result<(), JiraError> {
        let mut fallback_draft = draft.clone();
        fallback_draft.comment = comment_with_fallback_marker(&draft.comment, marker);
        let body = worklog_body(&fallback_draft);
        let url = self.worklog_url(issue_key, worklog_id);
        self.before_jira_write(issue_key).await;
        let response = self
            .send_with_retry(|| self.http.put(&url).json(&body))
            .await?;
        decode_success::<Worklog>(response).await.map(|_| ())
    }

    async fn before_jira_write(&self, issue_key: &str) {
        let delay = {
            let mut state = self
                .write_pacing_state
                .lock()
                .expect("Jira write pacing state lock should not poison");
            let delay = if state.has_written {
                if state.written_issue_keys.contains(issue_key) {
                    self.write_pacing
                        .same_issue_write_delay
                        .max(self.write_pacing.global_write_delay)
                } else {
                    self.write_pacing.global_write_delay
                }
            } else {
                Duration::ZERO
            };
            state.has_written = true;
            state.written_issue_keys.insert(issue_key.to_owned());
            delay
        };

        if delay > Duration::ZERO {
            self.sleeper.sleep(delay).await;
        }
    }

    async fn verify_marker(
        &self,
        issue_key: &str,
        worklog_id: &str,
        expected_marker: &TogglSyncMarker,
    ) -> Result<(), JiraError> {
        let actual = match self.read_marker_property(issue_key, worklog_id).await {
            Ok(marker) => marker,
            Err(JiraError::IssueNotFound) => {
                self.read_comment_fallback_marker(issue_key, worklog_id)
                    .await?
            }
            Err(error) => return Err(error),
        };
        if actual.toggl_workspace_id == expected_marker.toggl_workspace_id
            && actual.toggl_entry_id == expected_marker.toggl_entry_id
        {
            Ok(())
        } else {
            Err(JiraError::MarkerVerificationFailed(
                MarkerVerification::Mismatch {
                    expected_workspace_id: expected_marker.toggl_workspace_id,
                    expected_entry_id: expected_marker.toggl_entry_id,
                    actual_workspace_id: actual.toggl_workspace_id,
                    actual_entry_id: actual.toggl_entry_id,
                },
            ))
        }
    }

    async fn read_comment_fallback_marker(
        &self,
        issue_key: &str,
        worklog_id: &str,
    ) -> Result<TogglSyncMarker, JiraError> {
        let worklog = self
            .list_issue_worklogs(issue_key)
            .await?
            .into_iter()
            .find(|worklog| worklog.id == worklog_id)
            .ok_or(JiraError::MarkerVerificationFailed(
                MarkerVerification::Missing,
            ))?;
        let (toggl_workspace_id, toggl_entry_id) = worklog
            .comment
            .as_ref()
            .and_then(parse_comment_fallback_marker)
            .ok_or(JiraError::MarkerVerificationFailed(
                MarkerVerification::Missing,
            ))?;

        Ok(TogglSyncMarker {
            toggl_workspace_id,
            toggl_entry_id,
            source_hash: String::new(),
            synced_at: String::new(),
            tool_version: String::new(),
        })
    }

    async fn send_with_retry(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, JiraError> {
        let response = build()
            .basic_auth(&self.email, Some(&self.api_token))
            .send()
            .await?;

        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = parse_retry_after(response.headers());
            let message = response_error_text(response).await;
            self.sleeper.sleep(retry_after).await;
            return Err(JiraError::RateLimited {
                retry_after,
                message,
            });
        }

        Ok(response)
    }

    fn issue_worklogs_url(&self, issue_key: &str) -> String {
        format!("{}/rest/api/3/issue/{issue_key}/worklog", self.base_url)
    }

    fn issue_url(&self, issue_key: &str) -> String {
        format!("{}/rest/api/3/issue/{issue_key}", self.base_url)
    }

    fn worklog_url(&self, issue_key: &str, worklog_id: &str) -> String {
        format!(
            "{}/rest/api/3/issue/{issue_key}/worklog/{worklog_id}",
            self.base_url
        )
    }

    fn property_url(&self, issue_key: &str, worklog_id: &str) -> String {
        format!(
            "{}/rest/api/3/issue/{issue_key}/worklog/{worklog_id}/properties/{MARKER_PROPERTY_KEY}",
            self.base_url
        )
    }
}

#[derive(Debug, Deserialize)]
struct WorklogProperty {
    value: TogglSyncMarker,
}

fn worklog_body(draft: &WorklogDraft) -> Value {
    json!({
        "started": draft.started,
        "timeSpentSeconds": draft.time_spent_seconds,
        "comment": adf_document(&draft.comment),
    })
}

fn comment_with_fallback_marker(comment: &str, marker: &TogglSyncMarker) -> String {
    let suffix = format!(
        "[toggl-sync:workspace={};entry={}]",
        marker.toggl_workspace_id, marker.toggl_entry_id
    );
    if comment.trim_end().ends_with(&suffix) {
        comment.to_owned()
    } else if comment.is_empty() {
        suffix
    } else {
        format!("{comment} {suffix}")
    }
}

fn parse_comment_fallback_marker(comment: &Value) -> Option<(i64, i64)> {
    let mut text = String::new();
    collect_comment_text(comment, &mut text);

    let marker_start = "[toggl-sync:workspace=";
    let entry_separator = ";entry=";
    let start = text.find(marker_start)? + marker_start.len();
    let after_workspace = &text[start..];
    let separator = after_workspace.find(entry_separator)?;
    let workspace_id = after_workspace[..separator].parse().ok()?;
    let after_entry = &after_workspace[separator + entry_separator.len()..];
    let end = after_entry.find(']')?;
    let entry_id = after_entry[..end].parse().ok()?;

    Some((workspace_id, entry_id))
}

fn collect_comment_text(value: &Value, output: &mut String) {
    match value {
        Value::String(text) => output.push_str(text),
        Value::Array(items) => {
            for item in items {
                collect_comment_text(item, output);
            }
        }
        Value::Object(fields) => {
            if let Some(Value::String(text)) = fields.get("text") {
                output.push_str(text);
            }
            for (key, value) in fields {
                if key != "text" {
                    collect_comment_text(value, output);
                }
            }
        }
        _ => {}
    }
}

fn adf_document(text: &str) -> Value {
    json!({
        "type": "doc",
        "version": 1,
        "content": [{
            "type": "paragraph",
            "content": [{
                "type": "text",
                "text": text,
            }]
        }]
    })
}

async fn decode_success<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T, JiraError> {
    let status = response.status();
    if status.is_success() {
        return response.json::<T>().await.map_err(JiraError::Http);
    }

    let headers = response.headers().clone();
    Err(map_error_response(
        status,
        &headers,
        response_error_text(response).await,
    ))
}

async fn expect_empty_success(response: reqwest::Response) -> Result<(), JiraError> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }

    let headers = response.headers().clone();
    Err(map_error_response(
        status,
        &headers,
        response_error_text(response).await,
    ))
}

fn map_error_response(status: StatusCode, headers: &HeaderMap, message: String) -> JiraError {
    match status {
        StatusCode::UNAUTHORIZED
            if headers
                .get("X-Seraph-LoginReason")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value == "AUTHENTICATION_DENIED") =>
        {
            JiraError::AuthenticationCaptchaRequired
        }
        StatusCode::UNAUTHORIZED => JiraError::AuthenticationFailed,
        StatusCode::FORBIDDEN => JiraError::PermissionDenied,
        StatusCode::NOT_FOUND => JiraError::IssueNotFound,
        StatusCode::BAD_REQUEST if message.to_ascii_lowercase().contains("time tracking") => {
            JiraError::TimeTrackingDisabled
        }
        StatusCode::TOO_MANY_REQUESTS => JiraError::RateLimited {
            retry_after: parse_retry_after(headers),
            message,
        },
        _ => JiraError::ValidationFailed(message),
    }
}

async fn response_error_text(response: reqwest::Response) -> String {
    response
        .text()
        .await
        .unwrap_or_else(|_| "<unreadable Jira error body>".to_owned())
}

fn parse_retry_after(headers: &HeaderMap) -> Duration {
    headers
        .get("Retry-After")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(1))
}
