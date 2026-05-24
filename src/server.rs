use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::{
    app::{
        self, AppStateSnapshot, ConfigOnlySnapshot, ConfigSnapshot, ConfigUpdate,
        DeleteLocalDataResult, ExportConfigResult, ScheduleSnapshot,
    },
    cli::{ServerArgs, ServerMode, SharedPaths},
    format_error_chain,
    report::StatusReport,
};

#[derive(Clone)]
pub struct ServerState {
    paths: SharedPaths,
    credentials_path: Option<PathBuf>,
    limit: usize,
    tenant_store: Option<TenantStore>,
}

#[derive(Clone)]
struct TenantStore {
    db_path: PathBuf,
}

#[derive(Debug, Clone)]
struct TenantRecord {
    tenant_id: String,
    config_path: PathBuf,
    credentials_path: PathBuf,
    db_path: PathBuf,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Deserialize)]
struct ScheduleUpdate {
    enabled: bool,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug, Serialize)]
struct TenantIdentity {
    tenant_id: String,
}

pub async fn run(args: ServerArgs) -> anyhow::Result<()> {
    let listener = bind(&args.host, args.port).await?;
    let local_addr = listener
        .local_addr()
        .context("failed to read server address")?;
    eprintln!("toggl-jira-sync server listening on http://{local_addr}");
    serve_listener(
        listener,
        args.paths,
        args.credentials,
        args.limit,
        args.mode,
        args.tenant_db,
    )
    .await
}

pub async fn bind(host: &str, port: u16) -> anyhow::Result<TcpListener> {
    TcpListener::bind(format!("{host}:{port}"))
        .await
        .with_context(|| format!("failed to bind server to {host}:{port}"))
}

pub async fn serve_listener(
    listener: TcpListener,
    paths: SharedPaths,
    credentials_path: Option<PathBuf>,
    limit: usize,
    mode: ServerMode,
    tenant_db: Option<PathBuf>,
) -> anyhow::Result<()> {
    axum::serve(
        listener,
        router(paths, credentials_path, limit, mode, tenant_db)?,
    )
    .await
    .context("server failed")
}

pub fn router(
    paths: SharedPaths,
    credentials_path: Option<PathBuf>,
    limit: usize,
    mode: ServerMode,
    tenant_db: Option<PathBuf>,
) -> anyhow::Result<Router> {
    let tenant_store = match mode {
        ServerMode::Single => None,
        ServerMode::Multi => Some(TenantStore::open(
            tenant_db.context("--tenant-db is required in multi mode")?,
        )?),
    };
    let state = Arc::new(ServerState {
        paths,
        credentials_path,
        limit,
        tenant_store,
    });
    let router = Router::new().route("/healthz", get(healthz));
    let router = match mode {
        ServerMode::Single => router
            .route("/api/snapshot", get(snapshot))
            .route("/api/config", get(config_snapshot).put(save_config))
            .route("/api/status", get(status))
            .route("/api/sync/dry-run", post(dry_run))
            .route("/api/sync", post(sync))
            .route("/api/schedule", patch(update_schedule))
            .route("/api/local-data", delete(delete_local_data))
            .route("/api/config/export", post(export_config)),
        ServerMode::Multi => router
            .route("/api/me", get(me))
            .route("/api/tenants/:tenant_id/snapshot", get(tenant_snapshot))
            .route("/api/tenants/:tenant_id/config", get(tenant_config))
            .route("/api/tenants/:tenant_id/status", get(tenant_status))
            .route("/api/tenants/:tenant_id/sync/dry-run", post(tenant_dry_run))
            .route("/api/tenants/:tenant_id/sync", post(tenant_sync)),
    };
    Ok(router.with_state(state).layer(cors_layer()))
}

fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _| {
            origin.to_str().ok().is_some_and(|origin| {
                origin.starts_with("http://127.0.0.1:")
                    || origin.starts_with("http://localhost:")
                    || std::env::var("TJS_ALLOWED_ORIGINS")
                        .ok()
                        .is_some_and(|origins| {
                            origins.split(',').any(|allowed| allowed.trim() == origin)
                        })
            })
        }))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
        ])
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn me(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> ApiResult<TenantIdentity> {
    let tenant = authenticated_tenant(&state, &headers, None)?;
    Ok(Json(TenantIdentity {
        tenant_id: tenant.tenant_id,
    }))
}

async fn snapshot(State(state): State<Arc<ServerState>>) -> ApiResult<AppStateSnapshot> {
    app::snapshot_with_credentials(
        state.paths.clone(),
        state.limit,
        state.credentials_path.clone(),
    )
    .map(|snapshot| Json(snapshot.redacted()))
    .map_err(ApiError::from)
}

async fn config_snapshot(State(state): State<Arc<ServerState>>) -> ApiResult<ConfigOnlySnapshot> {
    app::config_snapshot_with_credentials(state.paths.clone(), state.credentials_path.clone())
        .map(|snapshot| Json(snapshot.redacted()))
        .map_err(ApiError::from)
}

async fn status(State(state): State<Arc<ServerState>>) -> ApiResult<StatusReport> {
    app::status_report(state.paths.clone(), state.limit)
        .map(|(_, _, report)| Json(report))
        .map_err(ApiError::from)
}

async fn dry_run(State(state): State<Arc<ServerState>>) -> ApiResult<AppStateSnapshot> {
    run_sync_off_thread(
        state.paths.clone(),
        state.credentials_path.clone(),
        true,
        false,
    )
    .await
    .map_err(ApiError::from)?;
    app::snapshot_with_credentials(
        state.paths.clone(),
        state.limit,
        state.credentials_path.clone(),
    )
    .map(|snapshot| Json(snapshot.redacted()))
    .map_err(ApiError::from)
}

async fn sync(State(state): State<Arc<ServerState>>) -> ApiResult<AppStateSnapshot> {
    run_sync_off_thread(
        state.paths.clone(),
        state.credentials_path.clone(),
        false,
        true,
    )
    .await
    .map_err(ApiError::from)?;
    app::snapshot_with_credentials(
        state.paths.clone(),
        state.limit,
        state.credentials_path.clone(),
    )
    .map(|snapshot| Json(snapshot.redacted()))
    .map_err(ApiError::from)
}

async fn run_sync_off_thread(
    paths: SharedPaths,
    credentials_path: Option<PathBuf>,
    dry_run: bool,
    cleanup_deleted: bool,
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to start sync runtime")?
            .block_on(async move {
                if let Some(credentials_path) = credentials_path {
                    app::run_sync_with_isolated_credentials(
                        paths,
                        dry_run,
                        cleanup_deleted,
                        credentials_path,
                    )
                    .await
                } else {
                    app::run_sync_with_credentials(paths, dry_run, cleanup_deleted, None).await
                }
            })
    })
    .await
    .context("sync task failed")?
}

async fn update_schedule(
    State(state): State<Arc<ServerState>>,
    Json(update): Json<ScheduleUpdate>,
) -> ApiResult<ScheduleSnapshot> {
    app::update_schedule(state.paths.clone(), update.enabled)
        .map(Json)
        .map_err(ApiError::from)
}

async fn save_config(
    State(state): State<Arc<ServerState>>,
    Json(update): Json<ConfigUpdate>,
) -> ApiResult<ConfigSnapshot> {
    app::save_config_with_credentials(state.paths.clone(), update, state.credentials_path.clone())
        .map(|mut snapshot| {
            snapshot.redact_secrets();
            Json(snapshot)
        })
        .map_err(ApiError::from)
}

async fn delete_local_data(
    State(state): State<Arc<ServerState>>,
) -> ApiResult<DeleteLocalDataResult> {
    app::delete_local_data(state.paths.clone())
        .map(Json)
        .map_err(ApiError::from)
}

async fn export_config(State(state): State<Arc<ServerState>>) -> ApiResult<ExportConfigResult> {
    app::export_config(state.paths.clone())
        .map(Json)
        .map_err(ApiError::from)
}

