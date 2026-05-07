use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use httpmock::{Method, MockServer};
use serde_json::json;
use toggl_jira_sync::jira::{
    JiraClient, JiraError, JiraWritePacing, JiraWritePacingScope, MarkerVerification,
    TogglSyncMarker, WorklogDraft, MARKER_PROPERTY_KEY,
};

const EMAIL: &str = "sync@example.test";
const TOKEN: &str = "fake-jira-token-do-not-log";
const ISSUE_KEY: &str = "SAB-123";
const WORKLOG_ID: &str = "10001";

#[derive(Debug, Clone, Default)]
struct RecordingSleeper {
    sleeps: Arc<Mutex<Vec<Duration>>>,
}

impl RecordingSleeper {
    fn durations(&self) -> Vec<Duration> {
        self.sleeps.lock().expect("sleep lock").clone()
    }
}

impl toggl_jira_sync::jira::Sleeper for RecordingSleeper {
    fn sleep<'a>(
        &'a self,
        duration: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.sleeps.lock().expect("sleep lock").push(duration);
        })
    }
}

fn auth_header() -> String {
    format!("Basic {}", STANDARD.encode(format!("{EMAIL}:{TOKEN}")))
}

fn marker() -> TogglSyncMarker {
    TogglSyncMarker {
        toggl_workspace_id: 700001,
        toggl_entry_id: 900001,
        source_hash: "sha256:fixture".to_owned(),
        synced_at: "2024-05-02T03:06:40Z".to_owned(),
        tool_version: "0.1.0".to_owned(),
    }
}

fn draft(comment: &str) -> WorklogDraft {
    WorklogDraft {
        started: "2024-05-02T01:00:00.000+0000".to_owned(),
        time_spent_seconds: 1800,
        comment: comment.to_owned(),
    }
}

fn client(server: &MockServer) -> JiraClient<RecordingSleeper> {
    JiraClient::new(
        server.base_url(),
        EMAIL.to_owned(),
        TOKEN.to_owned(),
        RecordingSleeper::default(),
    )
}

#[tokio::test]
async fn jira_client_issue_exists_uses_read_only_issue_lookup() {
    let server = MockServer::start();
    let found = server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}"))
            .header("authorization", auth_header().as_str());
        then.status(200).json_body(json!({ "key": ISSUE_KEY }));
    });
    let missing = server.mock(|when, then| {
        when.method(Method::GET)
            .path("/rest/api/3/issue/SAB-404")
            .header("authorization", auth_header().as_str());
        then.status(404)
            .json_body(json!({ "errorMessages": ["Issue does not exist"] }));
    });
    let mutation = server.mock(|when, then| {
        when.any_request()
            .matches(|request| matches!(request.method.as_str(), "POST" | "PUT" | "DELETE"));
        then.status(500).body("issue_exists must be read-only");
    });

    let client = client(&server);

    assert!(client
        .issue_exists(ISSUE_KEY)
        .await
        .expect("200 should mean the issue exists"));
    assert!(!client
        .issue_exists("SAB-404")
        .await
        .expect("404 should mean the issue is absent"));
    found.assert();
    missing.assert();
    assert_eq!(mutation.hits(), 0);
}

#[tokio::test]
async fn jira_client_issue_exists_surfaces_permission_auth_and_rate_errors() {
    for (status, expected) in [
        (401, JiraError::AuthenticationFailed),
        (403, JiraError::PermissionDenied),
        (
            429,
            JiraError::RateLimited {
                retry_after: Duration::from_secs(1),
                message: String::new(),
            },
        ),
    ] {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(Method::GET)
                .path(format!("/rest/api/3/issue/{ISSUE_KEY}"));
            then.status(status)
                .json_body(json!({ "errorMessages": ["lookup rejected"] }));
        });

        let actual = client(&server)
            .issue_exists(ISSUE_KEY)
            .await
            .expect_err("non-404 lookup errors must be surfaced");

        assert_eq!(
            std::mem::discriminant(&actual),
            std::mem::discriminant(&expected)
        );
    }
}

