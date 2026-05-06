mod support;

use reqwest::StatusCode;
use rusqlite::Connection;
use support::{
    assert_local_mock_url, fixed_timestamp, fixture_text, jira_auth, temp_db, toggl_auth,
    FixtureName, JiraMockServer, TogglMockServer, FIXED_TIMESTAMP_RFC3339,
};

#[tokio::test]
async fn test_harness_toggl_auth_header() {
    let token = "fake-toggl-token-do-not-log";
    let server = TogglMockServer::start().await;
    let mock = server.mock_time_entries_since(toggl_auth(token), 1_714_604_800);

    assert_local_mock_url(server.base_url());

    let response = reqwest::Client::new()
        .get(format!(
            "{}/api/v9/me/time_entries?since=1714604800",
            server.base_url()
        ))
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .basic_auth(token, Some("api_token"))
        .send()
        .await
        .expect("mock Toggl request should complete");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.expect("body should be readable");
    assert!(body.contains("SAB-123 created entry"));
    assert!(!format!("{mock:?}").contains(token));
    mock.assert();
}

#[tokio::test]
async fn test_harness_jira_retry_after_fixture() {
    let credentials = jira_auth("sync@example.test", "fake-jira-token-do-not-log");
    let server = JiraMockServer::start().await;
    let mock = server.mock_create_worklog_rate_limited(credentials, "SAB-123");

    assert_local_mock_url(server.base_url());

    let response = reqwest::Client::new()
        .post(format!(
            "{}/rest/api/3/issue/SAB-123/worklog",
            server.base_url()
        ))
        .basic_auth("sync@example.test", Some("fake-jira-token-do-not-log"))
        .send()
        .await
        .expect("mock Jira request should complete");

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers().get("Retry-After").unwrap(), "2");
    let body = response.text().await.expect("body should be readable");
    assert!(body.contains("Rate limit exceeded"));
    mock.assert();
}

#[test]
fn test_harness_fixture_loader_covers_required_http_cases() {
    let toggl_entries = fixture_text(FixtureName::TogglTimeEntries);
    assert!(toggl_entries.contains("deleted_at"));
    assert!(toggl_entries.contains("\"duration\": -1"));
    assert!(toggl_entries.contains("SAB-123 created entry"));
    assert!(toggl_entries.contains("BLOGIC-456 updated entry"));
    assert!(toggl_entries.contains("SAB-789 deleted entry"));

    let jira_created = fixture_text(FixtureName::JiraWorklogCreated);
    assert!(jira_created.contains("\"id\": \"10001\""));

    let jira_updated = fixture_text(FixtureName::JiraWorklogUpdated);
    assert!(jira_updated.contains("\"id\": \"10001\""));
    assert!(jira_updated.contains("Updated from Toggl"));

    let jira_deleted = fixture_text(FixtureName::JiraWorklogDeleted);
    assert!(jira_deleted.trim().is_empty());

    let property_success = fixture_text(FixtureName::JiraPropertySuccess);
    assert!(property_success.contains("com.toggl_jira_sync"));

    let property_failure = fixture_text(FixtureName::JiraPropertyFailure);
    assert!(property_failure.contains("Property write rejected"));

    let issue_cases = fixture_text(FixtureName::IssueKeyCases);
    assert!(issue_cases.contains("ambiguous"));
    assert!(issue_cases.contains("missing"));
}

#[test]
fn test_harness_fixed_timestamp_is_deterministic() {
    let timestamp = fixed_timestamp();
    assert_eq!(timestamp.rfc3339, FIXED_TIMESTAMP_RFC3339);
    assert_eq!(timestamp.unix_seconds, 1_714_604_800);
}

#[test]
fn test_harness_temp_db_creates_isolated_sqlite_file() {
    let db = temp_db();
    assert!(db.path().starts_with(std::env::temp_dir()));
    assert!(db
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with(".sqlite"));

    let connection = Connection::open(db.path()).expect("temp db should be openable as sqlite");
    connection
        .execute("CREATE TABLE smoke (id INTEGER PRIMARY KEY)", [])
        .expect("temp db should accept schema setup");
}
