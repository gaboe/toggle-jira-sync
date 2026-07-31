use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::{
    cli::SyncArgs,
    commands::config::{
        load_credentials_from_path, load_default_credentials, load_isolated_credentials_from_path,
        resolve_config_path, resolve_db_path, LocalCredentials,
    },
    config::{AppConfig, JiraSiteConfig},
    db::{
        Database, NewSyncRun, NewTogglEntry, StoredJiraWorklogLink, StoredLinkedLocalTogglEntry,
        StoredOpenTogglEntry, StoredPendingLocalTogglEntry, StoredRetryableTogglEntry,
    },
    jira::{JiraClient, JiraWritePacing, JiraWritePacingScope, TokioSleeper},
    local_api::LocalServer,
    report::DryRunReport,
    sync::{
        executor::{execute_plan, ExecutorFailurePolicy, ExecutorOptions, ExecutorReport},
        planner::{
            extract_issue_keys, plan_sync, ExistingWorklogLink, IssueSiteMapping, PlannedMutation,
            PlannerInput, PlannerOutcome, SyncPlan,
        },
        resolver::{IssueSiteResolutionError, IssueSiteResolver, ResolverSite},
    },
    time::{format_unix_utc, initial_backfill_since, parse_rfc3339_utc, unique_run_id},
    toggl::{TogglClient, TogglClientConfig, TogglTimeEntry},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SyncCommandReport {
    DryRun {
        report: DryRunReport,
    },
    Sync {
        succeeded: usize,
        failed: usize,
        statuses: Vec<String>,
    },
}

impl SyncCommandReport {
    pub fn print(&self, json: bool) -> anyhow::Result<()> {
        match self {
            Self::DryRun { report } => {
                if json {
                    println!("{}", report.to_json_string()?);
                } else {
                    println!("{}", report.to_human_string());
                }
            }
            Self::Sync {
                succeeded,
                failed,
                statuses,
            } => {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "mode": "sync",
                            "succeeded": succeeded,
                            "failed": failed,
                            "statuses": statuses,
                        }))?
                    );
                } else {
                    println!("sync: succeeded={succeeded} failed={failed}");
                }
            }
        }
        Ok(())
    }
}

pub async fn run(args: SyncArgs) -> anyhow::Result<()> {
    let server = LocalServer::start(args.paths.clone(), None, 200).await?;
    let report = server
        .client()
        .sync_command(args.dry_run, args.cleanup_deleted)
        .await?;
    if !args.quiet {
        report.print(args.json)?;
    }
    Ok(())
}

pub async fn run_direct(args: SyncArgs) -> anyhow::Result<()> {
    let json = args.json;
    let quiet = args.quiet;
    let report = sync_report(args, None).await?;
    if !quiet {
        report.print(json)?;
    }
    Ok(())
}

pub async fn run_with_credentials(args: SyncArgs, credentials_path: PathBuf) -> anyhow::Result<()> {
    let json = args.json;
    let quiet = args.quiet;
    let report = sync_report(args, Some(load_credentials_from_path(&credentials_path)?)).await?;
    if !quiet {
        report.print(json)?;
    }
    Ok(())
}

pub async fn run_with_isolated_credentials(
    args: SyncArgs,
    credentials_path: PathBuf,
) -> anyhow::Result<()> {
    let json = args.json;
    let quiet = args.quiet;
    let report = sync_report(
        args,
        Some(load_isolated_credentials_from_path(&credentials_path)?),
    )
    .await?;
    if !quiet {
        report.print(json)?;
    }
    Ok(())
}

pub(crate) async fn sync_report(
    args: SyncArgs,
    credentials: Option<LocalCredentials>,
) -> anyhow::Result<SyncCommandReport> {
    let config_path = resolve_config_path(args.paths.config.clone())?;
    run_with_config(args, config_path, credentials).await
}

