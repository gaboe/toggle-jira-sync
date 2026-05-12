use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = env!("CARGO_PKG_NAME"), version, about = "Mirror Toggl time entries into Jira worklogs")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Sync(SyncArgs),
    Recover(RecoverArgs),
    Config(ConfigArgs),
    Doctor(DoctorArgs),
    Status(StatusArgs),
    Tui(TuiArgs),
    Schedule(ScheduleArgs),
}

#[derive(Debug, Args, Clone)]
pub struct SharedPaths {
    #[arg(long)]
    pub config: Option<PathBuf>,

    #[arg(long)]
    pub db: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    #[command(flatten)]
    pub paths: SharedPaths,

    #[arg(long)]
    pub dry_run: bool,

    #[arg(long)]
    pub cleanup_deleted: bool,

    #[arg(long)]
    pub json: bool,

    #[arg(long, hide = true)]
    pub quiet: bool,
}

#[derive(Debug, Args)]
pub struct RecoverArgs {
    #[command(flatten)]
    pub paths: SharedPaths,

    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    #[command(flatten)]
    pub paths: SharedPaths,

    #[arg(long)]
    pub json: bool,

    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct TuiArgs {
    #[command(flatten)]
    pub paths: SharedPaths,

    #[arg(long, default_value_t = 200)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[command(flatten)]
    pub paths: SharedPaths,

    #[arg(long)]
    pub online: bool,
}

#[derive(Debug, Args)]
pub struct ScheduleArgs {
    #[command(flatten)]
    pub paths: SharedPaths,

    #[command(subcommand)]
    pub command: ScheduleCommand,
}

#[derive(Debug, Subcommand)]
pub enum ScheduleCommand {
    Install,
    Uninstall,
    Status,
    Set(ScheduleSetArgs),
}

#[derive(Debug, Args)]
pub struct ScheduleSetArgs {
    #[arg(long)]
    pub interval_minutes: Option<u32>,

    #[arg(long, conflicts_with = "disabled")]
    pub enabled: bool,

    #[arg(long, conflicts_with = "enabled")]
    pub disabled: bool,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Setup(ConfigSetupArgs),
    Show(ConfigShowArgs),
    Validate(ConfigValidateArgs),
}

#[derive(Debug, Args)]
pub struct ConfigSetupArgs {
    #[arg(
        long,
        help = "Path to config file (default: ~/.config/toggl-jira-sync/config.toml)"
    )]
    pub config: Option<PathBuf>,

    #[arg(
        long,
        help = "Path to credentials file (default: ~/.config/toggl-jira-sync/credentials.env)"
    )]
    pub credentials: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ConfigShowArgs {
    #[arg(
        long,
        help = "Path to config file (default: ~/.config/toggl-jira-sync/config.toml)"
    )]
    pub config: Option<PathBuf>,

    #[arg(
        long,
        help = "Path to credentials file (default: ~/.config/toggl-jira-sync/credentials.env)"
    )]
    pub credentials: Option<PathBuf>,

    #[arg(long)]
    pub show_secrets: bool,
}

#[derive(Debug, Args)]
pub struct ConfigValidateArgs {
    #[arg(long)]
    pub config: Option<PathBuf>,
}