#[tokio::test]
async fn jira_client_create_sets_toggl_property_and_uses_adf_comment() {
    let server = MockServer::start();

    let create = server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"))
            .header("authorization", auth_header().as_str())
            .json_body_obj(&json!({
                "started": "2024-05-02T01:00:00.000+0000",
                "timeSpentSeconds": 1800,
                "comment": {
                    "type": "doc",
                    "version": 1,
                    "content": [{
                        "type": "paragraph",
                        "content": [{ "type": "text", "text": "SAB-123 created entry" }]
                    }]
                }
            }));
        then.status(201).json_body(json!({
            "id": WORKLOG_ID,
            "issueId": "20001",
            "timeSpentSeconds": 1800,
            "started": "2024-05-02T01:00:00.000+0000"
        }));
    });
    let property = server.mock(|when, then| {
        when.method(Method::PUT)
            .path(format!(
                "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
            ))
            .header("authorization", auth_header().as_str())
            .json_body_obj(&marker());
        then.status(200).json_body(json!({}));
    });

    let worklog = client(&server)
        .create_worklog(ISSUE_KEY, &draft("SAB-123 created entry"), &marker())
        .await
        .expect("create should succeed");

    assert_eq!(worklog.id, WORKLOG_ID);
    create.assert();
    property.assert();
}

#[tokio::test]
async fn jira_client_create_writes_recoverable_comment_marker_when_property_set_fails() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(201).json_body(json!({
            "id": WORKLOG_ID,
            "issueId": "20001",
            "timeSpentSeconds": 1800,
            "started": "2024-05-02T01:00:00.000+0000"
        }));
    });
    server.mock(|when, then| {
        when.method(Method::PUT).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(400)
            .json_body(json!({"errorMessages":["Property write rejected"]}));
    });
    let fallback = server.mock(|when, then| {
        when.method(Method::PUT)
            .path(format!(
                "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}"
            ))
            .json_body_partial(
                r#"{"comment":{"content":[{"content":[{"text":"SAB-123 created entry [toggl-sync:workspace=700001;entry=900001]"}]}]}}"#,
            );
        then.status(200).json_body(json!({ "id": WORKLOG_ID }));
    });

    let worklog = client(&server)
        .create_worklog(ISSUE_KEY, &draft("SAB-123 created entry"), &marker())
        .await
        .expect("create should succeed with fallback marker when property write fails");

    assert_eq!(worklog.id, WORKLOG_ID);
    fallback.assert();
}

#[tokio::test]
async fn jira_client_property_set_and_read_round_trips_marker_shape() {
    let server = MockServer::start();
    let set = server.mock(|when, then| {
        when.method(Method::PUT)
            .path(format!(
                "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
            ))
            .json_body_obj(&marker());
        then.status(200).json_body(json!({}));
    });
    let read = server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200).json_body(json!({
            "key": MARKER_PROPERTY_KEY,
            "value": marker()
        }));
    });

    let client = client(&server);
    client
        .set_marker_property(ISSUE_KEY, WORKLOG_ID, &marker())
        .await
        .expect("property set should succeed");
    let actual = client
        .read_marker_property(ISSUE_KEY, WORKLOG_ID)
        .await
        .expect("property read should succeed");

    assert_eq!(actual, marker());
    set.assert();
    read.assert();
}

#[tokio::test]
async fn jira_client_update_and_delete_verify_marker_before_mutation() {
    let server = MockServer::start();
    let expected_marker = marker();
    let verify_marker = server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200)
            .json_body(json!({ "key": MARKER_PROPERTY_KEY, "value": expected_marker }));
    });
    let update = server.mock(|when, then| {
        when.method(Method::PUT)
            .path(format!(
                "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}"
            ))
            .json_body_partial(r#"{"timeSpentSeconds":900}"#);
        then.status(200).json_body(json!({ "id": WORKLOG_ID }));
    });
    let delete = server.mock(|when, then| {
        when.method(Method::DELETE).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}"
        ));
        then.status(204).body("");
    });

    let client = client(&server);
    client
        .update_marked_worklog(
            ISSUE_KEY,
            WORKLOG_ID,
            &draft("Updated from Toggl").with_seconds(900),
            &marker(),
        )
        .await
        .expect("marked update should succeed");
    client
        .delete_marked_worklog(ISSUE_KEY, WORKLOG_ID, &marker())
        .await
        .expect("marked delete should succeed");

    verify_marker.assert_hits(2);
    update.assert_hits(1);
    delete.assert_hits(1);
}

