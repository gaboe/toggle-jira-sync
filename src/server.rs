use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::{
    app::{
        self, AppStateSnapshot, ConfigOnlySnapshot, ConfigSnapshot, ConfigUpdate,
        DeleteLocalDataResult, ExportConfigResult, LogFileResult, ScheduleCommandStatus,
        ScheduleSnapshot,
    },
    cli::{DoctorArgs, RecoverArgs, ServerArgs, ServerMode, SharedPaths, SyncArgs},
    commands::{
        config::{
            ConfigDiscoverTogglWorkspacesReport, ConfigDiscoverTogglWorkspacesRequest,
            ConfigSetupWriteReport, ConfigSetupWriteRequest, ConfigShowReport,
            ConfigValidateReport,
        },
        doctor::DoctorCommandReport,
        recover::RecoveryCommandReport,
        sync::SyncCommandReport,
    },
    format_error_chain,
    report::StatusReport,
    toggl::TogglClient,
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
    enabled: Option<bool>,
    interval_minutes: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct SyncCommandRequest {
    dry_run: bool,
    cleanup_deleted: bool,
}

#[derive(Debug, Deserialize)]
struct RecoverCommandRequest {
    #[serde(default)]
    repair_duplicates: bool,
}

#[derive(Debug, Deserialize)]
struct DoctorCommandRequest {
    online: bool,
}

#[derive(Debug, Deserialize)]
struct ConfigShowCommandRequest {
    show_secrets: bool,
}

#[derive(Debug, Deserialize)]
struct TestTogglCredentialsRequest {
    base_url: Option<String>,
    api_token: String,
}

#[derive(Debug, Deserialize)]
struct TestJiraCredentialsRequest {
    base_url: String,
    email: String,
    api_token: String,
}

#[derive(Debug, Serialize)]
struct CredentialTestResponse {
    ok: bool,
    message: String,
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
            .route("/api/sync/command", post(sync_command))
            .route("/api/recover/command", post(recover_command))
            .route("/api/doctor/command", post(doctor_command))
            .route("/api/config/show/command", post(config_show_command))
            .route(
                "/api/config/validate/command",
                post(config_validate_command),
            )
            .route(
                "/api/config/setup-write/command",
                post(config_setup_write_command),
            )
            .route(
                "/api/config/discover-toggl-workspaces/command",
                post(config_discover_toggl_workspaces_command),
            )
            .route("/api/config/test-toggl", post(test_toggl_credentials))
            .route("/api/config/test-jira", post(test_jira_credentials))
            .route("/api/log-file", get(log_file))
            .route("/api/schedule", get(schedule_status).patch(update_schedule))
            .route("/api/schedule/install", post(install_schedule))
            .route("/api/schedule/uninstall", post(uninstall_schedule))
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
                    || origin == "tauri://localhost"
                    || origin == "http://tauri.localhost"
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
            HeaderName::from_static("x-tjs-desktop-secrets"),
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

async fn snapshot(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> ApiResult<AppStateSnapshot> {
    app::snapshot_with_credentials(
        state.paths.clone(),
        state.limit,
        state.credentials_path.clone(),
    )
    .map(|snapshot| Json(maybe_redact_snapshot(snapshot, &headers)))
    .map_err(ApiError::from)
}

async fn config_snapshot(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> ApiResult<ConfigOnlySnapshot> {
    app::config_snapshot_with_credentials(state.paths.clone(), state.credentials_path.clone())
        .map(|snapshot| Json(maybe_redact_config_snapshot(snapshot, &headers)))
        .map_err(ApiError::from)
}

async fn status(State(state): State<Arc<ServerState>>) -> ApiResult<StatusReport> {
    app::status_report(state.paths.clone(), state.limit)
        .map(|(_, _, report)| Json(report))
        .map_err(ApiError::from)
}

async fn dry_run(State(state): State<Arc<ServerState>>) -> ApiResult<AppStateSnapshot> {
    if let Err(error) = run_sync_off_thread(
        state.paths.clone(),
        state.credentials_path.clone(),
        true,
        false,
    )
    .await
    {
        app::append_log(&format!("dry_run failed: {}", format_error_chain(&error)));
        return Err(ApiError::from(error));
    }
    app::snapshot_with_credentials(
        state.paths.clone(),
        state.limit,
        state.credentials_path.clone(),
    )
    .map(|snapshot| Json(maybe_redact_snapshot(snapshot, &HeaderMap::new())))
    .map_err(ApiError::from)
}

async fn sync(State(state): State<Arc<ServerState>>) -> ApiResult<AppStateSnapshot> {
    if let Err(error) = run_sync_off_thread(
        state.paths.clone(),
        state.credentials_path.clone(),
        false,
        true,
    )
    .await
    {
        app::append_log(&format!("sync failed: {}", format_error_chain(&error)));
        return Err(ApiError::from(error));
    }
    app::snapshot_with_credentials(
        state.paths.clone(),
        state.limit,
        state.credentials_path.clone(),
    )
    .map(|snapshot| Json(maybe_redact_snapshot(snapshot, &HeaderMap::new())))
    .map_err(ApiError::from)
}

fn maybe_redact_snapshot(snapshot: AppStateSnapshot, headers: &HeaderMap) -> AppStateSnapshot {
    if desktop_secrets_allowed(headers) {
        snapshot
    } else {
        snapshot.redacted()
    }
}

fn maybe_redact_config_snapshot(
    snapshot: ConfigOnlySnapshot,
    headers: &HeaderMap,
) -> ConfigOnlySnapshot {
    if desktop_secrets_allowed(headers) {
        snapshot
    } else {
        snapshot.redacted()
    }
}

fn desktop_secrets_allowed(headers: &HeaderMap) -> bool {
    headers
        .get("x-tjs-desktop-secrets")
        .and_then(|value| value.to_str().ok())
        == Some("1")
}

async fn run_sync_off_thread(
    paths: SharedPaths,
    credentials_path: Option<PathBuf>,
    dry_run: bool,
    cleanup_deleted: bool,
) -> anyhow::Result<()> {
    run_sync_command_off_thread(paths, credentials_path, dry_run, cleanup_deleted)
        .await
        .map(|_| ())
}

async fn run_sync_command_off_thread(
    paths: SharedPaths,
    credentials_path: Option<PathBuf>,
    dry_run: bool,
    cleanup_deleted: bool,
) -> anyhow::Result<SyncCommandReport> {
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to start sync runtime")?
            .block_on(async move {
                let args = SyncArgs {
                    paths,
                    dry_run,
                    cleanup_deleted,
                    json: false,
                    quiet: true,
                };
                let credentials = if let Some(credentials_path) = credentials_path {
                    Some(
                        crate::commands::config::load_isolated_credentials_from_path(
                            &credentials_path,
                        )?,
                    )
                } else {
                    None
                };
                crate::commands::sync::sync_report(args, credentials).await
            })
    })
    .await
    .context("sync task failed")?
}

async fn sync_command(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<SyncCommandRequest>,
) -> ApiResult<SyncCommandReport> {
    run_sync_command_off_thread(
        state.paths.clone(),
        state.credentials_path.clone(),
        request.dry_run,
        request.cleanup_deleted,
    )
    .await
    .map(Json)
    .map_err(ApiError::from)
}

async fn run_recover_command_off_thread(
    paths: SharedPaths,
    credentials_path: Option<PathBuf>,
    repair_duplicates: bool,
) -> anyhow::Result<RecoveryCommandReport> {
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to start recover runtime")?
            .block_on(async move {
                let args = RecoverArgs {
                    paths,
                    repair_duplicates,
                    json: false,
                };
                if let Some(credentials_path) = credentials_path {
                    crate::commands::recover::recover_report_with_isolated_credentials(
                        args,
                        credentials_path,
                    )
                    .await
                } else {
                    crate::commands::recover::recover_report(args, None).await
                }
            })
    })
    .await
    .context("recover task failed")?
}

