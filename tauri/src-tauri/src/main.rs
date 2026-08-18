use std::{env, process::Command};

use clap::Parser;

use tauri::Manager;
use toggl_jira_sync::{
    app::{
        AppStateSnapshot, ConfigOnlySnapshot, ConfigSnapshot, ConfigUpdate, DeleteLocalDataResult,
        ExportConfigResult, LogFileResult, ScheduleSnapshot,
    },
    commands::recover::RecoveryCommandReport,
    format_error_chain,
    local_api::{
        CredentialTestResponse, LocalApiClient, TestJiraCredentialsRequest,
        TestTogglCredentialsRequest,
    },
    report::StatusReport,
};

const STATUS_LIMIT: usize = 200;

#[tauri::command]
async fn snapshot(api: tauri::State<'_, LocalApiClient>) -> Result<AppStateSnapshot, String> {
    api.snapshot().await.map_err(format_tauri_error)
}

#[tauri::command]
async fn config_snapshot(
    api: tauri::State<'_, LocalApiClient>,
) -> Result<ConfigOnlySnapshot, String> {
    api.config_snapshot().await.map_err(format_tauri_error)
}

#[tauri::command]
async fn status(api: tauri::State<'_, LocalApiClient>) -> Result<StatusReport, String> {
    api.status().await.map_err(format_tauri_error)
}

#[tauri::command]
async fn dry_run(api: tauri::State<'_, LocalApiClient>) -> Result<AppStateSnapshot, String> {
    api.dry_run().await.map_err(format_tauri_error)
}

#[tauri::command]
async fn sync(api: tauri::State<'_, LocalApiClient>) -> Result<AppStateSnapshot, String> {
    api.sync().await.map_err(format_tauri_error)
}

#[tauri::command]
async fn recover_command(
    api: tauri::State<'_, LocalApiClient>,
    repair_duplicates: bool,
) -> Result<RecoveryCommandReport, String> {
    api.recover_command(repair_duplicates)
        .await
        .map_err(format_tauri_error)
}

#[tauri::command]
async fn toggle_schedule(
    api: tauri::State<'_, LocalApiClient>,
    enabled: bool,
) -> Result<ScheduleSnapshot, String> {
    api.update_schedule(enabled)
        .await
        .map_err(format_tauri_error)
}

#[tauri::command]
async fn save_config(
    api: tauri::State<'_, LocalApiClient>,
    update: ConfigUpdate,
) -> Result<ConfigSnapshot, String> {
    api.save_config(&update).await.map_err(format_tauri_error)
}

#[tauri::command]
async fn test_toggl_credentials(
    api: tauri::State<'_, LocalApiClient>,
    request: TestTogglCredentialsRequest,
) -> Result<CredentialTestResponse, String> {
    api.test_toggl_credentials(&request)
        .await
        .map_err(format_tauri_error)
}

#[tauri::command]
async fn test_jira_credentials(
    api: tauri::State<'_, LocalApiClient>,
    request: TestJiraCredentialsRequest,
) -> Result<CredentialTestResponse, String> {
    api.test_jira_credentials(&request)
        .await
        .map_err(format_tauri_error)
}

#[tauri::command]
async fn log_file(api: tauri::State<'_, LocalApiClient>) -> Result<LogFileResult, String> {
    api.log_file().await.map_err(format_tauri_error)
}

#[tauri::command]
async fn delete_local_data(
    api: tauri::State<'_, LocalApiClient>,
) -> Result<DeleteLocalDataResult, String> {
    api.delete_local_data().await.map_err(format_tauri_error)
}