#[tokio::test]
async fn jira_client_refuses_to_mutate_unmarked_worklog() {
    let server = MockServer::start();
    let wrong_marker = TogglSyncMarker {
        toggl_entry_id: 42,
        ..marker()
    };
    server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200)
            .json_body(json!({ "key": MARKER_PROPERTY_KEY, "value": wrong_marker }));
    });
    let update = server.mock(|when, then| {
        when.method(Method::PUT).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}"
        ));
        then.status(200).json_body(json!({ "id": WORKLOG_ID }));
    });

    let error = client(&server)
        .update_marked_worklog(ISSUE_KEY, WORKLOG_ID, &draft("Do not touch"), &marker())
        .await
        .expect_err("unmarked worklog must not be updated");

    assert!(matches!(
        error,
        JiraError::MarkerVerificationFailed(MarkerVerification::Mismatch { .. })
    ));
    update.assert_hits(0);
}

#[tokio::test]
async fn jira_client_accepts_matching_comment_fallback_marker_when_property_is_missing() {
    let server = MockServer::start();
    let expected_marker = marker();
    let property_path = format!(
        "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
    );
    server.mock(|when, then| {
        when.method(Method::GET).path(property_path.as_str());
        then.status(404)
            .json_body(json!({"errorMessages":["Property does not exist"]}));
    });
    let list = server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({
            "worklogs": [{
                "id": WORKLOG_ID,
                "comment": {
                    "type": "doc",
                    "version": 1,
                    "content": [{
                        "type": "paragraph",
                        "content": [{
                            "type": "text",
                            "text": "Managed by Toggl [toggl-sync:workspace=700001;entry=900001]"
                        }]
                    }]
                }
            }]
        }));
    });
    let update = server.mock(|when, then| {
        when.method(Method::PUT)
            .path(format!(
                "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}"
            ))
            .json_body_partial(r#"{"timeSpentSeconds":900}"#);
        then.status(200).json_body(json!({ "id": WORKLOG_ID }));
    });
    let delete = server.mock(|when, then| {
        when.method(Method::DELETE).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}"
        ));
        then.status(204).body("");
    });

    let client = client(&server);
    client
        .update_marked_worklog(
            ISSUE_KEY,
            WORKLOG_ID,
            &draft("Updated from Toggl").with_seconds(900),
            &expected_marker,
        )
        .await
        .expect("matching fallback marker should allow update");
    client
        .delete_marked_worklog(ISSUE_KEY, WORKLOG_ID, &expected_marker)
        .await
        .expect("matching fallback marker should allow delete");

    list.assert_hits(2);
    update.assert_hits(1);
    delete.assert_hits(1);
}

#[tokio::test]
async fn jira_client_refuses_mismatched_comment_fallback_marker_when_property_is_missing() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(404)
            .json_body(json!({"errorMessages":["Property does not exist"]}));
    });
    server.mock(|when, then| {
        when.method(Method::GET)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(200).json_body(json!({
            "worklogs": [{
                "id": WORKLOG_ID,
                "comment": {
                    "type": "doc",
                    "version": 1,
                    "content": [{
                        "type": "paragraph",
                        "content": [{
                            "type": "text",
                            "text": "Managed by Toggl [toggl-sync:workspace=700001;entry=42]"
                        }]
                    }]
                }
            }]
        }));
    });
    let update = server.mock(|when, then| {
        when.method(Method::PUT).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}"
        ));
        then.status(200).json_body(json!({ "id": WORKLOG_ID }));
    });

    let error = client(&server)
        .update_marked_worklog(ISSUE_KEY, WORKLOG_ID, &draft("Do not touch"), &marker())
        .await
        .expect_err("mismatched fallback marker must not be updated");

    assert!(matches!(
        error,
        JiraError::MarkerVerificationFailed(MarkerVerification::Mismatch { .. })
    ));
    update.assert_hits(0);
}

