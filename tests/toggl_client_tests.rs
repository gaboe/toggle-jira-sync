#[allow(dead_code)]
mod support;

use httpmock::Method;
use toggl_jira_sync::{
    config::AppConfig,
    toggl::{SkipReason, TogglClient, TogglClientConfig, TogglFetchSkip, TogglWorkspace},
};

use support::{
    assert_local_mock_url, fixed_timestamp, fixture_text, toggl_auth, FixtureName, TogglMockServer,
};

fn client_config(base_url: String) -> TogglClientConfig {
    TogglClientConfig {
        base_url,
        workspace_id: 700001,
        api_token: "fake-toggl-token-do-not-log".to_owned(),
        max_rps: 1.0,
        initial_backfill_days: 90,
    }
}

#[tokio::test]
async fn toggl_client_lists_workspaces_for_setup_with_basic_auth() {
    let server = httpmock::MockServer::start();
    assert_local_mock_url(server.base_url());
    let mock = server.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/v9/me/workspaces")
            .header(
                "authorization",
                toggl_auth("fake-toggl-token-do-not-log").header_value(),
            );
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"[
                    {"id": 700001, "name": "Engineering"},
                    {"id": 700002, "name": "Operations"}
                ]"#,
            );
    });

    let workspaces =
        TogglClient::list_workspaces(&server.base_url(), "fake-toggl-token-do-not-log")
            .await
            .expect("mocked workspace discovery should succeed");

    assert_eq!(
        workspaces,
        vec![
            TogglWorkspace {
                id: 700001,
                name: "Engineering".to_owned(),
            },
            TogglWorkspace {
                id: 700002,
                name: "Operations".to_owned(),
            },
        ]
    );
    mock.assert();
}

#[tokio::test]
async fn toggl_client_since_includes_deleted_entries() {
    let since = fixed_timestamp().unix_seconds;
    let server = TogglMockServer::start().await;
    let mock = server.mock_time_entries_since(toggl_auth("fake-toggl-token-do-not-log"), since);
    let client =
        TogglClient::new(client_config(server.base_url())).expect("client config is valid");

    let result = client
        .fetch_time_entries_since(since)
        .await
        .expect("mocked Toggl request should succeed");

    assert_eq!(result.entries.len(), 4);
    let deleted = result
        .entries
        .iter()
        .find(|entry| entry.entry_id == "900003")
        .expect("deleted fixture entry should be returned to planner");
    assert_eq!(deleted.workspace_id, "700001");
    assert_eq!(deleted.start, "2024-05-01T10:00:00Z");
    assert_eq!(deleted.duration_seconds, 1200);
    assert_eq!(
        deleted.description.as_deref(),
        Some("SAB-789 deleted entry")
    );
    assert_eq!(deleted.updated_at, "2024-05-02T03:06:40Z");
    assert_eq!(deleted.deleted_at.as_deref(), Some("2024-05-02T03:06:40Z"));
    assert!(
        !result
            .skipped
            .iter()
            .any(|skip| skip.entry_id.as_deref() == Some("900003")),
        "deleted entries must be returned to the planner, not treated as skipped"
    );
    mock.assert();
}

#[tokio::test]
async fn toggl_client_marks_negative_duration_as_running() {
    let since = fixed_timestamp().unix_seconds;
    let server = TogglMockServer::start().await;
    let mock = server.mock_time_entries_since(toggl_auth("fake-toggl-token-do-not-log"), since);
    let client =
        TogglClient::new(client_config(server.base_url())).expect("client config is valid");

    let result = client
        .fetch_time_entries_since(since)
        .await
        .expect("mocked Toggl request should succeed");

    let running = result
        .entries
        .iter()
        .find(|entry| entry.entry_id == "900004")
        .expect("running fixture entry should be represented");
    assert!(running.is_running());
    assert!(!running.can_plan_jira_mutation());
    assert_eq!(running.skip_reason, Some(SkipReason::RunningEntry));
    assert_eq!(
        result.skipped,
        vec![TogglFetchSkip {
            workspace_id: "700001".to_owned(),
            entry_id: Some("900004".to_owned()),
            reason: SkipReason::RunningEntry,
        }]
    );
    mock.assert();
}