async fn run_with_config(
    args: SyncArgs,
    config_path: PathBuf,
    credentials: Option<LocalCredentials>,
) -> anyhow::Result<SyncCommandReport> {
    let uses_default_config = config_path == resolve_config_path(None)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let credentials = if let Some(credentials) = credentials {
        credentials
    } else if uses_default_config {
        load_default_credentials()?
    } else {
        LocalCredentials::process_env()
    };
    let db_path = resolve_db_path(
        args.paths.db,
        &config_path,
        config.runtime.sqlite_path.as_deref(),
        "sync",
    )?;

    let database = Database::open(&db_path)
        .with_context(|| format!("failed to open local DB {}", db_path.display()))?;
    database
        .run_migrations()
        .context("failed to run DB migrations")?;
    let mode = if args.dry_run { "dry_run" } else { "sync" };
    let run_id = unique_run_id(mode);
    let lock = database
        .acquire_sync_lock(mode)
        .context("failed to acquire sync lock")?;
    let sync_run_id = database
        .insert_sync_run(&NewSyncRun {
            run_id: &run_id,
            mode,
            status: if args.dry_run { "planned" } else { "running" },
        })
        .context("failed to insert sync audit row")?;

    let sync_result: anyhow::Result<SyncCommandReport> = async {
        let toggl_token = credentials.get_secret(&config.toggl.api_token_env)?;
        let toggl_config =
            TogglClientConfig::from_app_config(&config, toggl_token, config.toggl.base_url.clone())
                .context("failed to build Toggl client config")?;
        let now = current_unix_seconds();
        let since = toggl_config.initial_backfill_since(now);
        let toggl = TogglClient::new(toggl_config).context("failed to build Toggl client")?;
        let fetch = toggl
            .fetch_time_entries_since(since)
            .await
            .context("failed to fetch Toggl entries")?;
        let prepared_fetch = fetch_entries_for_planning(&database, &toggl, &config, now, &fetch)
            .await
            .context("failed to prepare Toggl entries for planning")?;
        if args.cleanup_deleted && !args.dry_run {
            cleanup_missing_toggl_entries(&database, config.toggl.workspace_id, since, &fetch)
                .context("failed to cleanup deleted Toggl entries")?;
        }
        let issue_site_mappings =
            resolve_issue_site_mappings(&config, &credentials, &database, &prepared_fetch.entries)
                .await
                .context("failed to resolve Jira issue sites")?;
        let existing_links =
            load_existing_links(&database).context("failed to load existing links")?;

        let plan = plan_sync(PlannerInput {
            entries: prepared_fetch.entries.clone(),
            issue_site_mappings: issue_site_mappings.clone(),
            existing_links: existing_links.clone(),
        })
        .expect("planner does not fail globally");
        record_toggl_entry_ledgers(&database, sync_run_id, &prepared_fetch.entries, &plan)
            .context("failed to upsert Toggl entry ledger rows")?;

        if args.dry_run {
            let report = DryRunReport::from_fetch_result_with_resolved_sites(
                crate::toggl::TogglFetchResult {
                    entries: prepared_fetch.entries,
                    skipped: prepared_fetch.skipped,
                },
                issue_site_mappings,
                existing_links,
            );

            return Ok(SyncCommandReport::DryRun { report });
        }

        let report =
            execute_destructive_plan(&config, &credentials, &plan, &database, sync_run_id).await?;

        Ok(SyncCommandReport::Sync {
            succeeded: report.succeeded,
            failed: report.failed,
            statuses: report
                .statuses
                .iter()
                .map(|status| format!("{status:?}"))
                .collect(),
        })
    }
    .await;

    match &sync_result {
        Ok(SyncCommandReport::Sync { failed, .. }) if *failed > 0 => database
            .finish_sync_run(
                sync_run_id,
                "error",
                Some("one or more Jira mutations failed"),
            )
            .context("failed to finish sync audit row")?,
        Ok(_) => database
            .finish_sync_run(sync_run_id, "completed", None)
            .context("failed to finish sync audit row")?,
        Err(error) => database
            .finish_sync_run(sync_run_id, "error", Some(&error.to_string()))
            .context("failed to finish sync audit row")?,
    }

    lock.release().context("failed to release sync lock")?;

    sync_result
}

