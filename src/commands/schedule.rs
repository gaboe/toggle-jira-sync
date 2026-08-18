use std::{env, fs, path::Path, path::PathBuf};

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::process::Command;

use anyhow::{anyhow, bail, Context};

use crate::{
    cli::{ScheduleArgs, ScheduleCommand},
    config::AppConfig,
    local_api::LocalServer,
};

#[cfg(any(target_os = "linux", target_os = "windows", test))]
const JOB_NAME: &str = "toggl-jira-sync";
#[cfg(target_os = "macos")]
const MACOS_LABEL: &str = "com.toggl-jira-sync.hourly";

pub async fn run(args: ScheduleArgs) -> anyhow::Result<()> {
    let server = LocalServer::start(args.paths, None, 200).await?;
    let client = server.client();
    match args.command {
        ScheduleCommand::Install => {
            let schedule = client.install_schedule().await?;
            println!(
                "schedule installed: every {} minutes",
                schedule.interval_minutes
            );
        }
        ScheduleCommand::Uninstall => {
            client.uninstall_schedule().await?;
            println!("schedule uninstalled");
        }
        ScheduleCommand::Status => {
            let status = client.schedule_status().await?;
            println!("schedule enabled: {}", status.enabled);
            println!("interval minutes: {}", status.interval_minutes);
            println!("job path: {}", status.job_path);
            println!("job installed: {}", status.job_installed);
        }
        ScheduleCommand::Set(set) => {
            if set.enabled && set.disabled {
                bail!("use either --enabled or --disabled, not both");
            }
            let enabled = if set.enabled {
                Some(true)
            } else if set.disabled {
                Some(false)
            } else {
                None
            };
            let schedule = client.set_schedule(set.interval_minutes, enabled).await?;
            if schedule.enabled {
                println!(
                    "schedule enabled: every {} minutes",
                    schedule.interval_minutes
                );
            } else {
                println!("schedule disabled");
            }
        }
    }

    Ok(())
}

pub(crate) fn install_default_job(config_path: &Path, interval_minutes: u32) -> anyhow::Result<()> {
    if interval_minutes == 0 {
        bail!("schedule interval must be greater than 0 minutes");
    }
    let executable = scheduler_executable()?;
    install_job(&executable, config_path, interval_minutes)
}

fn scheduler_executable() -> anyhow::Result<PathBuf> {
    let current_exe = env::current_exe().context("failed to resolve current executable")?;
    let appimage = env::var_os("APPIMAGE").map(PathBuf::from);
    Ok(select_scheduler_executable(
        &current_exe,
        appimage.as_deref(),
    ))
}

fn select_scheduler_executable(current_exe: &Path, appimage: Option<&Path>) -> PathBuf {
    #[cfg(target_os = "linux")]
    if let Some(appimage) = appimage.filter(|path| !path.as_os_str().is_empty()) {
        return appimage.to_owned();
    }

    #[cfg(not(target_os = "linux"))]
    let _ = appimage;

    current_exe.to_owned()
}

pub(crate) fn install_job(
    executable: &Path,
    config_path: &Path,
    interval_minutes: u32,
) -> anyhow::Result<()> {
    let path = job_path()?;
    let job_file = render_job_file(executable, config_path, interval_minutes);
    if job_installation_unchanged(&path, &job_file, executable, config_path)? {
        load_job(&path)?;
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, job_file)
        .with_context(|| format!("failed to write schedule job {}", path.display()))?;
    #[cfg(target_os = "linux")]
    {
        let service_path = path.with_file_name(format!("{JOB_NAME}.service"));
        fs::write(
            &service_path,
            render_systemd_service(executable, config_path),
        )
        .with_context(|| {
            format!(
                "failed to write schedule service {}",
                service_path.display()
            )
        })?;
    }
    load_job(&path)?;
    Ok(())
}

fn job_installation_unchanged(
    path: &Path,
    job_file: &str,
    _executable: &Path,
    _config_path: &Path,
) -> anyhow::Result<bool> {
    if !path.exists() || fs::read_to_string(path)? != job_file {
        return Ok(false);
    }

    #[cfg(target_os = "linux")]
    {
        let service_path = path.with_file_name(format!("{JOB_NAME}.service"));
        Ok(service_path.exists()
            && fs::read_to_string(&service_path)?
                == render_systemd_service(_executable, _config_path))
    }

    #[cfg(not(target_os = "linux"))]
    Ok(true)
}

