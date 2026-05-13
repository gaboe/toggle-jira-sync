use anyhow::Context;

use crate::{
    cli::RecoverArgs,
    commands::config::{
        load_default_credentials, resolve_config_path, resolve_db_path, LocalCredentials,
    },
    config::AppConfig,
    db::{Database, NewSyncRun},
    jira::JiraClient,
    sync::{
        planner::{extract_issue_keys, IssueSiteMapping},
        recovery::{recover, RecoveryInput, RecoveryReport, RecoverySite},
        resolver::{IssueSiteResolutionError, IssueSiteResolver, ResolverSite},
    },
    toggl::{TogglClient, TogglClientConfig, TogglTimeEntry},
};

const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

pub async fn run(args: RecoverArgs) -> anyhow::Result<()> {
    let uses_default_config = args.paths.config.is_none();
    let config_path = resolve_config_path(args.paths.config)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let credentials = if uses_default_config {
        load_default_credentials()?
    } else {
        LocalCredentials::default()
    };
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
            run_id: &format!("recover-{}", current_unix_seconds()),
            mode: "recover",
            status: "running",
        })
        .context("failed to insert recovery audit row")?;

    let toggl_token = credentials.get_secret(&config.toggl.api_token_env)?;
    let toggl_config =
        TogglClientConfig::from_app_config(&config, toggl_token, config.toggl.base_url.clone())
            .context("failed to build Toggl client config")?;
    let toggl = TogglClient::new(toggl_config).context("failed to build Toggl client")?;
    let since = recovery_since(
        current_unix_seconds(),
        config.runtime.recovery_from_month.as_deref(),
        config.runtime.recovery_scan_days,
    );
    let fetch = toggl
        .fetch_time_entries_since(since)
        .await
        .context("failed to fetch Toggl entries for recovery")?;

    let recovery_sites = build_recovery_sites(&config, &credentials)?;
    let issue_site_mappings =
        resolve_issue_site_mappings(&config, &credentials, &database, &fetch.entries)
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
    credentials: &LocalCredentials,
    database: &Database,
    entries: &[TogglTimeEntry],
) -> anyhow::Result<Vec<IssueSiteMapping>> {
    let resolver = IssueSiteResolver::new(database, build_resolver_sites(config, credentials)?);
    let mut issue_keys = entries
        .iter()
        .flat_map(|entry| extract_issue_keys(entry.description.as_deref().unwrap_or_default()))
        .collect::<Vec<_>>();
    issue_keys.sort();
    issue_keys.dedup();

    let mut mappings = Vec::with_capacity(issue_keys.len());
    for issue_key in issue_keys {
        match resolver.resolve_issue_key(&issue_key).await {
            Ok(resolved) => mappings.push(resolved.into()),
            Err(
                IssueSiteResolutionError::NoMatchingSite { .. }
                | IssueSiteResolutionError::MultipleMatchingSites { .. },
            ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(mappings)
}

fn build_resolver_sites(
    config: &AppConfig,
    credentials: &LocalCredentials,
) -> anyhow::Result<Vec<ResolverSite>> {
    config
        .enabled_jira_sites()
        .into_iter()
        .map(|site| {
            let email = credentials.get_secret(&site.email_env)?;
            let token = credentials.get_secret(&site.api_token_env)?;
            Ok(ResolverSite {
                key: site.key.clone(),
                client: JiraClient::from_credentials(site.base_url.clone(), email, token),
            })
        })
        .collect()
}

fn build_recovery_sites(
    config: &AppConfig,
    credentials: &LocalCredentials,
) -> anyhow::Result<Vec<RecoverySite>> {
    config
        .enabled_jira_sites()
        .into_iter()
        .map(|site| {
            let email = credentials.get_secret(&site.email_env)?;
            let token = credentials.get_secret(&site.api_token_env)?;
            Ok(RecoverySite {
                key: site.key.clone(),
                client: JiraClient::from_credentials(site.base_url.clone(), email, token),
            })
        })
        .collect()
}

fn recovery_since(
    now_unix_seconds: i64,
    recovery_from_month: Option<&str>,
    recovery_scan_days: u32,
) -> i64 {
    if let Some(month) = recovery_from_month.and_then(crate::time::month_start_since) {
        return month;
    }
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
