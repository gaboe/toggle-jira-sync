use anyhow::Context;

use crate::{
    cli::RecoverArgs,
    commands::config::{load_default_credentials_into_env, resolve_config_path, resolve_db_path},
    config::AppConfig,
    db::{Database, NewSyncRun},
    jira::JiraClient,
    sync::{
        planner::{extract_issue_keys, IssueSiteMapping},
        recovery::{recover, RecoveryInput, RecoveryReport, RecoverySite},
        resolver::{IssueSiteResolver, ResolverSite},
    },
    toggl::{TogglClient, TogglClientConfig, TogglTimeEntry},
};

const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

pub async fn run(args: RecoverArgs) -> anyhow::Result<()> {
    let uses_default_config = args.paths.config.is_none();
    let config_path = resolve_config_path(args.paths.config)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    if uses_default_config {
        load_default_credentials_into_env()?;
    }
    let db_path = resolve_db_path(
        args.paths.db,
        &config_path,
        config.runtime.sqlite_path.as_deref(),
        "recover",
    )?;

    let database = Database::open(&db_path)
        .with_context(|| format!("failed to open SQLite DB {}", db_path.display()))?;
    database
        .run_migrations()
        .context("failed to run DB migrations")?;
    let lock = database
        .acquire_sync_lock("recover")
        .context("failed to acquire sync lock")?;
    database
        .insert_sync_run(&NewSyncRun {
            run_id: "recover",
            mode: "recover",
            status: "running",
        })
        .context("failed to insert recovery audit row")?;

    let toggl_token = std::env::var(&config.toggl.api_token_env)
        .with_context(|| format!("missing env var {}", config.toggl.api_token_env))?;
    let toggl_config =
        TogglClientConfig::from_app_config(&config, toggl_token, config.toggl.base_url.clone())
            .context("failed to build Toggl client config")?;
    let toggl = TogglClient::new(toggl_config).context("failed to build Toggl client")?;
    let since = recovery_since(current_unix_seconds(), config.runtime.recovery_scan_days);
    let fetch = toggl
        .fetch_time_entries_since(since)
        .await
        .context("failed to fetch Toggl entries for recovery")?;

    let recovery_sites = build_recovery_sites(&config)?;
    let issue_site_mappings = resolve_issue_site_mappings(&config, &database, &fetch.entries)
        .await
        .context("failed to resolve Jira issue sites for recovery")?;
    let report = recover(RecoveryInput {
        database: &database,
        entries: fetch.entries,
        issue_site_mappings,
        recovery_sites,
        recovery_scan_days: config.runtime.recovery_scan_days,
        requested_scan_days: None,
    })
    .await
    .context("failed to recover Jira worklog links")?;

    lock.release().context("failed to release sync lock")?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", human_report(&report));
    }

    Ok(())
}

async fn resolve_issue_site_mappings(
    config: &AppConfig,
    database: &Database,
    entries: &[TogglTimeEntry],
) -> anyhow::Result<Vec<IssueSiteMapping>> {
    let resolver = IssueSiteResolver::new(database, build_resolver_sites(config)?);
    let mut issue_keys = entries
        .iter()
        .flat_map(|entry| extract_issue_keys(entry.description.as_deref().unwrap_or_default()))
        .collect::<Vec<_>>();
    issue_keys.sort();
    issue_keys.dedup();

    let mut mappings = Vec::with_capacity(issue_keys.len());
    for issue_key in issue_keys {
        mappings.push(resolver.resolve_issue_key(&issue_key).await?.into());
    }
    Ok(mappings)
}

fn build_resolver_sites(config: &AppConfig) -> anyhow::Result<Vec<ResolverSite>> {
    config
        .enabled_jira_sites()
        .into_iter()
        .map(|site| {
            let email = std::env::var(&site.email_env)
                .with_context(|| format!("missing env var {}", site.email_env))?;
            let token = std::env::var(&site.api_token_env)
                .with_context(|| format!("missing env var {}", site.api_token_env))?;
            Ok(ResolverSite {
                key: site.key.clone(),
                client: JiraClient::from_credentials(site.base_url.clone(), email, token),
            })
        })
        .collect()
}

fn build_recovery_sites(config: &AppConfig) -> anyhow::Result<Vec<RecoverySite>> {
    config
        .enabled_jira_sites()
        .into_iter()
        .map(|site| {
            let email = std::env::var(&site.email_env)
                .with_context(|| format!("missing env var {}", site.email_env))?;
            let token = std::env::var(&site.api_token_env)
                .with_context(|| format!("missing env var {}", site.api_token_env))?;
            Ok(RecoverySite {
                key: site.key.clone(),
                client: JiraClient::from_credentials(site.base_url.clone(), email, token),
            })
        })
        .collect()
}

fn recovery_since(now_unix_seconds: i64, recovery_scan_days: u32) -> i64 {
    now_unix_seconds - i64::from(recovery_scan_days) * SECONDS_PER_DAY
}

fn human_report(report: &RecoveryReport) -> String {
    format!(
        "Recovery scanned {} Toggl entries across {} Jira issues and {} worklogs; recovered {} links; conflicts: {}; warnings: {}",
        report.scanned_entries,
        report.scanned_issues,
        report.scanned_worklogs,
        report.recovered_links,
        report.conflicts.len(),
        report.warnings.len()
    )
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}
