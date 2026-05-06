use clap::Parser;
use std::process::ExitCode;
use toggl_jira_sync::cli::Cli;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    let cli = Cli::parse();
    match toggl_jira_sync::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", format_error_chain(&error));
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();
}

fn format_error_chain(error: &anyhow::Error) -> String {
    let mut lines = Vec::new();

    for cause in error.chain() {
        let line = cause.to_string();
        if lines.last() != Some(&line) {
            lines.push(line);
        }
    }

    lines.join("\n")
}