pub(crate) fn job_installed() -> anyhow::Result<bool> {
    let timer_path = job_path()?;
    #[cfg(target_os = "linux")]
    let service_exists = timer_path
        .with_file_name(format!("{JOB_NAME}.service"))
        .exists();
    #[cfg(target_os = "linux")]
    return Ok(linux_job_files_present(timer_path.exists(), service_exists));

    #[cfg(not(target_os = "linux"))]
    Ok(timer_path.exists())
}

#[cfg(any(target_os = "linux", test))]
fn linux_job_files_present(timer_exists: bool, service_exists: bool) -> bool {
    timer_exists && service_exists
}

pub(crate) fn uninstall_job() -> anyhow::Result<()> {
    let path = job_path()?;
    let timer_exists = path.exists();
    if timer_exists {
        unload_job(&path)?;
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove schedule job {}", path.display()))?;
    }
    #[cfg(target_os = "linux")]
    {
        let service_path = path.with_file_name(format!("{JOB_NAME}.service"));
        let service_exists = service_path.exists();
        if service_exists {
            fs::remove_file(&service_path).with_context(|| {
                format!(
                    "failed to remove schedule service {}",
                    service_path.display()
                )
            })?;
        }
        if timer_exists || service_exists {
            reload_systemd()?;
        }
    }
    Ok(())
}

