use std::{collections::HashMap, time::Duration};

use anyhow::Context;

use crate::{
    cli::SyncArgs,
    commands::config::{
        load_credentials_from_path, load_default_credentials, resolve_config_path, resolve_db_path,
        LocalCredentials,
    },
    config::{AppConfig, JiraSiteConfig},
    db::{Database, NewSyncRun, NewTogglEntry, StoredJiraWorklogLink},
    jira::{JiraClient, JiraWritePacing, JiraWritePacingScope, TokioSleeper},
    report::DryRunReport,
    sync::{
        executor::{execute_plan, ExecutorOptions, ExecutorReport},
        planner::{
            extract_issue_keys, plan_sync, ExistingWorklogLink, IssueSiteMapping, PlannedMutation,
            PlannerInput, PlannerOutcome, SyncPlan,
        },
        resolver::{IssueSiteResolutionError, IssueSiteResolver, ResolverSite},
    },
    time::{format_unix_utc, parse_rfc3339_utc},
    toggl::{TogglClient, TogglClientConfig, TogglTimeEntry},
};

pub async fn run(args: SyncArgs) -> anyhow::Result<()> {
    let config_path = resolve_config_path(args.paths.config.clone())?;
    let credentials = None;
    run_with_config(args, config_path, credentials).await
}

pub async fn run_with_credentials(
    args: SyncArgs,
    credentials_path: std::path::PathBuf,
) -> anyhow::Result<()> {
    let config_path = resolve_config_path(args.paths.config.clone())?;
    let credentials = Some(load_credentials_from_path(&credentials_path)?);
    run_with_config(args, config_path, credentials).await
}

async fn run_with_config(
    args: SyncArgs,
    config_path: std::path::PathBuf,
    credentials: Option<LocalCredentials>,
) -> anyhow::Result<()> {
    let uses_default_config = config_path == resolve_config_path(None)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let credentials = if let Some(credentials) = credentials {
        credentials
    } else if uses_default_config {
        load_default_credentials()?
    } else {
        LocalCredentials::default()
    };
    let db_path = resolve_db_path(
        args.paths.db,
        &config_path,
        config.runtime.sqlite_path.as_deref(),
        "sync",
    )?;

    let database = Database::open(&db_path)
        .with_context(|| format!("failed to open SQLite DB {}", db_path.display()))?;
    database
        .run_migrations()
        .context("failed to run DB migrations")?;
    let mode = if args.dry_run { "dry_run" } else { "sync" };
    let lock = database
        .acquire_sync_lock(mode)
        .context("failed to acquire sync lock")?;
    let sync_run_id = database
        .insert_sync_run(&NewSyncRun {
            run_id: &format!("{mode}-{}", current_unix_seconds()),
            mode,
            status: if args.dry_run { "planned" } else { "running" },
        })
        .context("failed to insert sync audit row")?;

    let toggl_token = credentials.get_secret(&config.toggl.api_token_env)?;
    let toggl_config =
        TogglClientConfig::from_app_config(&config, toggl_token, config.toggl.base_url.clone())
            .context("failed to build Toggl client config")?;
    let since = toggl_config.initial_backfill_since(current_unix_seconds());
    let toggl = TogglClient::new(toggl_config).context("failed to build Toggl client")?;
    let fetch = toggl
        .fetch_time_entries_since(since)
        .await
        .context("failed to fetch Toggl entries")?;
    if args.cleanup_deleted && !args.dry_run {
        cleanup_missing_toggl_entries(&database, config.toggl.workspace_id, since, &fetch)
            .context("failed to cleanup deleted Toggl entries")?;
    }
    let issue_site_mappings =
        resolve_issue_site_mappings(&config, &credentials, &database, &fetch.entries)
            .await
            .context("failed to resolve Jira issue sites")?;
    let existing_links = load_existing_links(&database).context("failed to load existing links")?;

    let plan = plan_sync(PlannerInput {
        entries: fetch.entries.clone(),
        issue_site_mappings: issue_site_mappings.clone(),
        existing_links: existing_links.clone(),
    })
    .expect("planner does not fail globally");
    record_toggl_entry_ledgers(&database, sync_run_id, &fetch.entries, &plan)
        .context("failed to upsert Toggl entry ledger rows")?;

    if args.dry_run {
        let report = DryRunReport::from_fetch_result_with_resolved_sites(
            fetch,
            issue_site_mappings,
            existing_links,
        );

        lock.release().context("failed to release sync lock")?;

        if args.quiet {
            return Ok(());
        }

        if args.json {
            println!("{}", report.to_json_string()?);
        } else {
            println!("{}", report.to_human_string());
        }

        return Ok(());
    }

    let report = execute_destructive_plan(&config, &credentials, &plan, &database).await?;

    lock.release().context("failed to release sync lock")?;

    if args.quiet {
        return Ok(());
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "mode": "sync",
                "succeeded": report.succeeded,
                "failed": report.failed,
                "statuses": report.statuses.iter().map(|status| format!("{status:?}")).collect::<Vec<_>>(),
            }))?
        );
    } else {
        println!(
            "sync: succeeded={} failed={}",
            report.succeeded, report.failed
        );
    }

    Ok(())
}