async fn recover_command(
    State(state): State<Arc<ServerState>>,
    request: Option<Json<RecoverCommandRequest>>,
) -> ApiResult<RecoveryCommandReport> {
    let repair_duplicates = request
        .map(|Json(request)| request.repair_duplicates)
        .unwrap_or(false);
    run_recover_command_off_thread(
        state.paths.clone(),
        state.credentials_path.clone(),
        repair_duplicates,
    )
    .await
    .map(Json)
    .map_err(ApiError::from)
}

async fn run_doctor_command_off_thread(
    paths: SharedPaths,
    credentials_path: Option<PathBuf>,
    online: bool,
) -> anyhow::Result<DoctorCommandReport> {
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to start doctor runtime")?
            .block_on(async move {
                let args = DoctorArgs { paths, online };
                if let Some(credentials_path) = credentials_path {
                    crate::commands::doctor::doctor_report_with_isolated_credentials(
                        args,
                        credentials_path,
                    )
                    .await
                } else {
                    crate::commands::doctor::doctor_report(args, None).await
                }
            })
    })
    .await
    .context("doctor task failed")?
}

async fn doctor_command(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<DoctorCommandRequest>,
) -> ApiResult<DoctorCommandReport> {
    run_doctor_command_off_thread(
        state.paths.clone(),
        state.credentials_path.clone(),
        request.online,
    )
    .await
    .map(Json)
    .map_err(ApiError::from)
}

