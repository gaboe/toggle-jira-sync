use anyhow::Context;

use crate::{
    cli::StatusArgs,
    commands::config::{resolve_config_path, resolve_db_path},
    config::AppConfig,
    db::Database,
    report::StatusReport,
};

pub async fn run(args: StatusArgs) -> anyhow::Result<()> {
    let config_path = resolve_config_path(args.paths.config)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let db_path = resolve_db_path(
        args.paths.db,
        &config_path,
        config.runtime.sqlite_path.as_deref(),
        "status",
    )?;

    let database = Database::open(&db_path)
        .with_context(|| format!("failed to open SQLite DB {}", db_path.display()))?;
    database
        .run_migrations()
        .context("failed to run DB migrations")?;

    let report = StatusReport::from_rows(
        database
            .list_status_entries(args.limit)
            .context("failed to load status rows")?,
    );

    if args.json {
        println!("{}", report.to_json_string()?);
    } else {
        println!("{}", report.to_human_string());
    }

    Ok(())
}