async fn tenant_snapshot(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(tenant_id): Path<String>,
) -> ApiResult<AppStateSnapshot> {
    let tenant = authenticated_tenant(&state, &headers, Some(&tenant_id))?;
    app::snapshot_with_credentials(
        tenant_paths(&tenant),
        state.limit,
        Some(tenant.credentials_path),
    )
    .map(|snapshot| Json(snapshot.redacted()))
    .map_err(ApiError::from)
}

async fn tenant_config(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(tenant_id): Path<String>,
) -> ApiResult<ConfigOnlySnapshot> {
    let tenant = authenticated_tenant(&state, &headers, Some(&tenant_id))?;
    app::config_snapshot_with_credentials(tenant_paths(&tenant), Some(tenant.credentials_path))
        .map(|snapshot| Json(snapshot.redacted()))
        .map_err(ApiError::from)
}

async fn tenant_status(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(tenant_id): Path<String>,
) -> ApiResult<StatusReport> {
    let tenant = authenticated_tenant(&state, &headers, Some(&tenant_id))?;
    app::status_report(tenant_paths(&tenant), state.limit)
        .map(|(_, _, report)| Json(report))
        .map_err(ApiError::from)
}

async fn tenant_dry_run(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(tenant_id): Path<String>,
) -> ApiResult<AppStateSnapshot> {
    let tenant = authenticated_tenant(&state, &headers, Some(&tenant_id))?;
    let paths = tenant_paths(&tenant);
    run_sync_off_thread(
        paths.clone(),
        Some(tenant.credentials_path.clone()),
        true,
        false,
    )
    .await
    .map_err(ApiError::from)?;
    app::snapshot_with_credentials(paths, state.limit, Some(tenant.credentials_path))
        .map(|snapshot| Json(snapshot.redacted()))
        .map_err(ApiError::from)
}

async fn tenant_sync(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(tenant_id): Path<String>,
) -> ApiResult<AppStateSnapshot> {
    let tenant = authenticated_tenant(&state, &headers, Some(&tenant_id))?;
    let paths = tenant_paths(&tenant);
    run_sync_off_thread(
        paths.clone(),
        Some(tenant.credentials_path.clone()),
        false,
        true,
    )
    .await
    .map_err(ApiError::from)?;
    app::snapshot_with_credentials(paths, state.limit, Some(tenant.credentials_path))
        .map(|snapshot| Json(snapshot.redacted()))
        .map_err(ApiError::from)
}

fn tenant_paths(tenant: &TenantRecord) -> SharedPaths {
    SharedPaths {
        config: Some(tenant.config_path.clone()),
        db: Some(tenant.db_path.clone()),
    }
}

fn authenticated_tenant(
    state: &ServerState,
    headers: &HeaderMap,
    tenant_id: Option<&str>,
) -> Result<TenantRecord, ApiError> {
    let token =
        bearer_token(headers).ok_or_else(|| ApiError::unauthorized("missing bearer token"))?;
    let store = state
        .tenant_store
        .as_ref()
        .ok_or_else(|| ApiError::unauthorized("multi-tenant store is not configured"))?;
    let tenant = store
        .resolve_token(token)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::unauthorized("invalid bearer token"))?;
    if tenant_id.is_some_and(|expected| expected != tenant.tenant_id) {
        return Err(ApiError::forbidden("token does not match tenant"));
    }
    Ok(tenant)
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

type ApiResult<T> = Result<Json<T>, ApiError>;

struct ApiError {
    status: StatusCode,
    error: anyhow::Error,
}

impl ApiError {
    fn unauthorized(message: &'static str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            error: anyhow::anyhow!(message),
        }
    }

    fn forbidden(message: &'static str) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            error: anyhow::anyhow!(message),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: format_error_chain(&self.error),
            }),
        )
            .into_response()
    }
}

impl TenantStore {
    fn open(db_path: PathBuf) -> anyhow::Result<Self> {
        let store = Self { db_path };
        store.run_migrations()?;
        Ok(store)
    }