async fn config_show_command(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<ConfigShowCommandRequest>,
) -> ApiResult<ConfigShowReport> {
    crate::commands::config::config_show_report(
        state.paths.clone(),
        state.credentials_path.clone(),
        request.show_secrets,
    )
    .map(Json)
    .map_err(ApiError::from)
}

async fn config_validate_command(
    State(state): State<Arc<ServerState>>,
) -> ApiResult<ConfigValidateReport> {
    crate::commands::config::config_validate_report(state.paths.clone())
        .map(Json)
        .map_err(ApiError::from)
}

async fn config_setup_write_command(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<ConfigSetupWriteRequest>,
) -> ApiResult<ConfigSetupWriteReport> {
    crate::commands::config::config_setup_write(
        state.paths.clone(),
        state.credentials_path.clone(),
        request,
    )
    .map(Json)
    .map_err(ApiError::from)
}

async fn config_discover_toggl_workspaces_command(
    Json(request): Json<ConfigDiscoverTogglWorkspacesRequest>,
) -> ApiResult<ConfigDiscoverTogglWorkspacesReport> {
    TogglClient::list_workspaces(&request.base_url, &request.api_token)
        .await
        .map(|workspaces| Json(ConfigDiscoverTogglWorkspacesReport { workspaces }))
        .map_err(|error| ApiError::from(anyhow::anyhow!(error)))
}

async fn test_toggl_credentials(
    Json(request): Json<TestTogglCredentialsRequest>,
) -> ApiResult<CredentialTestResponse> {
    let base_url = request
        .base_url
        .unwrap_or_else(|| "https://api.track.toggl.com".to_owned());
    TogglClient::list_workspaces(&base_url, &request.api_token)
        .await
        .map(|workspaces| {
            Json(CredentialTestResponse {
                ok: true,
                message: format!(
                    "Toggl credentials work. {} workspace(s) visible.",
                    workspaces.len()
                ),
            })
        })
        .map_err(|error| ApiError::from(anyhow::anyhow!(error)))
}

async fn test_jira_credentials(
    Json(request): Json<TestJiraCredentialsRequest>,
) -> ApiResult<CredentialTestResponse> {
    let client = crate::jira::JiraClient::from_credentials(
        request.base_url,
        request.email,
        request.api_token,
    );
    client
        .validate_credentials()
        .await
        .map(|_| {
            Json(CredentialTestResponse {
                ok: true,
                message: "Jira credentials work.".to_owned(),
            })
        })
        .map_err(|error| ApiError::from(anyhow::anyhow!(error)))
}

async fn schedule_status(
    State(state): State<Arc<ServerState>>,
) -> ApiResult<ScheduleCommandStatus> {
    app::schedule_status(state.paths.clone())
        .map(Json)
        .map_err(ApiError::from)
}

async fn install_schedule(State(state): State<Arc<ServerState>>) -> ApiResult<ScheduleSnapshot> {
    app::install_schedule(state.paths.clone())
        .map(Json)
        .map_err(ApiError::from)
}

async fn uninstall_schedule(State(_state): State<Arc<ServerState>>) -> ApiResult<()> {
    app::uninstall_schedule().map(Json).map_err(ApiError::from)
}