#[tokio::test]
async fn toggl_client_uses_basic_auth_token_style_and_epoch_since_seconds() {
    let since = fixed_timestamp().unix_seconds;
    let server = TogglMockServer::start().await;
    let mock = server.mock_time_entries_since(toggl_auth("fake-toggl-token-do-not-log"), since);
    let client =
        TogglClient::new(client_config(server.base_url())).expect("client config is valid");

    client
        .fetch_time_entries_since(since)
        .await
        .expect("mocked Toggl request should succeed");

    assert!(!format!("{client:?}").contains("fake-toggl-token-do-not-log"));
    mock.assert();
}

#[tokio::test]
async fn toggl_client_surfaces_error_response_body() {
    let server = httpmock::MockServer::start();
    assert_local_mock_url(server.base_url());
    let mock = server.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/v9/me/time_entries")
            .query_param("since", "1770199127")
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header(
                "authorization",
                toggl_auth("fake-toggl-token-do-not-log").header_value(),
            );
        then.status(400)
            .header("content-type", "application/json")
            .body(r#"{"message":"invalid since"}"#);
    });
    let client = TogglClient::new(client_config(server.base_url())).expect("client config");

    let error = client
        .fetch_time_entries_since(1_770_199_127)
        .await
        .expect_err("400 should surface as an error");

    let message = error.to_string();
    assert!(message.contains("400 Bad Request"), "{message}");
    assert!(message.contains("invalid since"), "{message}");
    mock.assert();
}

#[tokio::test]
async fn toggl_client_accepts_entries_without_updated_at() {
    let since = fixed_timestamp().unix_seconds;
    let server = httpmock::MockServer::start();
    assert_local_mock_url(server.base_url());
    let mock = server.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/v9/me/time_entries")
            .query_param("since", since.to_string().as_str())
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header(
                "authorization",
                toggl_auth("fake-toggl-token-do-not-log").header_value(),
            );
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"[{"id":900001,"workspace_id":700001,"description":"SAB-123","start":"2024-05-02T01:00:00Z","duration":1800}]"#);
    });
    let client = TogglClient::new(client_config(server.base_url())).expect("client config");

    let result = client
        .fetch_time_entries_since(since)
        .await
        .expect("entry without updated_at should parse");

    assert_eq!(result.entries[0].updated_at, "2024-05-02T01:00:00Z");
    mock.assert();
}

#[tokio::test]
async fn toggl_client_initial_backfill_since_uses_configured_days() {
    let config = AppConfig::from_toml_str(
        r#"
[toggl]
workspace_id = 700001
api_token_env = "TOGGL_API_TOKEN"

[runtime]
initial_backfill_days = 7

[jira]

[[jira.sites]]
key = "sabservis"
base_url = "https://sabservis.atlassian.net"
email_env = "SABSERVIS_JIRA_EMAIL"
api_token_env = "SABSERVIS_JIRA_API_TOKEN"
enabled = true
"#,
    )
    .expect("config should parse");
    let toggl_config = TogglClientConfig::from_app_config(
        &config,
        "fake-toggl-token-do-not-log".to_owned(),
        "http://127.0.0.1:12345".to_owned(),
    )
    .expect("client config should build from app config");

    assert_eq!(
        toggl_config.initial_backfill_since(fixed_timestamp().unix_seconds),
        1_714_521_600
    );
}

#[tokio::test]
async fn toggl_client_initial_backfill_starts_at_current_month_for_large_config_window() {
    let config = TogglClientConfig {
        initial_backfill_days: 90,
        ..client_config("http://127.0.0.1:12345".to_owned())
    };

    assert_eq!(
        config.initial_backfill_since(fixed_timestamp().unix_seconds),
        1_714_521_600
    );
}

#[tokio::test]
async fn toggl_client_initial_backfill_fetch_uses_bounded_since() {
    let now = fixed_timestamp().unix_seconds;
    let server = TogglMockServer::start().await;
    let expected_since = 1_714_521_600;
    let mock =
        server.mock_time_entries_since(toggl_auth("fake-toggl-token-do-not-log"), expected_since);
    let client =
        TogglClient::new(client_config(server.base_url())).expect("client config is valid");

    let result = client
        .fetch_initial_backfill(now)
        .await
        .expect("bounded initial backfill should fetch from configured lower bound");

    assert_eq!(result.entries.len(), 4);
    mock.assert();
}