fn load_job(_path: &Path) -> anyhow::Result<()> {
    if env::var_os("TOGGL_JIRA_SYNC_SKIP_SCHEDULER_LOAD").is_some() {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let _ = quiet_launchctl("unload", _path);
        let output = Command::new("launchctl")
            .arg("load")
            .arg(_path)
            .output()
            .context("failed to run launchctl load")?;
        if !output.status.success() {
            bail!("launchctl load failed for {}", _path.display());
        }
    }
    #[cfg(target_os = "windows")]
    {
        let output = Command::new("cmd")
            .arg("/C")
            .arg(_path)
            .output()
            .with_context(|| format!("failed to run {}", _path.display()))?;
        if !output.status.success() {
            bail!("schtasks create failed for {}", _path.display());
        }
    }
    #[cfg(target_os = "linux")]
    {
        reload_systemd()?;

        let output = Command::new("systemctl")
            .args(systemd_enable_args())
            .output()
            .context("failed to enable toggl-jira-sync systemd timer")?;
        if !output.status.success() {
            bail!("systemctl --user enable --now failed for {JOB_NAME}.timer");
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemd_reload_args() -> [&'static str; 2] {
    ["--user", "daemon-reload"]
}

#[cfg(target_os = "linux")]
fn reload_systemd() -> anyhow::Result<()> {
    let output = Command::new("systemctl")
        .args(systemd_reload_args())
        .output()
        .context("failed to run systemctl --user daemon-reload")?;
    if !output.status.success() {
        bail!("systemctl --user daemon-reload failed");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemd_enable_args() -> [&'static str; 4] {
    ["--user", "enable", "--now", "toggl-jira-sync.timer"]
}

#[cfg(target_os = "linux")]
fn systemd_disable_args() -> [&'static str; 4] {
    ["--user", "disable", "--now", "toggl-jira-sync.timer"]
}

fn unload_job(_path: &Path) -> anyhow::Result<()> {
    if env::var_os("TOGGL_JIRA_SYNC_SKIP_SCHEDULER_LOAD").is_some() {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let _ = quiet_launchctl("unload", _path);
    }
    #[cfg(target_os = "windows")]
    {
        let output = Command::new("schtasks")
            .args(["/Delete", "/F", "/TN", JOB_NAME])
            .output()
            .context("failed to run schtasks delete")?;
        if !output.status.success() && _path.exists() {
            bail!("schtasks delete failed for {JOB_NAME}");
        }
    }
    #[cfg(target_os = "linux")]
    {
        let output = Command::new("systemctl")
            .args(systemd_disable_args())
            .output()
            .context("failed to disable toggl-jira-sync systemd timer")?;
        if !output.status.success() {
            bail!("systemctl --user disable --now failed for {JOB_NAME}.timer");
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn quiet_launchctl(action: &str, path: &Path) -> anyhow::Result<()> {
    let output = Command::new("launchctl")
        .arg(action)
        .arg(path)
        .output()
        .with_context(|| format!("failed to run launchctl {action}"))?;
    if !output.status.success() && output.status.code() != Some(5) {
        bail!(
            "launchctl {action} failed with code {}",
            output.status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

pub(crate) fn update_schedule_config(
    config_path: &Path,
    interval_minutes: Option<u32>,
    enabled: Option<bool>,
) -> anyhow::Result<()> {
    if matches!(interval_minutes, Some(0)) {
        bail!("--interval-minutes must be greater than 0");
    }
    let original = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config {}", config_path.display()))?;
    let mut value = original
        .parse::<toml::Value>()
        .context("failed to parse config as TOML")?;
    let table = value
        .as_table_mut()
        .ok_or_else(|| anyhow!("config root must be a TOML table"))?;
    let schedule = table
        .entry("schedule")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("schedule config must be a TOML table"))?;
    if let Some(interval_minutes) = interval_minutes {
        schedule.insert(
            "interval_minutes".to_owned(),
            toml::Value::Integer(i64::from(interval_minutes)),
        );
    }
    if let Some(enabled) = enabled {
        schedule.insert("enabled".to_owned(), toml::Value::Boolean(enabled));
    }
    let updated = toml::to_string_pretty(&value).context("failed to serialize config")?;
    AppConfig::from_toml_str(&updated).context("updated config failed validation")?;
    fs::write(config_path, updated)
        .with_context(|| format!("failed to write config {}", config_path.display()))?;
    Ok(())
}

fn render_job_file(executable: &Path, config_path: &Path, interval_minutes: u32) -> String {
    #[cfg(target_os = "macos")]
    return render_macos_plist(executable, config_path, interval_minutes);
    #[cfg(target_os = "linux")]
    return render_systemd_timer(executable, config_path, interval_minutes);
    #[cfg(target_os = "windows")]
    return render_windows_command(executable, config_path, interval_minutes);
}

#[cfg(target_os = "macos")]
pub(crate) fn job_path() -> anyhow::Result<PathBuf> {
    Ok(home_dir()?.join(format!("Library/LaunchAgents/{MACOS_LABEL}.plist")))
}

#[cfg(target_os = "linux")]
pub(crate) fn job_path() -> anyhow::Result<PathBuf> {
    Ok(home_dir()?.join(format!(".config/systemd/user/{JOB_NAME}.timer")))
}

#[cfg(target_os = "windows")]
pub(crate) fn job_path() -> anyhow::Result<PathBuf> {
    let appdata = env::var_os("APPDATA").ok_or_else(|| anyhow!("APPDATA must be set"))?;
    Ok(PathBuf::from(appdata).join(format!("{JOB_NAME}.schedule.cmd")))
}

fn home_dir() -> anyhow::Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME must be set"))
}

#[cfg(target_os = "macos")]
fn render_macos_plist(executable: &Path, config_path: &Path, interval_minutes: u32) -> String {
    let home = home_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "/tmp".to_owned());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>sync</string>
    <string>--cleanup-deleted</string>
    <string>--config</string>
    <string>{}</string>
  </array>
  <key>StartInterval</key><integer>{}</integer>
  <key>RunAtLoad</key><false/>
  <key>WorkingDirectory</key><string>{}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>{}</string>
    <key>PATH</key><string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>StandardOutPath</key><string>{}/Library/Logs/toggl-jira-sync.out.log</string>
  <key>StandardErrorPath</key><string>{}/Library/Logs/toggl-jira-sync.err.log</string>
</dict>
</plist>
"#,
        MACOS_LABEL,
        xml_escape(&executable.display().to_string()),
        xml_escape(&config_path.display().to_string()),
        interval_minutes * 60,
        xml_escape(&home),
        xml_escape(&home),
        xml_escape(&home),
        xml_escape(&home)
    )
}

#[cfg(target_os = "linux")]
fn render_systemd_timer(_executable: &Path, _config_path: &Path, interval_minutes: u32) -> String {
    format!(
        "[Unit]\nDescription=Toggl Jira Sync timer\n\n[Timer]\nOnBootSec={}min\nOnUnitActiveSec={}min\nUnit={JOB_NAME}.service\n\n[Install]\nWantedBy=timers.target\n",
        interval_minutes,
        interval_minutes,
    )
}

#[cfg(any(target_os = "linux", test))]
fn render_systemd_service(executable: &Path, config_path: &Path) -> String {
    format!(
        "[Unit]\nDescription=Toggl Jira Sync\n\n[Service]\nType=oneshot\nExecStart={} sync --cleanup-deleted --config {}\n",
        systemd_quote(executable),
        systemd_quote(config_path)
    )
}

#[cfg(any(target_os = "linux", test))]
fn systemd_quote(path: &Path) -> String {
    let mut quoted = String::from("\"");
    for character in path.to_string_lossy().chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '$' => quoted.push_str("$$"),
            '%' => quoted.push_str("%%"),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(any(target_os = "windows", test))]
fn render_windows_command(executable: &Path, config_path: &Path, interval_minutes: u32) -> String {
    format!(
        "schtasks /Create /F /SC MINUTE /MO {} /TN {} /TR \"\\\"{}\\\" sync --cleanup-deleted --config \\\"{}\\\"\"\r\n",
        interval_minutes,
        JOB_NAME,
        batch_escape(&executable.to_string_lossy()),
        batch_escape(&config_path.to_string_lossy())
    )
}

#[cfg(any(target_os = "windows", test))]
fn batch_escape(value: &str) -> String {
    value.replace('%', "%%")
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_schedule_config_sets_interval_and_enabled() {
        let temp = tempfile::NamedTempFile::new().expect("temp config");
        fs::write(
            temp.path(),
            r#"
[toggl]
workspace_id = 123
api_token_env = "TOGGL_API_TOKEN"

[jira]

[[jira.sites]]
key = "sabservis"
base_url = "https://sabservis.atlassian.net"
email_env = "SABSERVIS_JIRA_EMAIL"
api_token_env = "SABSERVIS_JIRA_API_TOKEN"
enabled = true
"#,
        )
        .expect("write config");

        update_schedule_config(temp.path(), Some(30), Some(false)).expect("update schedule");

        let config = AppConfig::from_path(temp.path()).expect("config should parse");
        assert!(!config.schedule.enabled);
        assert_eq!(config.schedule.interval_minutes, 30);
    }

    #[test]
    fn scheduler_executable_falls_back_to_current_exe_without_appimage() {
        let current_exe = Path::new("/tmp/.mount_tjs/toggl-jira-sync");

        assert_eq!(select_scheduler_executable(current_exe, None), current_exe);
    }

    #[test]
    fn linux_job_presence_requires_timer_and_service() {
        assert!(!linux_job_files_present(false, false));
        assert!(!linux_job_files_present(true, false));
        assert!(!linux_job_files_present(false, true));
        assert!(linux_job_files_present(true, true));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn appimage_scheduler_uses_stable_appimage_path() {
        let current_exe = Path::new("/tmp/.mount_tjs/toggl-jira-sync");
        let appimage = Path::new("/home/me/Applications/toggl-jira-sync.AppImage");

        assert_eq!(
            select_scheduler_executable(current_exe, Some(appimage)),
            appimage
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_install_enables_user_timer_after_reload() {
        assert_eq!(systemd_reload_args(), ["--user", "daemon-reload"]);
        assert_eq!(
            systemd_enable_args(),
            ["--user", "enable", "--now", "toggl-jira-sync.timer"]
        );
        assert_eq!(
            systemd_disable_args(),
            ["--user", "disable", "--now", "toggl-jira-sync.timer"]
        );
    }

    #[test]
    fn systemd_service_quotes_paths_and_systemd_expansions() {
        let service = render_systemd_service(
            Path::new(r#"/opt/Toggl Jira/100%/$ready/"sync"\bin"#),
            Path::new(r#"/home/me/My Config/$daily 100%.toml"#),
        );

        assert_eq!(
            service,
            r#"[Unit]
Description=Toggl Jira Sync

[Service]
Type=oneshot
ExecStart="/opt/Toggl Jira/100%%/$$ready/\"sync\"\\bin" sync --cleanup-deleted --config "/home/me/My Config/$$daily 100%%.toml"
"#
        );
    }

    #[test]
    fn windows_command_escapes_percent_in_quoted_paths() {
        let command = render_windows_command(
            Path::new(r#"C:\Program Files\Toggl Jira\100%\sync.exe"#),
            Path::new(r#"C:\Users\Me\My Config\100%.toml"#),
            60,
        );

        assert_eq!(
            command,
            "schtasks /Create /F /SC MINUTE /MO 60 /TN toggl-jira-sync /TR \"\\\"C:\\Program Files\\Toggl Jira\\100%%\\sync.exe\\\" sync --cleanup-deleted --config \\\"C:\\Users\\Me\\My Config\\100%%.toml\\\"\"\r\n"
        );
    }
}