fn entries_with_pending_local_entries(
    database: &Database,
    workspace_id: i64,
    fetched_entries: &[TogglTimeEntry],
) -> anyhow::Result<Vec<TogglTimeEntry>> {
    let mut entries = fetched_entries.to_vec();
    let mut seen_keys = fetched_entries
        .iter()
        .map(|entry| (entry.workspace_id.clone(), entry.entry_id.clone()))
        .collect::<std::collections::HashSet<_>>();

    for retryable in database.list_retryable_error_toggl_entries(&workspace_id.to_string())? {
        if !seen_keys.insert((retryable.workspace.clone(), retryable.entry.clone())) {
            continue;
        }
        entries.push(retryable_toggl_entry(retryable));
    }

    for pending in database.list_pending_local_toggl_entries(&workspace_id.to_string())? {
        if !seen_keys.insert((pending.workspace.clone(), pending.entry.clone())) {
            continue;
        }
        entries.push(pending_local_toggl_entry(pending));
    }

    Ok(entries)
}

struct PreparedTogglFetch {
    entries: Vec<TogglTimeEntry>,
    skipped: Vec<crate::toggl::TogglFetchSkip>,
}

async fn fetch_entries_for_planning(
    database: &Database,
    toggl: &TogglClient,
    config: &AppConfig,
    now: i64,
    primary_fetch: &crate::toggl::TogglFetchResult,
) -> anyhow::Result<PreparedTogglFetch> {
    let workspace_id = config.toggl.workspace_id.to_string();
    let ledger_is_empty = database.count_toggl_entries(&workspace_id)? == 0;
    let mut entries = primary_fetch.entries.clone();
    let mut skipped = primary_fetch.skipped.clone();

    if ledger_is_empty && primary_fetch.entries.is_empty() {
        wait_for_toggl_pacing(config).await;
        let fallback = toggl
            .fetch_time_entries_since(initial_backfill_since(
                now,
                config.runtime.initial_backfill_days,
            ))
            .await
            .context("failed to fetch fallback Toggl backfill")?;
        merge_fresh_toggl_entries(&mut entries, fallback.entries);
        skipped.extend(fallback.skipped);
    }

    let open_entries = database.list_open_zero_duration_toggl_entries(&workspace_id)?;
    if let Some(oldest_since) = oldest_missing_open_entry_since(&open_entries, &entries) {
        wait_for_toggl_pacing(config).await;
        let refresh = toggl
            .fetch_time_entries_since(oldest_since)
            .await
            .context("failed to refresh open Toggl entries")?;
        merge_fresh_toggl_entries(&mut entries, refresh.entries);
        skipped.extend(refresh.skipped);
    }

    for entry_id in missing_open_entry_ids(&open_entries, &entries) {
        wait_for_toggl_pacing(config).await;
        let refresh = toggl
            .fetch_time_entry_by_id(&entry_id)
            .await
            .with_context(|| format!("failed to refresh Toggl entry {entry_id}"))?;
        merge_fresh_toggl_entries(&mut entries, refresh.entries);
        skipped.extend(refresh.skipped);
    }

    let active_linked_entries = database.list_active_linked_local_toggl_entries_since(
        &workspace_id,
        &format_unix_utc(initial_backfill_since(
            now,
            config.runtime.initial_backfill_days,
        )),
    )?;
    if let Some(oldest_since) = oldest_missing_linked_entry_since(&active_linked_entries, &entries)
    {
        wait_for_toggl_pacing(config).await;
        let refresh = toggl
            .fetch_time_entries_since(oldest_since)
            .await
            .context("failed to refresh linked Toggl entries")?;
        let skipped_pacing = refresh
            .skipped
            .iter()
            .any(|skip| skip.reason == crate::toggl::SkipReason::PacingWindow);
        merge_fresh_toggl_entries(&mut entries, refresh.entries);
        skipped.extend(refresh.skipped);
        if !skipped_pacing {
            let deleted_entries = missing_linked_entries(&active_linked_entries, &entries)
                .into_iter()
                .map(deleted_linked_toggl_entry)
                .collect::<Vec<_>>();
            merge_fresh_toggl_entries(&mut entries, deleted_entries);
        }
    }

    Ok(PreparedTogglFetch {
        entries: entries_with_pending_local_entries(database, config.toggl.workspace_id, &entries)?,
        skipped,
    })
}

