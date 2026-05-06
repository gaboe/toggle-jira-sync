use std::fmt;
use std::path::Path;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use httpmock::{Method, Mock, MockServer};
use tempfile::{Builder, NamedTempFile};

pub const FIXED_TIMESTAMP_RFC3339: &str = "2024-05-02T03:06:40Z";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedTimestamp {
    pub rfc3339: &'static str,
    pub unix_seconds: i64,
}

#[derive(Clone, PartialEq, Eq)]
pub struct BasicAuth {
    header_value: String,
}

impl BasicAuth {
    fn new(username: &str, password: &str) -> Self {
        Self {
            header_value: format!(
                "Basic {}",
                STANDARD.encode(format!("{username}:{password}"))
            ),
        }
    }

    #[allow(dead_code)]
    pub fn header_value(&self) -> &str {
        &self.header_value
    }
}

impl fmt::Debug for BasicAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BasicAuth")
            .field("header_value", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureName {
    TogglTimeEntries,
    JiraWorklogCreated,
    JiraWorklogUpdated,
    JiraWorklogDeleted,
    JiraRateLimited,
    JiraPropertySuccess,
    JiraPropertyFailure,
    IssueKeyCases,
}

#[derive(Debug)]
pub struct TempDb {
    file: NamedTempFile,
}

impl TempDb {
    pub fn path(&self) -> &Path {
        self.file.path()
    }
}

pub struct TogglMockServer {
    server: MockServer,
}

pub struct JiraMockServer {
    server: MockServer,
}

pub struct ExpectedMock<'server> {
    mock: Mock<'server>,
    name: &'static str,
}

impl ExpectedMock<'_> {
    pub fn assert(&self) {
        self.mock.assert();
    }
}

impl fmt::Debug for ExpectedMock<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExpectedMock")
            .field("name", &self.name)
            .field("auth", &"<redacted>")
            .finish()
    }
}

impl TogglMockServer {
    pub async fn start() -> Self {
        Self {
            server: MockServer::start(),
        }
    }

    pub fn base_url(&self) -> String {
        self.server.base_url()
    }

    pub fn mock_time_entries_since(&self, auth: BasicAuth, since: i64) -> ExpectedMock<'_> {
        let since = since.to_string();
        let mock = self.server.mock(|when, then| {
            when.method(Method::GET)
                .path("/api/v9/me/time_entries")
                .query_param("since", since.as_str())
                .header("accept", "application/json")
                .header("content-type", "application/json")
                .header("authorization", auth.header_value.as_str());
            then.status(200)
                .header("content-type", "application/json")
                .body(fixture_text(FixtureName::TogglTimeEntries));
        });

        ExpectedMock {
            mock,
            name: "toggl_time_entries_since",
        }
    }
}

impl fmt::Debug for TogglMockServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TogglMockServer")
            .field("base_url", &self.base_url())
            .finish()
    }
}

impl JiraMockServer {
    pub async fn start() -> Self {
        Self {
            server: MockServer::start(),
        }
    }

    pub fn base_url(&self) -> String {
        self.server.base_url()
    }

    pub fn mock_create_worklog_rate_limited(
        &self,
        auth: BasicAuth,
        issue_key: &str,
    ) -> ExpectedMock<'_> {
        let path = format!("/rest/api/3/issue/{issue_key}/worklog");
        let mock = self.server.mock(|when, then| {
            when.method(Method::POST)
                .path(path.as_str())
                .header("authorization", auth.header_value.as_str());
            then.status(429)
                .header("content-type", "application/json")
                .header("Retry-After", "2")
                .body(fixture_text(FixtureName::JiraRateLimited));
        });

        ExpectedMock {
            mock,
            name: "jira_create_worklog_rate_limited",
        }
    }
}

impl fmt::Debug for JiraMockServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JiraMockServer")
            .field("base_url", &self.base_url())
            .finish()
    }
}

pub fn toggl_auth(api_token: &str) -> BasicAuth {
    BasicAuth::new(api_token, "api_token")
}

pub fn jira_auth(email: &str, api_token: &str) -> BasicAuth {
    BasicAuth::new(email, api_token)
}

pub fn fixture_text(name: FixtureName) -> &'static str {
    match name {
        FixtureName::TogglTimeEntries => include_str!("../fixtures/http/toggl_time_entries.json"),
        FixtureName::JiraWorklogCreated => {
            include_str!("../fixtures/http/jira_worklog_created.json")
        }
        FixtureName::JiraWorklogUpdated => {
            include_str!("../fixtures/http/jira_worklog_updated.json")
        }
        FixtureName::JiraWorklogDeleted => {
            include_str!("../fixtures/http/jira_worklog_deleted.json")
        }
        FixtureName::JiraRateLimited => include_str!("../fixtures/http/jira_429_retry_after.json"),
        FixtureName::JiraPropertySuccess => {
            include_str!("../fixtures/http/jira_property_success.json")
        }
        FixtureName::JiraPropertyFailure => {
            include_str!("../fixtures/http/jira_property_failure.json")
        }
        FixtureName::IssueKeyCases => include_str!("../fixtures/http/issue_key_cases.json"),
    }
}

pub fn fixed_timestamp() -> FixedTimestamp {
    FixedTimestamp {
        rfc3339: FIXED_TIMESTAMP_RFC3339,
        unix_seconds: 1_714_604_800,
    }
}

pub fn temp_db() -> TempDb {
    let file = Builder::new()
        .prefix("toggl-jira-sync-test-")
        .suffix(".sqlite")
        .tempfile()
        .expect("temp sqlite file should be creatable");

    TempDb { file }
}

pub fn assert_local_mock_url(url: impl AsRef<str>) {
    let url = url.as_ref();
    assert!(
        url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:"),
        "mock URL must stay on localhost, got {url}"
    );
}