fn cleanup_missing_toggl_entries(
    database: &Database,
    workspace_id: i64,
    since: i64,
    fetch: &crate::toggl::TogglFetchResult,
) -> anyhow::Result<()> {
    if fetch
        .skipped
        .iter()
        .any(|skip| skip.reason == crate::toggl::SkipReason::PacingWindow)
    {
        return Ok(());
    }
    let seen_entry_ids = fetch
        .entries
        .iter()
        .map(|entry| entry.entry_id.clone())
        .collect::<Vec<_>>();
    database.mark_missing_toggl_entries_deleted(
        &workspace_id.to_string(),
        &format_unix_utc(since),
        &seen_entry_ids,
    )?;
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

fn load_existing_links(database: &Database) -> anyhow::Result<Vec<ExistingWorklogLink>> {
    Ok(database
        .list_active_jira_worklog_links()?
        .into_iter()
        .filter_map(existing_link_from_stored)
        .collect())
}

fn existing_link_from_stored(link: StoredJiraWorklogLink) -> Option<ExistingWorklogLink> {
    Some(ExistingWorklogLink {
        toggl_workspace_id: link.toggl_workspace_id,
        toggl_entry_id: link.toggl_entry_id,
        jira_site_key: link.jira_site_key,
        jira_issue_key: link.jira_issue_key,
        jira_worklog_id: link.jira_worklog_id?,
        source_hash: link.source_hash,
        marked_toggl_managed: true,
    })
}

fn record_toggl_entry_ledgers(
    database: &Database,
    sync_run_id: i64,
    entries: &[TogglTimeEntry],
    plan: &SyncPlan,
) -> anyhow::Result<()> {
    let outcomes: HashMap<_, _> = plan
        .entries
        .iter()
        .map(|entry| {
            (
                (
                    entry.toggl_workspace_id.as_str(),
                    entry.toggl_entry_id.as_str(),
                ),
                &entry.outcome,
            )
        })
        .collect();

    let mutation_details: HashMap<_, _> = plan
        .mutations
        .iter()
        .map(|mutation| {
            let (workspace, entry, issue_key, source_hash, rounded_duration_seconds) =
                match mutation {
                    PlannedMutation::Create(create) => (
                        create.toggl_workspace_id.as_str(),
                        create.toggl_entry_id.as_str(),
                        create.jira_issue_key.as_str(),
                        create.source_hash.as_str(),
                        create.draft.time_spent_seconds,
                    ),
                    PlannedMutation::Update(update) => (
                        update.toggl_workspace_id.as_str(),
                        update.toggl_entry_id.as_str(),
                        update.jira_issue_key.as_str(),
                        update.source_hash.as_str(),
                        update.draft.time_spent_seconds,
                    ),
                    PlannedMutation::Delete(delete) => (
                        delete.toggl_workspace_id.as_str(),
                        delete.toggl_entry_id.as_str(),
                        delete.jira_issue_key.as_str(),
                        delete.source_hash.as_str(),
                        0,
                    ),
                };
            (
                (workspace, entry),
                (
                    Some(issue_key),
                    source_hash.to_owned(),
                    rounded_duration_seconds,
                ),
            )
        })
        .collect();

    for entry in entries {
        let key = (entry.workspace_id.as_str(), entry.entry_id.as_str());
        let (issue_key, source_hash, rounded_duration_seconds) = mutation_details
            .get(&key)
            .cloned()
            .unwrap_or_else(|| (None, fallback_source_hash(entry), rounded_duration(entry)));
        let outcome = outcomes.get(&key);
        let fallback_issue_key = ledger_issue_key(entry, outcome.copied());
        let ledger_issue_key = issue_key.or(fallback_issue_key.as_deref());
        let status = match outcome {
            Some(PlannerOutcome::Error(_)) => "error",
            _ => "planned",
        };

        database.upsert_toggl_entry(&NewTogglEntry {
            toggl_workspace_id: &entry.workspace_id,
            toggl_entry_id: &entry.entry_id,
            description: entry.description.as_deref(),
            extracted_issue_key: ledger_issue_key,
            source_hash: &source_hash,
            rounded_duration_seconds,
            status,
            started_at: Some(&entry.start),
            stopped_at: stopped_at(entry).as_deref(),
        })?;

        if entry.deleted_at.is_some() {
            database.mark_toggl_entry_deleted(&entry.workspace_id, &entry.entry_id)?;
        }

        if let Some(PlannerOutcome::Error(issue)) = outcome {
            let attempt_issue_key = planner_issue_key(issue);
            database.insert_sync_attempt(&crate::db::NewSyncAttempt {
                sync_run_id: Some(sync_run_id),
                toggl_workspace_id: &entry.workspace_id,
                toggl_entry_id: &entry.entry_id,
                jira_site_key: None,
                jira_issue_key: attempt_issue_key,
                jira_worklog_id: None,
                status: "error",
                error_message: Some(&issue.to_string()),
            })?;
        } else if let Some((Some(issue_key), _, _)) = mutation_details.get(&key) {
            let site_key = planned_site_key(&plan.mutations, &entry.workspace_id, &entry.entry_id);
            database.insert_sync_attempt(&crate::db::NewSyncAttempt {
                sync_run_id: Some(sync_run_id),
                toggl_workspace_id: &entry.workspace_id,
                toggl_entry_id: &entry.entry_id,
                jira_site_key: site_key,
                jira_issue_key: Some(issue_key),
                jira_worklog_id: None,
                status: "planned",
                error_message: None,
            })?;
        }
    }

    Ok(())
}

fn ledger_issue_key(entry: &TogglTimeEntry, outcome: Option<&PlannerOutcome>) -> Option<String> {
    if let Some(PlannerOutcome::Error(issue)) = outcome {
        if let Some(issue_key) = planner_issue_key(issue) {
            return Some(issue_key.to_owned());
        }
    }

    let description = entry.description.as_deref()?;
    let issue_keys = extract_issue_keys(description);
    match issue_keys.as_slice() {
        [issue_key] => Some(issue_key.clone()),
        _ => None,
    }
}

fn planner_issue_key(issue: &crate::sync::planner::PlannerIssue) -> Option<&str> {
    match issue {
        crate::sync::planner::PlannerIssue::UnresolvedIssueSite { issue_key }
        | crate::sync::planner::PlannerIssue::AmbiguousIssueSite { issue_key } => {
            Some(issue_key.as_str())
        }
        crate::sync::planner::PlannerIssue::MultipleIssueKeys { .. } => None,
    }
}

fn planned_site_key<'a>(
    mutations: &'a [PlannedMutation],
    workspace_id: &str,
    entry_id: &str,
) -> Option<&'a str> {
    mutations.iter().find_map(|mutation| match mutation {
        PlannedMutation::Create(create)
            if create.toggl_workspace_id == workspace_id && create.toggl_entry_id == entry_id =>
        {
            Some(create.jira_site_key.as_str())
        }
        PlannedMutation::Update(update)
            if update.toggl_workspace_id == workspace_id && update.toggl_entry_id == entry_id =>
        {
            Some(update.jira_site_key.as_str())
        }
        PlannedMutation::Delete(delete)
            if delete.toggl_workspace_id == workspace_id && delete.toggl_entry_id == entry_id =>
        {
            Some(delete.jira_site_key.as_str())
        }
        _ => None,
    })
}