    fn run_migrations(&self) -> anyhow::Result<()> {
        let connection = rusqlite::Connection::open(&self.db_path)
            .with_context(|| format!("failed to open tenant DB {}", self.db_path.display()))?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS server_tenants (
                tenant_id TEXT PRIMARY KEY NOT NULL,
                slug TEXT UNIQUE NOT NULL,
                display_name TEXT NOT NULL,
                config_path TEXT NOT NULL,
                credentials_path TEXT NOT NULL,
                db_path TEXT NOT NULL,
                disabled_at TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );
            CREATE TABLE IF NOT EXISTS tenant_api_tokens (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                tenant_id TEXT NOT NULL REFERENCES server_tenants(tenant_id),
                token_hash TEXT UNIQUE NOT NULL,
                token_label TEXT NOT NULL,
                revoked_at TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );",
        )?;
        Ok(())
    }

    fn resolve_token(&self, token: &str) -> anyhow::Result<Option<TenantRecord>> {
        let connection = rusqlite::Connection::open(&self.db_path)
            .with_context(|| format!("failed to open tenant DB {}", self.db_path.display()))?;
        let token_hash = token_hash(token);
        let mut statement = connection.prepare(
            "SELECT tenant.tenant_id, tenant.config_path, tenant.credentials_path, tenant.db_path
             FROM tenant_api_tokens token
             INNER JOIN server_tenants tenant ON tenant.tenant_id = token.tenant_id
             WHERE token.token_hash = ?1
               AND token.revoked_at IS NULL
               AND tenant.disabled_at IS NULL",
        )?;
        let result = statement.query_row([token_hash], |row| {
            Ok(TenantRecord {
                tenant_id: row.get(0)?,
                config_path: PathBuf::from(row.get::<_, String>(1)?),
                credentials_path: PathBuf::from(row.get::<_, String>(2)?),
                db_path: PathBuf::from(row.get::<_, String>(3)?),
            })
        });
        match result {
            Ok(tenant) => Ok(Some(tenant)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

fn token_hash(token: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode};
    use http_body_util::BodyExt;
    use rusqlite::params;
    use tower::util::ServiceExt;

    #[tokio::test]
    async fn health_route_returns_ok() {
        let response = single_router()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn multi_mode_does_not_expose_unscoped_snapshot() {
        let tenant_db = tempfile::NamedTempFile::new().expect("tenant db");

        let response = router(
            SharedPaths {
                config: None,
                db: None,
            },
            None,
            200,
            ServerMode::Multi,
            Some(tenant_db.path().to_path_buf()),
        )
        .expect("router")
        .oneshot(
            Request::builder()
                .uri("/api/snapshot")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn multi_mode_requires_matching_tenant_token() {
        let tenant_db = tempfile::NamedTempFile::new().expect("tenant db");
        let store = TenantStore::open(tenant_db.path().to_path_buf()).expect("tenant store");
        insert_tenant(&store, "tenant-a", "token-a");
        insert_tenant(&store, "tenant-b", "token-b");

        let missing_token = multi_router(tenant_db.path().to_path_buf())
            .oneshot(
                Request::builder()
                    .uri("/api/tenants/tenant-a/status")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(missing_token.status(), StatusCode::UNAUTHORIZED);

        let wrong_tenant = multi_router(tenant_db.path().to_path_buf())
            .oneshot(
                Request::builder()
                    .uri("/api/tenants/tenant-b/status")
                    .header(header::AUTHORIZATION, "Bearer token-a")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(wrong_tenant.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn server_config_response_redacts_secret_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let credentials_path = dir.path().join("credentials.env");
        std::fs::write(
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
        .expect("config");
        std::fs::write(
            &credentials_path,
            "TOGGL_API_TOKEN=toggl-secret\nACME_JIRA_EMAIL=dev@example.com\nACME_JIRA_API_TOKEN=jira-secret\n",
        )
        .expect("credentials");

        let response = router(
            SharedPaths {
                config: Some(config_path),
                db: None,
            },
            Some(credentials_path),
            200,
            ServerMode::Single,
            None,
        )
        .expect("router")
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json["config"]["toggl_api_token_present"], true);
        assert_eq!(
            json["config"]["toggl_api_token_value"],
            serde_json::Value::Null
        );
        assert_eq!(json["config"]["jira_sites"][0]["email_present"], true);
        assert_eq!(
            json["config"]["jira_sites"][0]["email_value"],
            serde_json::Value::Null
        );
        assert_eq!(json["config"]["jira_sites"][0]["api_token_present"], true);
        assert_eq!(
            json["config"]["jira_sites"][0]["api_token_value"],
            serde_json::Value::Null
        );
    }

    #[tokio::test]
    async fn server_snapshot_response_redacts_secret_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let credentials_path = dir.path().join("credentials.env");
        let db_path = dir.path().join("ledger.sqlite");
        write_test_config(&config_path, &db_path, "https://api.track.toggl.com");
        std::fs::write(
            &credentials_path,
            "TOGGL_API_TOKEN=toggl-secret\nACME_JIRA_EMAIL=dev@example.com\nACME_JIRA_API_TOKEN=jira-secret\n",
        )
        .expect("credentials");

        let response = router(
            SharedPaths {
                config: Some(config_path),
                db: Some(db_path),
            },
            Some(credentials_path),
            200,
            ServerMode::Single,
            None,
        )
        .expect("router")
        .oneshot(
            Request::builder()
                .uri("/api/snapshot")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json["config"]["toggl_api_token_present"], true);
        assert_eq!(
            json["config"]["toggl_api_token_value"],
            serde_json::Value::Null
        );
        assert_eq!(json["config"]["jira_sites"][0]["email_present"], true);
        assert_eq!(
            json["config"]["jira_sites"][0]["email_value"],
            serde_json::Value::Null
        );
        assert_eq!(json["config"]["jira_sites"][0]["api_token_present"], true);
        assert_eq!(
            json["config"]["jira_sites"][0]["api_token_value"],
            serde_json::Value::Null
        );
    }

    #[tokio::test]
    async fn tenant_dry_run_uses_tenant_credentials_file() {
        assert_tenant_sync_uses_tenant_credentials_file("/api/tenants/tenant-a/sync/dry-run").await;
    }

    #[tokio::test]
    async fn tenant_sync_uses_tenant_credentials_file() {
        assert_tenant_sync_uses_tenant_credentials_file("/api/tenants/tenant-a/sync").await;
    }

    async fn assert_tenant_sync_uses_tenant_credentials_file(route: &'static str) {
        let dir = tempfile::tempdir().expect("tempdir");
        let tenant_db = dir.path().join("tenants.sqlite");
        let config_path = dir.path().join("config.toml");
        let credentials_path = dir.path().join("credentials.env");
        let sync_db_path = dir.path().join("sync.sqlite");
        write_test_config(&config_path, &sync_db_path, "http://127.0.0.1:9");
        std::fs::write(&credentials_path, "TOGGL_API_TOKEN=tenant-token\n").expect("credentials");
        let store = TenantStore::open(tenant_db.clone()).expect("tenant store");
        insert_tenant_with_paths(
            &store,
            "tenant-a",
            "token-a",
            &config_path,
            &credentials_path,
            &sync_db_path,
        );

        let response = multi_router(tenant_db)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(route)
                    .header(header::AUTHORIZATION, "Bearer token-a")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");

        let error = json["error"].as_str().expect("error message");
        assert!(error.contains("failed to fetch Toggl entries"), "{error}");
        assert!(
            !error.contains("missing env var TOGGL_API_TOKEN"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn cors_allows_loopback_browser_origins() {
        assert_cors_origin("http://127.0.0.1:5174").await;
        assert_cors_origin("http://localhost:5174").await;
    }

    #[tokio::test]
    async fn cors_allows_configured_saas_origin() {
        std::env::set_var("TJS_ALLOWED_ORIGINS", "https://tjs.example.com");
        assert_cors_origin("https://tjs.example.com").await;
        std::env::remove_var("TJS_ALLOWED_ORIGINS");
    }

    async fn assert_cors_origin(origin: &'static str) {
        let response = single_router()
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/config")
                    .header(header::ORIGIN, origin)
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static(origin))
        );
    }

    #[tokio::test]
    async fn save_config_uses_explicit_credentials_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let credentials_path = dir.path().join("credentials.env");
        let db_path = dir.path().join("sync.sqlite");
        write_test_config(&config_path, &db_path, "https://api.track.toggl.com");

        let update = serde_json::json!({
            "toggl_workspace_id": 123,
            "toggl_api_token_env": "TOGGL_API_TOKEN",
            "toggl_api_token_value": "saved-token",
            "sqlite_path": db_path.display().to_string(),
            "initial_backfill_from_month": null,
            "initial_backfill_days": 90,
            "recovery_from_month": null,
            "recovery_scan_days": 180,
            "schedule_enabled": true,
            "schedule_interval_minutes": 60,
            "jira_sites": [{
                "key": "acme",
                "base_url": "https://acme.atlassian.net",
                "email_env": "ACME_JIRA_EMAIL",
                "email_value": "dev@example.com",
                "api_token_env": "ACME_JIRA_API_TOKEN",
                "api_token_value": "jira-token",
                "enabled": true
            }]
        });

        let response = router(
            SharedPaths {
                config: Some(config_path),
                db: Some(db_path),
            },
            Some(credentials_path.clone()),
            200,
            ServerMode::Single,
            None,
        )
        .expect("router")
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(update.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let credentials = std::fs::read_to_string(credentials_path).expect("credentials");
        assert!(
            credentials.contains("TOGGL_API_TOKEN=saved-token"),
            "{credentials}"
        );
        assert!(
            credentials.contains("ACME_JIRA_EMAIL=dev@example.com"),
            "{credentials}"
        );
        assert!(
            credentials.contains("ACME_JIRA_API_TOKEN=jira-token"),
            "{credentials}"
        );
    }

    fn single_router() -> Router {
        router(
            SharedPaths {
                config: None,
                db: None,
            },
            None,
            200,
            ServerMode::Single,
            None,
        )
        .expect("router")
    }

    fn multi_router(tenant_db: PathBuf) -> Router {
        router(
            SharedPaths {
                config: None,
                db: None,
            },
            None,
            200,
            ServerMode::Multi,
            Some(tenant_db),
        )
        .expect("router")
    }

    fn insert_tenant(store: &TenantStore, tenant_id: &str, token: &str) {
        insert_tenant_with_paths(
            store,
            tenant_id,
            token,
            PathBuf::from(format!("/tmp/{tenant_id}.toml")).as_path(),
            PathBuf::from(format!("/tmp/{tenant_id}.env")).as_path(),
            PathBuf::from(format!("/tmp/{tenant_id}.sqlite")).as_path(),
        );
    }

    fn insert_tenant_with_paths(
        store: &TenantStore,
        tenant_id: &str,
        token: &str,
        config_path: &std::path::Path,
        credentials_path: &std::path::Path,
        db_path: &std::path::Path,
    ) {
        let connection = rusqlite::Connection::open(&store.db_path).expect("tenant db open");
        connection
            .execute(
                "INSERT INTO server_tenants (
                    tenant_id, slug, display_name, config_path, credentials_path, db_path
                ) VALUES (?1, ?1, ?1, ?2, ?3, ?4)",
                params![
                    tenant_id,
                    config_path.display().to_string(),
                    credentials_path.display().to_string(),
                    db_path.display().to_string()
                ],
            )
            .expect("insert tenant");
        connection
            .execute(
                "INSERT INTO tenant_api_tokens (tenant_id, token_hash, token_label)
                 VALUES (?1, ?2, 'test')",
                params![tenant_id, token_hash(token)],
            )
            .expect("insert token");
    }

    fn write_test_config(
        config_path: &std::path::Path,
        db_path: &std::path::Path,
        toggl_base_url: &str,
    ) {
        std::fs::write(
            config_path,
            format!(
                r#"[toggl]
workspace_id = 123
api_token_env = "TOGGL_API_TOKEN"
base_url = "{toggl_base_url}"

[runtime]
sqlite_path = "{}"

[[jira.sites]]
key = "acme"
base_url = "https://acme.atlassian.net"
email_env = "ACME_JIRA_EMAIL"
api_token_env = "ACME_JIRA_API_TOKEN"
"#,
                db_path.display()
            ),
        )
        .expect("config");
    }
}
