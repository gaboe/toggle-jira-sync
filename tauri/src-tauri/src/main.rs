use std::process::Command;

use toggl_jira_sync::{
    app::{self, AppStateSnapshot, ConfigOnlySnapshot, ConfigSnapshot, ConfigUpdate, DeleteLocalDataResult, ExportConfigResult, ScheduleSnapshot},
    cli::SharedPaths,
    format_error_chain,
    report::StatusReport,
};

const STATUS_LIMIT: usize = 200;

#[tauri::command]
fn snapshot() -> Result<AppStateSnapshot, String> {
    app::snapshot(default_paths(), STATUS_LIMIT).map_err(format_tauri_error)
}

#[tauri::command]
fn config_snapshot() -> Result<ConfigOnlySnapshot, String> {
    app::config_snapshot(default_paths()).map_err(format_tauri_error)
}

#[tauri::command]
fn status() -> Result<StatusReport, String> {
    app::status_report(default_paths(), STATUS_LIMIT)
        .map(|(_, _, report)| report)
        .map_err(format_tauri_error)
}

#[tauri::command]
fn dry_run() -> Result<AppStateSnapshot, String> {
    run_sync_blocking(true)?;
    snapshot()
}

#[tauri::command]
fn sync() -> Result<AppStateSnapshot, String> {
    run_sync_blocking(false)?;
    snapshot()
}

#[tauri::command]
fn toggle_schedule(enabled: bool) -> Result<ScheduleSnapshot, String> {
    app::update_schedule(default_paths(), enabled).map_err(format_tauri_error)
}

#[tauri::command]
fn save_config(update: ConfigUpdate) -> Result<ConfigSnapshot, String> {
    app::save_config(default_paths(), update).map_err(format_tauri_error)
}

#[tauri::command]
fn delete_local_data() -> Result<DeleteLocalDataResult, String> {
    app::delete_local_data(default_paths()).map_err(format_tauri_error)
}

#[tauri::command]
fn export_config() -> Result<ExportConfigResult, String> {
    app::export_config(default_paths()).map_err(format_tauri_error)
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

fn default_paths() -> SharedPaths {
    SharedPaths {
        config: None,
        db: None,
    }
}

fn run_sync_blocking(dry_run: bool) -> Result<(), String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to start sync runtime: {error}"))?
        .block_on(app::run_sync(default_paths(), dry_run, !dry_run))
        .map_err(format_tauri_error)
}

fn format_tauri_error(error: anyhow::Error) -> String {
    format_error_chain(&error)
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            snapshot,
            config_snapshot,
            status,
            dry_run,
            sync,
            toggle_schedule,
            save_config,
            delete_local_data,
            export_config,
            open_url
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}
