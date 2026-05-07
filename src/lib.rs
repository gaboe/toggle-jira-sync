pub mod cli;
pub mod commands;
pub mod config;
pub mod db;
pub mod jira;
pub mod report;
pub mod sync;
pub mod time;
pub mod toggl;

pub async fn run(cli: cli::Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(cli::Command::Sync(args)) => commands::sync::run(args).await,
        Some(cli::Command::Recover(args)) => commands::recover::run(args).await,
        Some(cli::Command::Config(args)) => commands::config::run(args).await,
        Some(cli::Command::Doctor(args)) => commands::doctor::run(args).await,
        Some(cli::Command::Status(args)) => commands::status::run(args).await,
        Some(cli::Command::Tui(args)) => commands::tui::run(args).await,
        Some(cli::Command::Schedule(args)) => commands::schedule::run(args),
        None => {
            commands::tui::run(cli::TuiArgs {
                paths: cli::SharedPaths {
                    config: None,
                    db: None,
                },
                limit: 200,
            })
            .await
        }
    }
}