fn merge_fresh_toggl_entries(
    entries: &mut Vec<TogglTimeEntry>,
    fresh_entries: Vec<TogglTimeEntry>,
) {
    let fresh_keys = fresh_entries
        .iter()
        .map(|entry| (entry.workspace_id.clone(), entry.entry_id.clone()))
        .collect::<HashSet<_>>();
    entries.retain(|entry| {
        !fresh_keys.contains(&(entry.workspace_id.clone(), entry.entry_id.clone()))
    });
    entries.extend(fresh_entries);
}

fn oldest_missing_open_entry_since(
    open_entries: &[StoredOpenTogglEntry],
    fetched_entries: &[TogglTimeEntry],
) -> Option<i64> {
    let fetched_keys = fetched_entries
        .iter()
        .map(|entry| (entry.workspace_id.as_str(), entry.entry_id.as_str()))
        .collect::<HashSet<_>>();

    open_entries
        .iter()
        .filter(|entry| !fetched_keys.contains(&(entry.workspace.as_str(), entry.entry.as_str())))
        .filter_map(|entry| parse_rfc3339_utc(&entry.started_at))
        .min()
}

fn missing_open_entry_ids(
    open_entries: &[StoredOpenTogglEntry],
    fetched_entries: &[TogglTimeEntry],
) -> Vec<String> {
    let fetched_keys = fetched_entries
        .iter()
        .map(|entry| (entry.workspace_id.as_str(), entry.entry_id.as_str()))
        .collect::<HashSet<_>>();

    open_entries
        .iter()
        .filter(|entry| !fetched_keys.contains(&(entry.workspace.as_str(), entry.entry.as_str())))
        .map(|entry| entry.entry.clone())
        .collect()
}

fn oldest_missing_linked_entry_since(
    linked_entries: &[StoredLinkedLocalTogglEntry],
    fetched_entries: &[TogglTimeEntry],
) -> Option<i64> {
    missing_linked_entries(linked_entries, fetched_entries)
        .iter()
        .filter_map(|entry| parse_rfc3339_utc(&entry.started_at))
        .min()
}

fn missing_linked_entries(
    linked_entries: &[StoredLinkedLocalTogglEntry],
    fetched_entries: &[TogglTimeEntry],
) -> Vec<StoredLinkedLocalTogglEntry> {
    let fetched_keys = fetched_entries
        .iter()
        .map(|entry| (entry.workspace_id.as_str(), entry.entry_id.as_str()))
        .collect::<HashSet<_>>();

    linked_entries
        .iter()
        .filter(|entry| !fetched_keys.contains(&(entry.workspace.as_str(), entry.entry.as_str())))
        .cloned()
        .collect()
}

async fn wait_for_toggl_pacing(config: &AppConfig) {
    tokio::time::sleep(Duration::from_secs_f64(
        1.0 / config.rate_limits.toggl_max_rps,
    ))
    .await;
}

fn retryable_toggl_entry(entry: StoredRetryableTogglEntry) -> TogglTimeEntry {
    TogglTimeEntry {
        workspace_id: entry.workspace,
        entry_id: entry.entry,
        start: entry.started_at.clone(),
        duration_seconds: entry.rounded_duration_seconds,
        description: entry.description,
        updated_at: entry.stopped_at.unwrap_or(entry.started_at),
        deleted_at: None,
        skip_reason: None,
    }
}