#[tokio::test]
async fn jira_client_honors_retry_after() {
    let server = MockServer::start();
    let sleeper = RecordingSleeper::default();
    server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(429)
            .header("Retry-After", "2")
            .json_body(json!({ "errorMessages": ["Rate limit exceeded"] }));
    });

    let error = JiraClient::new(
        server.base_url(),
        EMAIL.to_owned(),
        TOKEN.to_owned(),
        sleeper.clone(),
    )
    .create_worklog(ISSUE_KEY, &draft("SAB-123 created entry"), &marker())
    .await
    .expect_err("429 should be surfaced after honoring Retry-After");

    assert!(
        matches!(error, JiraError::RateLimited { retry_after, .. } if retry_after == Duration::from_secs(2))
    );
    assert_eq!(
        sleeper.durations(),
        vec![Duration::from_secs(2), Duration::from_secs(2)]
    );
}

#[tokio::test]
async fn jira_client_enforces_global_and_same_issue_write_pacing() {
    let server = MockServer::start();
    let sleeper = RecordingSleeper::default();
    let pacing = JiraWritePacing {
        global_write_delay: Duration::from_millis(50),
        same_issue_write_delay: Duration::from_millis(200),
    };
    for (issue_key, worklog_id) in [(ISSUE_KEY, WORKLOG_ID), ("SAB-456", "10002")] {
        server.mock(|when, then| {
            when.method(Method::POST)
                .path(format!("/rest/api/3/issue/{issue_key}/worklog"));
            then.status(201).json_body(json!({ "id": worklog_id }));
        });
        server.mock(|when, then| {
            when.method(Method::PUT).path(format!(
                "/rest/api/3/issue/{issue_key}/worklog/{worklog_id}/properties/{MARKER_PROPERTY_KEY}"
            ));
            then.status(200).json_body(json!({}));
        });
    }

    let client = JiraClient::new_with_pacing(
        server.base_url(),
        EMAIL.to_owned(),
        TOKEN.to_owned(),
        sleeper.clone(),
        pacing,
    );
    client
        .create_worklog(ISSUE_KEY, &draft("SAB-123 created entry"), &marker())
        .await
        .expect("first create should succeed");
    client
        .create_worklog("SAB-456", &draft("SAB-456 created entry"), &marker())
        .await
        .expect("second create should succeed");

    assert_eq!(
        sleeper.durations(),
        vec![
            Duration::from_millis(200),
            Duration::from_millis(50),
            Duration::from_millis(200),
        ]
    );
}

#[tokio::test]
async fn jira_client_enforces_same_issue_pacing_for_interleaved_writes() {
    let server = MockServer::start();
    let sleeper = RecordingSleeper::default();
    let pacing = JiraWritePacing {
        global_write_delay: Duration::from_millis(50),
        same_issue_write_delay: Duration::from_millis(200),
    };
    for (issue_key, worklog_id) in [(ISSUE_KEY, WORKLOG_ID), ("SAB-456", "10002")] {
        server.mock(|when, then| {
            when.method(Method::PUT).path(format!(
                "/rest/api/3/issue/{issue_key}/worklog/{worklog_id}/properties/{MARKER_PROPERTY_KEY}"
            ));
            then.status(200).json_body(json!({}));
        });
    }

    let client = JiraClient::new_with_pacing(
        server.base_url(),
        EMAIL.to_owned(),
        TOKEN.to_owned(),
        sleeper.clone(),
        pacing,
    );
    client
        .set_marker_property(ISSUE_KEY, WORKLOG_ID, &marker())
        .await
        .expect("first A write should succeed");
    client
        .set_marker_property("SAB-456", "10002", &marker())
        .await
        .expect("B write should succeed");
    client
        .set_marker_property(ISSUE_KEY, WORKLOG_ID, &marker())
        .await
        .expect("second A write should succeed");

    assert_eq!(
        sleeper.durations(),
        vec![Duration::from_millis(50), Duration::from_millis(200)]
    );
}