fn stopped_at(entry: &TogglTimeEntry) -> Option<String> {
    if entry.duration_seconds <= 0 {
        return None;
    }

    let timestamp = parse_rfc3339_utc(&entry.start)? + entry.duration_seconds;
    Some(format_unix_utc(timestamp))
}

fn fallback_source_hash(entry: &TogglTimeEntry) -> String {
    format!(
        "sha256:local:{}:{}:{}",
        entry.workspace_id, entry.entry_id, entry.updated_at
    )
}

fn rounded_duration(entry: &TogglTimeEntry) -> i64 {
    if entry.duration_seconds <= 0 {
        return 0;
    }
    ((entry.duration_seconds + 30) / 60) * 60
}

async fn execute_destructive_plan(
    config: &AppConfig,
    credentials: &LocalCredentials,
    plan: &SyncPlan,
    database: &Database,
) -> anyhow::Result<ExecutorReport> {
    let mut combined = ExecutorReport::default();
    let write_pacing_scope = JiraWritePacingScope::default();
    for site in config.enabled_jira_sites() {
        let site_plan = plan_for_site(plan, &site.key);
        if site_plan.mutations.is_empty() {
            continue;
        }
        let client = jira_client_for_site(config, credentials, site, write_pacing_scope.clone())?;
        let report = execute_plan(&site_plan, &client, database, ExecutorOptions::default())
            .await
            .with_context(|| format!("failed to execute Jira sync for site {}", site.key))?;
        combined.succeeded += report.succeeded;
        combined.failed += report.failed;
        combined.statuses.extend(report.statuses);
        combined.conflicts.extend(report.conflicts);
    }
    Ok(combined)
}