fn pending_local_toggl_entry(entry: StoredPendingLocalTogglEntry) -> TogglTimeEntry {
    TogglTimeEntry {
        workspace_id: entry.workspace,
        entry_id: entry.entry,
        start: entry.started_at.clone(),
        duration_seconds: entry.rounded_duration_seconds,
        description: entry.description,
        updated_at: entry.stopped_at.unwrap_or(entry.started_at),
        deleted_at: None,
        skip_reason: None,
    }
}

fn deleted_linked_toggl_entry(entry: StoredLinkedLocalTogglEntry) -> TogglTimeEntry {
    let stopped_at = entry.stopped_at.unwrap_or_else(|| entry.started_at.clone());
    TogglTimeEntry {
        workspace_id: entry.workspace,
        entry_id: entry.entry,
        start: entry.started_at.clone(),
        duration_seconds: entry.rounded_duration_seconds,
        description: entry.description,
        updated_at: stopped_at.clone(),
        deleted_at: Some(stopped_at),
        skip_reason: None,
    }
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
        // Only 'planned', 'error', and 'deleted' are ever stored here. Sync success is
        // recorded in jira_worklog_links, which is what the status report and the pending
        // query both read, so this column stays at 'planned' for synced entries.
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
    sync_run_id: i64,
) -> anyhow::Result<ExecutorReport> {
    let mut combined = ExecutorReport::default();
    let write_pacing_scope = JiraWritePacingScope::default();
    for site in config.enabled_jira_sites() {
        let site_plan = plan_for_site(plan, &site.key);
        if site_plan.mutations.is_empty() {
            continue;
        }
        let client = jira_client_for_site(config, credentials, site, write_pacing_scope.clone())?;
        let report = execute_plan(
            &site_plan,
            &client,
            database,
            ExecutorOptions {
                sync_run_id: Some(sync_run_id),
                max_parallel_groups: config.rate_limits.jira_max_parallel_groups,
                failure_policy: ExecutorFailurePolicy::ContinueIndependent,
                ..ExecutorOptions::default()
            },
        )
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
            max_rate_limit_retries: config.rate_limits.jira_max_rate_limit_retries,
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

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::{Method, MockServer};
    use serde_json::json;

    fn test_config(toggl_base_url: &str) -> AppConfig {
        AppConfig::from_toml_str(&format!(
            r#"
[toggl]
workspace_id = 700001
api_token_env = "TOGGL_API_TOKEN"
base_url = "{toggl_base_url}"

[runtime]
initial_backfill_days = 90

[rate_limits]
toggl_max_rps = 1000.0

[jira]

[[jira.sites]]
key = "sabservis"
base_url = "https://sabservis.atlassian.net"
email_env = "SABSERVIS_JIRA_EMAIL"
api_token_env = "SABSERVIS_JIRA_API_TOKEN"
enabled = true
"#
        ))
        .expect("test config should parse")
    }

    fn test_toggl(config: &AppConfig) -> TogglClient {
        let toggl_config = TogglClientConfig::from_app_config(
            config,
            "fake-toggl-token-do-not-log".to_owned(),
            config.toggl.base_url.clone(),
        )
        .expect("toggl config should build");
        TogglClient::new(toggl_config).expect("toggl client should build")
    }

    #[tokio::test]
    async fn first_run_fallback_fetches_bounded_window_after_empty_month() {
        let database = Database::open(":memory:").expect("open in-memory db");
        database.run_migrations().expect("run migrations");
        let server = MockServer::start();
        let config = test_config(&server.base_url());
        let toggl = test_toggl(&config);
        let now = 1_714_604_800;
        let fallback_since = initial_backfill_since(now, config.runtime.initial_backfill_days);
        let fallback = server.mock(|when, then| {
            when.method(Method::GET)
                .path("/api/v9/me/time_entries")
                .query_param("since", fallback_since.to_string().as_str());
            then.status(200).json_body(json!([
                {
                    "id": 900001,
                    "workspace_id": 700001,
                    "description": "SAB-123 fallback entry",
                    "start": "2024-04-25T10:00:00Z",
                    "duration": 1800,
                    "updated_at": "2024-04-25T10:30:00Z"
                }
            ]));
        });

        let prepared = fetch_entries_for_planning(
            &database,
            &toggl,
            &config,
            now,
            &crate::toggl::TogglFetchResult {
                entries: Vec::new(),
                skipped: Vec::new(),
            },
        )
        .await
        .expect("fallback fetch should prepare entries");

        assert_eq!(prepared.entries.len(), 1);
        assert_eq!(prepared.entries[0].entry_id, "900001");
        fallback.assert_hits(1);
    }

    #[tokio::test]
    async fn first_run_does_not_fallback_when_month_fetch_has_entries() {
        let database = Database::open(":memory:").expect("open in-memory db");
        database.run_migrations().expect("run migrations");
        let server = MockServer::start();
        let config = test_config(&server.base_url());
        let toggl = test_toggl(&config);
        let unexpected_fetch = server.mock(|when, then| {
            when.method(Method::GET).path("/api/v9/me/time_entries");
            then.status(500).body("unexpected Toggl fetch");
        });

        let prepared = fetch_entries_for_planning(
            &database,
            &toggl,
            &config,
            1_714_604_800,
            &crate::toggl::TogglFetchResult {
                entries: vec![TogglTimeEntry {
                    workspace_id: "700001".to_owned(),
                    entry_id: "900001".to_owned(),
                    start: "2024-05-01T10:00:00Z".to_owned(),
                    duration_seconds: 1800,
                    description: Some("SAB-123 current entry".to_owned()),
                    updated_at: "2024-05-01T10:30:00Z".to_owned(),
                    deleted_at: None,
                    skip_reason: None,
                }],
                skipped: Vec::new(),
            },
        )
        .await
        .expect("primary entries should prepare without fallback");

        assert_eq!(prepared.entries.len(), 1);
        assert_eq!(unexpected_fetch.hits(), 0);
    }

    #[tokio::test]
    async fn running_refresh_replaces_stale_open_entry() {
        let database = Database::open(":memory:").expect("open in-memory db");
        database.run_migrations().expect("run migrations");
        database
            .upsert_toggl_entry(&NewTogglEntry {
                toggl_workspace_id: "700001",
                toggl_entry_id: "900004",
                description: Some("SAB-555 running entry"),
                extracted_issue_key: Some("SAB-555"),
                source_hash: "sha256:old",
                rounded_duration_seconds: 0,
                status: "planned",
                started_at: Some("2026-05-30T10:00:00Z"),
                stopped_at: None,
            })
            .expect("seed running entry");
        let server = MockServer::start();
        let config = test_config(&server.base_url());
        let toggl = test_toggl(&config);
        let refresh = server.mock(|when, then| {
            when.method(Method::GET)
                .path("/api/v9/me/time_entries")
                .query_param("since", "1780135200");
            then.status(200).json_body(json!([
                {
                    "id": 900004,
                    "workspace_id": 700001,
                    "description": "SAB-555 running entry",
                    "start": "2026-05-30T10:00:00Z",
                    "duration": 3600,
                    "updated_at": "2026-05-30T11:00:00Z"
                }
            ]));
        });

        let prepared = fetch_entries_for_planning(
            &database,
            &toggl,
            &config,
            1_780_272_000,
            &crate::toggl::TogglFetchResult {
                entries: Vec::new(),
                skipped: Vec::new(),
            },
        )
        .await
        .expect("running refresh should prepare entries");

        assert_eq!(prepared.entries.len(), 1);
        assert_eq!(prepared.entries[0].duration_seconds, 3600);
        refresh.assert_hits(1);
    }

    #[tokio::test]
    async fn running_refresh_fetches_still_missing_entry_by_id() {
        let database = Database::open(":memory:").expect("open in-memory db");
        database.run_migrations().expect("run migrations");
        database
            .upsert_toggl_entry(&NewTogglEntry {
                toggl_workspace_id: "700001",
                toggl_entry_id: "900004",
                description: Some("SAB-555 running entry"),
                extracted_issue_key: Some("SAB-555"),
                source_hash: "sha256:old",
                rounded_duration_seconds: 0,
                status: "planned",
                started_at: Some("2026-05-30T10:00:00Z"),
                stopped_at: None,
            })
            .expect("seed running entry");
        let server = MockServer::start();
        let config = test_config(&server.base_url());
        let toggl = test_toggl(&config);
        let range_refresh = server.mock(|when, then| {
            when.method(Method::GET)
                .path("/api/v9/me/time_entries")
                .query_param("since", "1780135200");
            then.status(200).json_body(json!([]));
        });
        let direct_refresh = server.mock(|when, then| {
            when.method(Method::GET)
                .path("/api/v9/me/time_entries/900004");
            then.status(200).json_body(json!({
                "id": 900004,
                "workspace_id": 700001,
                "description": "SAB-555 running entry",
                "start": "2026-05-30T10:00:00Z",
                "duration": 3600,
                "updated_at": "2026-05-30T11:00:00Z"
            }));
        });

        let prepared = fetch_entries_for_planning(
            &database,
            &toggl,
            &config,
            1_780_272_000,
            &crate::toggl::TogglFetchResult {
                entries: Vec::new(),
                skipped: Vec::new(),
            },
        )
        .await
        .expect("direct running refresh should prepare entries");

        assert_eq!(prepared.entries.len(), 1);
        assert_eq!(prepared.entries[0].duration_seconds, 3600);
        range_refresh.assert_hits(1);
        direct_refresh.assert_hits(1);
    }

    #[tokio::test]
    async fn running_refresh_is_skipped_when_entry_is_already_fetched() {
        let database = Database::open(":memory:").expect("open in-memory db");
        database.run_migrations().expect("run migrations");
        database
            .upsert_toggl_entry(&NewTogglEntry {
                toggl_workspace_id: "700001",
                toggl_entry_id: "900004",
                description: Some("SAB-555 running entry"),
                extracted_issue_key: Some("SAB-555"),
                source_hash: "sha256:old",
                rounded_duration_seconds: 0,
                status: "planned",
                started_at: Some("2026-05-30T10:00:00Z"),
                stopped_at: None,
            })
            .expect("seed running entry");
        let server = MockServer::start();
        let config = test_config(&server.base_url());
        let toggl = test_toggl(&config);
        let unexpected_fetch = server.mock(|when, then| {
            when.method(Method::GET).path("/api/v9/me/time_entries");
            then.status(500).body("unexpected Toggl fetch");
        });

        let prepared = fetch_entries_for_planning(
            &database,
            &toggl,
            &config,
            1_780_272_000,
            &crate::toggl::TogglFetchResult {
                entries: vec![TogglTimeEntry {
                    workspace_id: "700001".to_owned(),
                    entry_id: "900004".to_owned(),
                    start: "2026-05-30T10:00:00Z".to_owned(),
                    duration_seconds: 3600,
                    description: Some("SAB-555 running entry".to_owned()),
                    updated_at: "2026-05-30T11:00:00Z".to_owned(),
                    deleted_at: None,
                    skip_reason: None,
                }],
                skipped: Vec::new(),
            },
        )
        .await
        .expect("fetched running entry should prepare");

        assert_eq!(prepared.entries.len(), 1);
        assert_eq!(unexpected_fetch.hits(), 0);
    }

    #[tokio::test]
    async fn linked_historical_refresh_fetches_missing_entries_by_range() {
        let database = Database::open(":memory:").expect("open in-memory db");
        database.run_migrations().expect("run migrations");
        database
            .upsert_toggl_entry(&NewTogglEntry {
                toggl_workspace_id: "700001",
                toggl_entry_id: "900100",
                description: Some("SAB-123 old linked entry"),
                extracted_issue_key: Some("SAB-123"),
                source_hash: "sha256:old",
                rounded_duration_seconds: 1800,
                status: "planned",
                started_at: Some("2026-05-12T10:00:00Z"),
                stopped_at: Some("2026-05-12T10:30:00Z"),
            })
            .expect("seed linked entry");
        database
            .upsert_jira_worklog_link(&crate::db::NewJiraWorklogLink {
                toggl_workspace_id: "700001",
                toggl_entry_id: "900100",
                jira_site_key: "sabservis",
                jira_issue_key: "SAB-123",
                jira_worklog_id: Some("10001"),
                source_hash: "sha256:old",
                rounded_duration_seconds: 1800,
                status: "created",
            })
            .expect("seed active link");
        let server = MockServer::start();
        let config = test_config(&server.base_url());
        let toggl = test_toggl(&config);
        let range_refresh = server.mock(|when, then| {
            when.method(Method::GET)
                .path("/api/v9/me/time_entries")
                .query_param("since", "1778580000");
            then.status(200).json_body(json!([
                {
                    "id": 900100,
                    "workspace_id": 700001,
                    "description": "SAB-123 old linked entry",
                    "start": "2026-05-12T10:00:00Z",
                    "duration": 1740,
                    "updated_at": "2026-06-01T10:00:00Z"
                }
            ]));
        });

        let prepared = fetch_entries_for_planning(
            &database,
            &toggl,
            &config,
            1_780_272_000,
            &crate::toggl::TogglFetchResult {
                entries: Vec::new(),
                skipped: Vec::new(),
            },
        )
        .await
        .expect("linked refresh should prepare entries");

        assert_eq!(prepared.entries.len(), 1);
        assert_eq!(prepared.entries[0].entry_id, "900100");
        assert_eq!(prepared.entries[0].duration_seconds, 1740);
        assert_eq!(prepared.entries[0].deleted_at, None);
        range_refresh.assert_hits(1);
    }

    #[tokio::test]
    async fn linked_historical_refresh_marks_entries_missing_from_range_deleted() {
        let database = Database::open(":memory:").expect("open in-memory db");
        database.run_migrations().expect("run migrations");
        database
            .upsert_toggl_entry(&NewTogglEntry {
                toggl_workspace_id: "700001",
                toggl_entry_id: "900101",
                description: Some("SAB-123 deleted linked entry"),
                extracted_issue_key: Some("SAB-123"),
                source_hash: "sha256:old",
                rounded_duration_seconds: 1800,
                status: "planned",
                started_at: Some("2026-05-12T10:00:00Z"),
                stopped_at: Some("2026-05-12T10:30:00Z"),
            })
            .expect("seed linked entry");
        database
            .upsert_jira_worklog_link(&crate::db::NewJiraWorklogLink {
                toggl_workspace_id: "700001",
                toggl_entry_id: "900101",
                jira_site_key: "sabservis",
                jira_issue_key: "SAB-123",
                jira_worklog_id: Some("10002"),
                source_hash: "sha256:old",
                rounded_duration_seconds: 1800,
                status: "created",
            })
            .expect("seed active link");
        let server = MockServer::start();
        let config = test_config(&server.base_url());
        let toggl = test_toggl(&config);
        let range_refresh = server.mock(|when, then| {
            when.method(Method::GET)
                .path("/api/v9/me/time_entries")
                .query_param("since", "1778580000");
            then.status(200).json_body(json!([]));
        });

        let prepared = fetch_entries_for_planning(
            &database,
            &toggl,
            &config,
            1_780_272_000,
            &crate::toggl::TogglFetchResult {
                entries: Vec::new(),
                skipped: Vec::new(),
            },
        )
        .await
        .expect("linked delete refresh should prepare entries");

        assert_eq!(prepared.entries.len(), 1);
        assert_eq!(prepared.entries[0].entry_id, "900101");
        assert_eq!(
            prepared.entries[0].deleted_at.as_deref(),
            Some("2026-05-12T10:30:00Z")
        );
        range_refresh.assert_hits(1);
    }
}