#[tokio::test]
async fn jira_client_shares_global_pacing_across_clients_in_one_sync_scope() {
    let first_server = MockServer::start();
    let second_server = MockServer::start();
    let sleeper = RecordingSleeper::default();
    let pacing = JiraWritePacing {
        global_write_delay: Duration::from_millis(50),
        same_issue_write_delay: Duration::from_millis(200),
    };
    let scope = JiraWritePacingScope::default();
    first_server.mock(|when, then| {
        when.method(Method::PUT).path(format!(
            "/rest/api/3/issue/{ISSUE_KEY}/worklog/{WORKLOG_ID}/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200).json_body(json!({}));
    });
    second_server.mock(|when, then| {
        when.method(Method::PUT).path(format!(
            "/rest/api/3/issue/SAB-456/worklog/10002/properties/{MARKER_PROPERTY_KEY}"
        ));
        then.status(200).json_body(json!({}));
    });

    let first_client = JiraClient::new_with_pacing_scope(
        first_server.base_url(),
        EMAIL.to_owned(),
        TOKEN.to_owned(),
        sleeper.clone(),
        pacing,
        scope.clone(),
    );
    let second_client = JiraClient::new_with_pacing_scope(
        second_server.base_url(),
        EMAIL.to_owned(),
        TOKEN.to_owned(),
        sleeper.clone(),
        pacing,
        scope,
    );

    first_client
        .set_marker_property(ISSUE_KEY, WORKLOG_ID, &marker())
        .await
        .expect("first site write should succeed");
    second_client
        .set_marker_property("SAB-456", "10002", &marker())
        .await
        .expect("second site write should share pacing state");

    assert_eq!(sleeper.durations(), vec![Duration::from_millis(50)]);
}

#[tokio::test]
async fn jira_client_maps_common_jira_errors_to_typed_errors() {
    let cases = [
        (
            401,
            json!({"errorMessages":["Unauthorized"]}),
            JiraError::AuthenticationFailed,
        ),
        (
            403,
            json!({"errorMessages":["Forbidden"]}),
            JiraError::PermissionDenied,
        ),
        (
            404,
            json!({"errorMessages":["Issue does not exist"]}),
            JiraError::IssueNotFound,
        ),
        (
            400,
            json!({"errorMessages":["Time tracking is disabled"]}),
            JiraError::TimeTrackingDisabled,
        ),
        (
            400,
            json!({"errorMessages":["Field validation failed"]}),
            JiraError::ValidationFailed(String::new()),
        ),
    ];

    for (status, body, expected) in cases {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(Method::POST)
                .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
            then.status(status).json_body(body.clone());
        });

        let actual = client(&server)
            .create_worklog(ISSUE_KEY, &draft("SAB-123 created entry"), &marker())
            .await
            .expect_err("error status should map to typed error");

        assert_eq!(
            std::mem::discriminant(&actual),
            std::mem::discriminant(&expected)
        );
    }
}

#[tokio::test]
async fn jira_client_maps_captcha_auth_denial() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST)
            .path(format!("/rest/api/3/issue/{ISSUE_KEY}/worklog"));
        then.status(401)
            .header("X-Seraph-LoginReason", "AUTHENTICATION_DENIED")
            .json_body(json!({"errorMessages":["CAPTCHA required"]}));
    });

    let actual = client(&server)
        .create_worklog(ISSUE_KEY, &draft("SAB-123 created entry"), &marker())
        .await
        .expect_err("CAPTCHA denial should map to typed auth error");

    assert!(matches!(actual, JiraError::AuthenticationCaptchaRequired));
}

trait DraftTestExt {
    fn with_seconds(self, seconds: i64) -> Self;
}

impl DraftTestExt for WorklogDraft {
    fn with_seconds(mut self, seconds: i64) -> Self {
        self.time_spent_seconds = seconds;
        self
    }
}