#[tokio::test]
async fn toggl_client_bounded_backfill_never_fetches_before_configured_window() {
    let now = fixed_timestamp().unix_seconds;
    let requested_since = now - 365 * 24 * 60 * 60;
    let bounded_since = 1_714_521_600;
    let server = TogglMockServer::start().await;
    let mock =
        server.mock_time_entries_since(toggl_auth("fake-toggl-token-do-not-log"), bounded_since);
    let client =
        TogglClient::new(client_config(server.base_url())).expect("client config is valid");

    client
        .fetch_bounded_backfill_since(requested_since, now)
        .await
        .expect("bounded backfill should clamp old requests to configured lower bound");

    mock.assert();
}

#[tokio::test]
async fn toggl_client_filters_entries_from_other_workspaces() {
    let since = fixed_timestamp().unix_seconds;
    let server = httpmock::MockServer::start();
    assert_local_mock_url(server.base_url());
    let mock = server.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/v9/me/time_entries")
            .query_param("since", since.to_string().as_str());
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"[
                    {
                        "id": 900010,
                        "workspace_id": 700001,
                        "description": "SAB-100 included entry",
                        "start": "2024-05-02T01:00:00Z",
                        "duration": 1800,
                        "updated_at": "2024-05-02T03:06:40Z",
                        "deleted_at": null
                    },
                    {
                        "id": 900011,
                        "workspace_id": 999999,
                        "description": "SAB-999 wrong workspace",
                        "start": "2024-05-02T02:00:00Z",
                        "duration": -1,
                        "updated_at": "2024-05-02T03:06:40Z",
                        "deleted_at": null
                    }
                ]"#,
            );
    });
    let client =
        TogglClient::new(client_config(server.base_url())).expect("client config is valid");

    let result = client
        .fetch_time_entries_since(since)
        .await
        .expect("workspace-filtered fetch should succeed");

    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.entries[0].entry_id, "900010");
    assert!(result.skipped.is_empty());
    mock.assert();
}

#[test]
fn toggl_client_rejects_insecure_non_local_base_urls_before_sending_token() {
    let config = TogglClientConfig {
        base_url: "http://attacker.example.test".to_owned(),
        workspace_id: 700001,
        api_token: "fake-toggl-token-do-not-log".to_owned(),
        max_rps: 1.0,
        initial_backfill_days: 90,
    };

    let error = TogglClient::new(config).expect_err("insecure non-local URL should be rejected");

    assert!(error
        .to_string()
        .contains("Toggl base URL must use https unless it is localhost"));
}

#[test]
fn toggl_client_rejects_base_urls_with_embedded_credentials() {
    let config = TogglClientConfig {
        base_url: "https://user:password@api.track.toggl.com".to_owned(),
        workspace_id: 700001,
        api_token: "fake-toggl-token-do-not-log".to_owned(),
        max_rps: 1.0,
        initial_backfill_days: 90,
    };

    let error = TogglClient::new(config).expect_err("URL credentials should be rejected");

    assert!(error
        .to_string()
        .contains("Toggl base URL must not contain embedded credentials"));
}

#[tokio::test]
async fn toggl_client_skips_immediate_second_poll_to_stay_quota_friendly() {
    let since = fixed_timestamp().unix_seconds;
    let server = httpmock::MockServer::start();
    assert_local_mock_url(server.base_url());
    let mock = server.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/v9/me/time_entries")
            .query_param("since", since.to_string().as_str());
        then.status(200)
            .header("content-type", "application/json")
            .body(fixture_text(FixtureName::TogglTimeEntries));
    });
    let client =
        TogglClient::new(client_config(server.base_url())).expect("client config is valid");

    let first = client
        .fetch_time_entries_since(since)
        .await
        .expect("first request should hit mock server");
    let second = client
        .fetch_time_entries_since(since + 1)
        .await
        .expect("second immediate request should be skipped locally");

    assert_eq!(first.entries.len(), 4);
    assert!(second.entries.is_empty());
    assert_eq!(
        second.skipped,
        vec![TogglFetchSkip {
            workspace_id: "700001".to_owned(),
            entry_id: None,
            reason: SkipReason::PacingWindow,
        }]
    );
    assert_eq!(
        mock.hits(),
        1,
        "second poll must not call Toggl /me/* above configured max_rps"
    );
}