fn plan_for_site(plan: &SyncPlan, site_key: &str) -> SyncPlan {
    SyncPlan {
        entries: Vec::new(),
        mutations: plan
            .mutations
            .iter()
            .filter(|mutation| mutation_site_key(mutation) == site_key)
            .cloned()
            .collect(),
    }
}

fn mutation_site_key(mutation: &PlannedMutation) -> &str {
    match mutation {
        PlannedMutation::Create(create) => &create.jira_site_key,
        PlannedMutation::Update(update) => &update.jira_site_key,
        PlannedMutation::Delete(delete) => &delete.jira_site_key,
    }
}

fn jira_client_for_site(
    config: &AppConfig,
    credentials: &LocalCredentials,
    site: &JiraSiteConfig,
    write_pacing_scope: JiraWritePacingScope,
) -> anyhow::Result<JiraClient<TokioSleeper>> {
    let email = credentials.get_secret(&site.email_env)?;
    let api_token = credentials.get_secret(&site.api_token_env)?;
    Ok(JiraClient::new_with_pacing_scope(
        site.base_url.clone(),
        email,
        api_token,
        TokioSleeper,
        JiraWritePacing {
            global_write_delay: Duration::from_millis(
                config.rate_limits.jira_global_write_delay_ms,
            ),
            same_issue_write_delay: Duration::from_millis(
                config.rate_limits.jira_same_issue_write_delay_ms,
            ),
        },
        write_pacing_scope,
    ))
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}