#[tauri::command]
async fn export_config(
    api: tauri::State<'_, LocalApiClient>,
) -> Result<ExportConfigResult, String> {
    api.export_config().await.map_err(format_tauri_error)
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    let status = if cfg!(target_os = "macos") {
        Command::new("open").arg(url).status()
    } else if cfg!(target_os = "windows") {
        Command::new("cmd").args(["/C", "start", &url]).status()
    } else {
        Command::new("xdg-open").arg(url).status()
    }
    .map_err(|error| format!("failed to open URL: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("open URL exited with {status}"))
    }
}

fn format_tauri_error(error: anyhow::Error) -> String {
    format_error_chain(&error)
}

fn headless_cli_args(args: &[String]) -> Option<Vec<String>> {
    let command_index = match args.get(1).map(String::as_str) {
        Some("sync") => 1,
        Some(arg)
            if arg.starts_with("-psn_") && args.get(2).map(String::as_str) == Some("sync") =>
        {
            2
        }
        _ => return None,
    };

    Some(
        std::iter::once(args[0].clone())
            .chain(args[command_index..].iter().cloned())
            .collect(),
    )
}

fn run_headless(args: Vec<String>) -> i32 {
    let cli = match toggl_jira_sync::cli::Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = error.exit_code();
            error.print().expect("failed to write CLI error");
            return exit_code;
        }
    };
    let runtime = tokio::runtime::Runtime::new().expect("failed to create Tokio runtime");
    match runtime.block_on(toggl_jira_sync::run(cli)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{}", format_error_chain(&error));
            1
        }
    }
}

fn main() {
    if let Some(args) = headless_cli_args(&env::args().collect::<Vec<_>>()) {
        std::process::exit(run_headless(args));
    }

    tauri::Builder::default()
        .setup(|app| {
            let listener =
                tauri::async_runtime::block_on(toggl_jira_sync::server::bind("127.0.0.1", 0))
                    .map_err(|error| Box::<dyn std::error::Error>::from(error.to_string()))?;
            let local_addr = listener
                .local_addr()
                .map_err(|error| Box::<dyn std::error::Error>::from(error.to_string()))?;
            let api_base_url = format!("http://{local_addr}");

            tauri::async_runtime::spawn(async move {
                if let Err(error) = toggl_jira_sync::server::serve_listener(
                    listener,
                    toggl_jira_sync::cli::SharedPaths {
                        config: None,
                        db: None,
                    },
                    None,
                    STATUS_LIMIT,
                    toggl_jira_sync::cli::ServerMode::Single,
                    None,
                )
                .await
                {
                    eprintln!("embedded server failed: {error}");
                }
            });

            app.manage(LocalApiClient::new(api_base_url.clone()));

            if let Some(window) = app.get_webview_window("main") {
                window.eval(format!(
                    "window.__TJS_API_BASE_URL__ = {}; window.__TJS_DESKTOP_SECRETS__ = true;",
                    serde_json::to_string(&api_base_url)?
                ))?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            config_snapshot,
            status,
            dry_run,
            sync,
            recover_command,
            toggle_schedule,
            save_config,
            test_toggl_credentials,
            test_jira_credentials,
            log_file,
            delete_local_data,
            export_config,
            open_url
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}

#[cfg(test)]
mod tests {
    use super::headless_cli_args;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn scheduler_sync_args_dispatch_headlessly() {
        let result = headless_cli_args(&args(&[
            "/Applications/Toggl Jira Sync.app/Contents/MacOS/toggl-jira-sync-tauri",
            "sync",
            "--cleanup-deleted",
            "--config",
            "/Users/me/.config/toggl-jira-sync/config.toml",
        ]));

        assert_eq!(
            result,
            Some(args(&[
                "/Applications/Toggl Jira Sync.app/Contents/MacOS/toggl-jira-sync-tauri",
                "sync",
                "--cleanup-deleted",
                "--config",
                "/Users/me/.config/toggl-jira-sync/config.toml",
            ]))
        );
    }

    #[test]
    fn normal_desktop_launch_stays_gui() {
        assert_eq!(headless_cli_args(&args(&["toggl-jira-sync-tauri"])), None);
        assert_eq!(
            headless_cli_args(&args(&["toggl-jira-sync-tauri", "-psn_0_12345"])),
            None
        );
    }

    #[test]
    fn macos_launch_service_prefix_keeps_scheduler_headless() {
        assert!(headless_cli_args(&args(&[
            "toggl-jira-sync-tauri",
            "-psn_0_12345",
            "sync",
            "--config",
            "/tmp/config.toml",
        ]))
        .is_some());
    }
}
