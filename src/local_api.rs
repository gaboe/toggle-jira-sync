use std::{net::SocketAddr, path::PathBuf};

use anyhow::{anyhow, Context};
use serde::de::DeserializeOwned;
use tokio::task::JoinHandle;

use crate::{
    app::{
        AppStateSnapshot, ConfigOnlySnapshot, ConfigSnapshot, ConfigUpdate, DeleteLocalDataResult,
        ExportConfigResult, ScheduleCommandStatus, ScheduleSnapshot,
    },
    cli::{ServerMode, SharedPaths},
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
    report::StatusReport,
    server,
};

pub struct LocalServer {
    base_url: String,
    _task: JoinHandle<()>,
}

impl LocalServer {
    pub async fn start(
        paths: SharedPaths,
        credentials_path: Option<PathBuf>,
        limit: usize,
    ) -> anyhow::Result<Self> {
        let listener = server::bind("127.0.0.1", 0).await?;
        let addr = listener
            .local_addr()
            .context("failed to read local server address")?;
        let task = tokio::spawn(async move {
            if let Err(error) = server::serve_listener(
                listener,
                paths,
                credentials_path,
                limit,
                ServerMode::Single,
                None,
            )
            .await
            {
                eprintln!("local server failed: {error}");
            }
        });

        Ok(Self {
            base_url: format_base_url(addr),
            _task: task,
        })
    }

    pub fn client(&self) -> LocalApiClient {
        LocalApiClient::new(self.base_url.clone())
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self._task.abort();
    }
}

#[derive(Clone)]
pub struct LocalApiClient {
    base_url: String,
    client: reqwest::Client,
}

impl LocalApiClient {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            client: reqwest::Client::new(),
        }
    }

    pub async fn status(&self) -> anyhow::Result<StatusReport> {
        self.get_json("/api/status").await
    }

    pub async fn snapshot(&self) -> anyhow::Result<AppStateSnapshot> {
        self.get_json("/api/snapshot").await
    }

    pub async fn config_snapshot(&self) -> anyhow::Result<ConfigOnlySnapshot> {
        self.get_json("/api/config").await
    }

    pub async fn dry_run(&self) -> anyhow::Result<AppStateSnapshot> {
        self.post_empty_json("/api/sync/dry-run").await
    }

    pub async fn sync(&self) -> anyhow::Result<AppStateSnapshot> {
        self.post_empty_json("/api/sync").await
    }

    pub async fn sync_command(
        &self,
        dry_run: bool,
        cleanup_deleted: bool,
    ) -> anyhow::Result<SyncCommandReport> {
        self.post_json(
            "/api/sync/command",
            &serde_json::json!({
                "dry_run": dry_run,
                "cleanup_deleted": cleanup_deleted,
            }),
        )
        .await
    }

    pub async fn recover_command(&self) -> anyhow::Result<RecoveryCommandReport> {
        self.post_empty_json("/api/recover/command").await
    }

    pub async fn doctor_command(&self, online: bool) -> anyhow::Result<DoctorCommandReport> {
        self.post_json(
            "/api/doctor/command",
            &serde_json::json!({
                "online": online,
            }),
        )
        .await
    }

    pub async fn config_show(&self, show_secrets: bool) -> anyhow::Result<ConfigShowReport> {
        self.post_json(
            "/api/config/show/command",
            &serde_json::json!({
                "show_secrets": show_secrets,
            }),
        )
        .await
    }

    pub async fn config_validate(&self) -> anyhow::Result<ConfigValidateReport> {
        self.post_empty_json("/api/config/validate/command").await
    }

    pub async fn config_setup_write(
        &self,
        request: &ConfigSetupWriteRequest,
    ) -> anyhow::Result<ConfigSetupWriteReport> {
        self.post_json("/api/config/setup-write/command", request)
            .await
    }

    pub async fn config_discover_toggl_workspaces(
        &self,
        base_url: &str,
        api_token: &str,
    ) -> anyhow::Result<ConfigDiscoverTogglWorkspacesReport> {
        self.post_json(
            "/api/config/discover-toggl-workspaces/command",
            &ConfigDiscoverTogglWorkspacesRequest {
                base_url: base_url.to_owned(),
                api_token: api_token.to_owned(),
            },
        )
        .await
    }

    pub async fn schedule_status(&self) -> anyhow::Result<ScheduleCommandStatus> {
        self.get_json("/api/schedule").await
    }

    pub async fn install_schedule(&self) -> anyhow::Result<ScheduleSnapshot> {
        self.post_empty_json("/api/schedule/install").await
    }

    pub async fn uninstall_schedule(&self) -> anyhow::Result<()> {
        self.post_empty_json("/api/schedule/uninstall").await
    }

    pub async fn set_schedule(
        &self,
        interval_minutes: Option<u32>,
        enabled: Option<bool>,
    ) -> anyhow::Result<ScheduleSnapshot> {
        self.patch_json(
            "/api/schedule",
            &serde_json::json!({
                "enabled": enabled,
                "interval_minutes": interval_minutes,
            }),
        )
        .await
    }

    pub async fn update_schedule(&self, enabled: bool) -> anyhow::Result<ScheduleSnapshot> {
        self.set_schedule(None, Some(enabled)).await
    }

    pub async fn save_config(&self, update: &ConfigUpdate) -> anyhow::Result<ConfigSnapshot> {
        self.put_json("/api/config", update).await
    }

    pub async fn delete_local_data(&self) -> anyhow::Result<DeleteLocalDataResult> {
        let response = self
            .client
            .delete(format!("{}/api/local-data", self.base_url))
            .send()
            .await
            .context("failed to call local server /api/local-data")?;
        decode_response(response).await
    }

    pub async fn export_config(&self) -> anyhow::Result<ExportConfigResult> {
        self.post_empty_json("/api/config/export").await
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let response = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .send()
            .await
            .with_context(|| format!("failed to call local server {path}"))?;
        decode_response(response).await
    }

    async fn post_empty_json<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let response = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .send()
            .await
            .with_context(|| format!("failed to call local server {path}"))?;
        decode_response(response).await
    }

    async fn post_json<T: DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        let response = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .json(body)
            .send()
            .await
            .with_context(|| format!("failed to call local server {path}"))?;
        decode_response(response).await
    }

    async fn patch_json<T: DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        let response = self
            .client
            .patch(format!("{}{}", self.base_url, path))
            .json(body)
            .send()
            .await
            .with_context(|| format!("failed to call local server {path}"))?;
        decode_response(response).await
    }

    async fn put_json<T: DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        let response = self
            .client
            .put(format!("{}{}", self.base_url, path))
            .json(body)
            .send()
            .await
            .with_context(|| format!("failed to call local server {path}"))?;
        decode_response(response).await
    }
}

async fn decode_response<T: DeserializeOwned>(response: reqwest::Response) -> anyhow::Result<T> {
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read response body")?;
    if !status.is_success() {
        if let Ok(error) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(message) = error.get("error").and_then(|error| error.as_str()) {
                return Err(anyhow!(message.to_owned()));
            }
        }
        return Err(anyhow!("local server returned {status}: {body}"));
    }
    serde_json::from_str(&body)
        .with_context(|| format!("failed to decode local server response: {body}"))
}

fn format_base_url(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_server_starts_on_loopback_ephemeral_port() {
        let server = LocalServer::start(
            SharedPaths {
                config: None,
                db: None,
            },
            None,
            10,
        )
        .await
        .expect("local server");

        assert!(server.base_url().starts_with("http://127.0.0.1:"));
    }
}