async fn update_schedule(
    State(state): State<Arc<ServerState>>,
    Json(update): Json<ScheduleUpdate>,
) -> ApiResult<ScheduleSnapshot> {
    app::set_schedule(state.paths.clone(), update.interval_minutes, update.enabled)
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

async fn log_file() -> ApiResult<LogFileResult> {
    app::log_file().map(Json).map_err(ApiError::from)
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
    use crate::config::AppConfig;
    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode};
    use http_body_util::BodyExt;
    use httpmock::MockServer;
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
    async fn multi_mode_does_not_expose_unscoped_workspace_discovery() {
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
                .method(Method::POST)
                .uri("/api/config/discover-toggl-workspaces/command")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "base_url": "https://api.track.toggl.com",
                        "api_token": "do-not-expose",
                    })
                    .to_string(),
                ))
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

    #[tokio::test]
    async fn single_mode_discovers_toggl_workspaces_without_returning_token() {
        let toggl = MockServer::start();
        let workspaces_mock = toggl.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/v9/me/workspaces")
                .header(
                    "authorization",
                    "Basic ZGlzY292ZXJ5LXRva2VuOmFwaV90b2tlbg==",
                );
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"[{"id":700001,"name":"Engineering"}]"#);
        });

        let response = single_router()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/config/discover-toggl-workspaces/command")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "base_url": toggl.base_url(),
                            "api_token": "discovery-token",
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        workspaces_mock.assert();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let body_text = String::from_utf8(body.to_vec()).expect("body utf-8");
        assert!(!body_text.contains("discovery-token"), "{body_text}");
        let json: serde_json::Value = serde_json::from_str(&body_text).expect("json");
        assert_eq!(json["workspaces"][0]["id"], 700001);
        assert_eq!(json["workspaces"][0]["name"], "Engineering");
    }

    #[tokio::test]
    async fn single_mode_exposes_schedule_status_and_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let db_path = dir.path().join("sync.sqlite");
        write_test_config(&config_path, &db_path, "https://api.track.toggl.com");

        let status_response = router(
            SharedPaths {
                config: Some(config_path.clone()),
                db: Some(db_path.clone()),
            },
            None,
            200,
            ServerMode::Single,
            None,
        )
        .expect("router")
        .oneshot(
            Request::builder()
                .uri("/api/schedule")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
        assert_eq!(status_response.status(), StatusCode::OK);

        let update_response = router(
            SharedPaths {
                config: Some(config_path.clone()),
                db: Some(db_path),
            },
            None,
            200,
            ServerMode::Single,
            None,
        )
        .expect("router")
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri("/api/schedule")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "enabled": false,
                        "interval_minutes": 45,
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
        assert_eq!(update_response.status(), StatusCode::OK);

        let config = AppConfig::from_path(config_path).expect("updated config");
        assert!(!config.schedule.enabled);
        assert_eq!(config.schedule.interval_minutes, 45);
    }

    #[tokio::test]
    async fn single_mode_sync_command_uses_server_route() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let db_path = dir.path().join("sync.sqlite");
        write_test_config(&config_path, &db_path, "https://api.track.toggl.com");
        let missing_token = "TJS_MISSING_TOKEN_FOR_SYNC_COMMAND_TEST";
        std::env::remove_var(missing_token);
        let config = std::fs::read_to_string(&config_path).expect("config");
        std::fs::write(
            &config_path,
            config.replace("TOGGL_API_TOKEN", missing_token),
        )
        .expect("config");

        let response = router(
            SharedPaths {
                config: Some(config_path),
                db: Some(db_path),
            },
            None,
            200,
            ServerMode::Single,
            None,
        )
        .expect("router")
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/sync/command")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "dry_run": true,
                        "cleanup_deleted": false,
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert!(json["error"]
            .as_str()
            .expect("error")
            .contains("missing env var TJS_MISSING_TOKEN_FOR_SYNC_COMMAND_TEST"));
    }

    #[tokio::test]
    async fn single_mode_doctor_command_uses_server_route() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let db_path = dir.path().join("sync.sqlite");
        write_test_config(&config_path, &db_path, "https://api.track.toggl.com");
        let missing_token = "TJS_MISSING_TOKEN_FOR_DOCTOR_COMMAND_TEST";
        std::env::remove_var(missing_token);
        let config = std::fs::read_to_string(&config_path).expect("config");
        std::fs::write(
            &config_path,
            config.replace("TOGGL_API_TOKEN", missing_token),
        )
        .expect("config");

        let response = router(
            SharedPaths {
                config: Some(config_path),
                db: Some(db_path),
            },
            None,
            200,
            ServerMode::Single,
            None,
        )
        .expect("router")
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/doctor/command")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "online": false }).to_string(),
                ))
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
        assert!(json["failures"]
            .as_array()
            .expect("failures")
            .iter()
            .any(|failure| failure
                .as_str()
                .expect("failure")
                .contains("missing env var TJS_MISSING_TOKEN_FOR_DOCTOR_COMMAND_TEST")));
    }

    #[tokio::test]
    async fn single_mode_recover_command_uses_server_route() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let db_path = dir.path().join("sync.sqlite");
        write_test_config(&config_path, &db_path, "https://api.track.toggl.com");
        let missing_token = "TJS_MISSING_TOKEN_FOR_RECOVER_COMMAND_TEST";
        std::env::remove_var(missing_token);
        let config = std::fs::read_to_string(&config_path).expect("config");
        std::fs::write(
            &config_path,
            config.replace("TOGGL_API_TOKEN", missing_token),
        )
        .expect("config");

        let response = router(
            SharedPaths {
                config: Some(config_path),
                db: Some(db_path),
            },
            None,
            200,
            ServerMode::Single,
            None,
        )
        .expect("router")
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/recover/command")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert!(json["error"]
            .as_str()
            .expect("error")
            .contains("missing env var TJS_MISSING_TOKEN_FOR_RECOVER_COMMAND_TEST"));
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
