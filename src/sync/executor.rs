use std::collections::{HashMap, HashSet};
use std::fmt;

use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::{Database, DbError, NewJiraWorklogLink, NewSyncAttempt};
use crate::jira::{JiraClient, JiraError, MarkerVerification, Sleeper, TogglSyncMarker, Worklog};
use crate::sync::planner::{
    PlannedCreate, PlannedDelete, PlannedMutation, PlannedUpdate, SyncPlan,
};
use crate::time::current_rfc3339_utc;

const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExecutorOptions {
    pub crash_after_remote_create: bool,
    pub sync_run_id: Option<i64>,
    pub max_parallel_groups: usize,
    pub failure_policy: ExecutorFailurePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutorFailurePolicy {
    #[default]
    FailFast,
    ContinueIndependent,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ExecutorReport {
    pub succeeded: usize,
    pub failed: usize,
    pub statuses: Vec<MutationStatus>,
    pub conflicts: Vec<ExecutorConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationStatus {
    Created,
    Updated,
    Deleted,
    RecoveredExisting,
    AdoptedExternal,
    UnmanagedWorklog,
    Conflict,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExistingWorklogMatch {
    Marked(Worklog),
    External(Worklog),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutorConflict {
    MoveDeleteFailed {
        toggl_workspace_id: String,
        toggl_entry_id: String,
    },
}

#[derive(Debug)]
pub enum ExecutorError {
    Jira(JiraError),
    Db(DbError),
    InvalidTogglId { field: &'static str, value: String },
    SimulatedCrashAfterRemoteCreate,
}

impl fmt::Display for ExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Jira(error) => write!(formatter, "{error}"),
            Self::Db(error) => write!(formatter, "{error}"),
            Self::InvalidTogglId { field, value } => {
                write!(formatter, "invalid numeric Toggl {field}: {value}")
            }
            Self::SimulatedCrashAfterRemoteCreate => {
                formatter.write_str("simulated crash after remote create before DB commit")
            }
        }
    }
}

impl std::error::Error for ExecutorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Jira(error) => Some(error),
            Self::Db(error) => Some(error),
            Self::InvalidTogglId { .. } | Self::SimulatedCrashAfterRemoteCreate => None,
        }
    }
}

impl From<JiraError> for ExecutorError {
    fn from(error: JiraError) -> Self {
        Self::Jira(error)
    }
}

impl From<DbError> for ExecutorError {
    fn from(error: DbError) -> Self {
        Self::Db(error)
    }
}

pub async fn execute_plan<S: Sleeper>(
    plan: &SyncPlan,
    jira: &JiraClient<S>,
    database: &Database,
    options: ExecutorOptions,
) -> Result<ExecutorReport, ExecutorError> {
    if options.failure_policy == ExecutorFailurePolicy::ContinueIndependent {
        return execute_plan_continue_independent(plan, jira, database, options).await;
    }

    let mut report = ExecutorReport::default();
    let mut blocked_creates = HashSet::<(String, String)>::new();

    for mutation in &plan.mutations {
        match mutation {
            PlannedMutation::Create(create) => {
                let key = toggl_key(
                    create.toggl_workspace_id.as_str(),
                    create.toggl_entry_id.as_str(),
                );
                if blocked_creates.contains(&key) {
                    push_conflict(&mut report, key);
                    continue;
                }

                match execute_create(create, jira, database, options).await {
                    Ok(status) => push_success(&mut report, status),
                    Err(error) => {
                        report.failed += 1;
                        report
                            .statuses
                            .push(MutationStatus::Error(error.to_string()));
                        if !matches!(error, ExecutorError::SimulatedCrashAfterRemoteCreate) {
                            let error_message = error.to_string();
                            record_create_attempt(
                                database,
                                create,
                                options.sync_run_id,
                                None,
                                "error",
                                Some(error_message.as_str()),
                            )?;
                        }
                        return Err(error);
                    }
                }
            }
            PlannedMutation::Update(update) => {
                match execute_update(update, jira, database, options.sync_run_id).await {
                    Ok(status) => push_success(&mut report, status),
                    Err(error) if is_unmanaged_worklog_error(&error) => {
                        record_update_attempt(
                            database,
                            update,
                            options.sync_run_id,
                            "error",
                            Some("UnmanagedWorklog"),
                        )?;
                        report.failed += 1;
                        report.statuses.push(MutationStatus::UnmanagedWorklog);
                    }
                    Err(error) => {
                        report.failed += 1;
                        report
                            .statuses
                            .push(MutationStatus::Error(error.to_string()));
                        let error_message = error.to_string();
                        record_update_attempt(
                            database,
                            update,
                            options.sync_run_id,
                            "error",
                            Some(error_message.as_str()),
                        )?;
                        return Err(error);
                    }
                }
            }
            PlannedMutation::Delete(delete) => {
                match execute_delete(delete, jira, database, options.sync_run_id).await {
                    Ok(status) => push_success(&mut report, status),
                    Err(error) if is_unmanaged_worklog_error(&error) => {
                        record_delete_attempt(
                            database,
                            delete,
                            options.sync_run_id,
                            "error",
                            Some("UnmanagedWorklog"),
                        )?;
                        let key = toggl_key(
                            delete.toggl_workspace_id.as_str(),
                            delete.toggl_entry_id.as_str(),
                        );
                        blocked_creates.insert(key.clone());
                        push_conflict(&mut report, key);
                    }
                    Err(error) => {
                        report.failed += 1;
                        report
                            .statuses
                            .push(MutationStatus::Error(error.to_string()));
                        let error_message = error.to_string();
                        record_delete_attempt(
                            database,
                            delete,
                            options.sync_run_id,
                            "error",
                            Some(error_message.as_str()),
                        )?;
                        return Err(error);
                    }
                }
            }
        }
    }

    Ok(report)
}

async fn execute_plan_continue_independent<S: Sleeper>(
    plan: &SyncPlan,
    jira: &JiraClient<S>,
    database: &Database,
    options: ExecutorOptions,
) -> Result<ExecutorReport, ExecutorError> {
    let groups = mutation_groups(plan);
    let max_parallel_groups = options.max_parallel_groups.max(1);
    let mut next_group = 0;
    let mut running = FuturesUnordered::new();
    let mut reports = Vec::with_capacity(groups.len());

    while next_group < groups.len() && running.len() < max_parallel_groups {
        running.push(execute_mutation_group(
            groups[next_group].clone(),
            jira,
            database,
            options,
        ));
        next_group += 1;
    }

    while let Some(result) = running.next().await {
        reports.push(result?);
        while next_group < groups.len() && running.len() < max_parallel_groups {
            running.push(execute_mutation_group(
                groups[next_group].clone(),
                jira,
                database,
                options,
            ));
            next_group += 1;
        }
    }

    Ok(combine_group_reports(reports))
}

async fn execute_mutation_group<S: Sleeper>(
    group: Vec<IndexedMutation>,
    jira: &JiraClient<S>,
    database: &Database,
    options: ExecutorOptions,
) -> Result<IndexedGroupReport, ExecutorError> {
    let mut statuses = Vec::with_capacity(group.len());
    let mut conflicts = Vec::new();
    let mut blocked_creates = HashSet::<(String, String)>::new();

    for indexed in group {
        let mut report = ExecutorReport::default();
        execute_mutation_continue_independent(
            &indexed.mutation,
            jira,
            database,
            options,
            &mut report,
            &mut blocked_creates,
        )
        .await?;
        statuses.extend(
            report
                .statuses
                .into_iter()
                .map(|status| IndexedMutationStatus {
                    index: indexed.index,
                    status,
                }),
        );
        conflicts.extend(
            report
                .conflicts
                .into_iter()
                .map(|conflict| IndexedExecutorConflict {
                    index: indexed.index,
                    conflict,
                }),
        );
    }

    Ok(IndexedGroupReport {
        statuses,
        conflicts,
    })
}

async fn execute_mutation_continue_independent<S: Sleeper>(
    mutation: &PlannedMutation,
    jira: &JiraClient<S>,
    database: &Database,
    options: ExecutorOptions,
    report: &mut ExecutorReport,
    blocked_creates: &mut HashSet<(String, String)>,
) -> Result<(), ExecutorError> {
    match mutation {
        PlannedMutation::Create(create) => {
            let key = toggl_key(
                create.toggl_workspace_id.as_str(),
                create.toggl_entry_id.as_str(),
            );
            if blocked_creates.contains(&key) {
                push_conflict(report, key);
                return Ok(());
            }

            match execute_create(create, jira, database, options).await {
                Ok(status) => push_success(report, status),
                Err(error) if can_continue_after(&error) => {
                    report.failed += 1;
                    report
                        .statuses
                        .push(MutationStatus::Error(error.to_string()));
                    let error_message = error.to_string();
                    record_create_attempt(
                        database,
                        create,
                        options.sync_run_id,
                        None,
                        "error",
                        Some(error_message.as_str()),
                    )?;
                }
                Err(error) => return Err(error),
            }
        }
        PlannedMutation::Update(update) => {
            match execute_update(update, jira, database, options.sync_run_id).await {
                Ok(status) => push_success(report, status),
                Err(error) if is_unmanaged_worklog_error(&error) => {
                    record_update_attempt(
                        database,
                        update,
                        options.sync_run_id,
                        "error",
                        Some("UnmanagedWorklog"),
                    )?;
                    report.failed += 1;
                    report.statuses.push(MutationStatus::UnmanagedWorklog);
                }
                Err(error) if can_continue_after(&error) => {
                    report.failed += 1;
                    report
                        .statuses
                        .push(MutationStatus::Error(error.to_string()));
                    let error_message = error.to_string();
                    record_update_attempt(
                        database,
                        update,
                        options.sync_run_id,
                        "error",
                        Some(error_message.as_str()),
                    )?;
                }
                Err(error) => return Err(error),
            }
        }
        PlannedMutation::Delete(delete) => {
            match execute_delete(delete, jira, database, options.sync_run_id).await {
                Ok(status) => push_success(report, status),
                Err(error) if is_unmanaged_worklog_error(&error) => {
                    record_delete_attempt(
                        database,
                        delete,
                        options.sync_run_id,
                        "error",
                        Some("UnmanagedWorklog"),
                    )?;
                    let key = toggl_key(
                        delete.toggl_workspace_id.as_str(),
                        delete.toggl_entry_id.as_str(),
                    );
                    blocked_creates.insert(key.clone());
                    push_conflict(report, key);
                }
                Err(error) if can_continue_after(&error) => {
                    report.failed += 1;
                    report
                        .statuses
                        .push(MutationStatus::Error(error.to_string()));
                    let error_message = error.to_string();
                    record_delete_attempt(
                        database,
                        delete,
                        options.sync_run_id,
                        "error",
                        Some(error_message.as_str()),
                    )?;
                    let key = toggl_key(
                        delete.toggl_workspace_id.as_str(),
                        delete.toggl_entry_id.as_str(),
                    );
                    blocked_creates.insert(key);
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn can_continue_after(error: &ExecutorError) -> bool {
    matches!(error, ExecutorError::Jira(_))
}

#[derive(Debug, Clone)]
struct IndexedMutation {
    index: usize,
    mutation: PlannedMutation,
}

#[derive(Debug)]
struct IndexedMutationStatus {
    index: usize,
    status: MutationStatus,
}

#[derive(Debug)]
struct IndexedGroupReport {
    statuses: Vec<IndexedMutationStatus>,
    conflicts: Vec<IndexedExecutorConflict>,
}

#[derive(Debug)]
struct IndexedExecutorConflict {
    index: usize,
    conflict: ExecutorConflict,
}

fn mutation_groups(plan: &SyncPlan) -> Vec<Vec<IndexedMutation>> {
    let mut parents: Vec<usize> = (0..plan.mutations.len()).collect();
    let mut issue_indexes = HashMap::<(String, String), usize>::new();
    let mut toggl_indexes = HashMap::<(String, String), usize>::new();

    for (index, mutation) in plan.mutations.iter().enumerate() {
        if let Some(previous) = issue_indexes.insert(mutation_issue_key(mutation), index) {
            union_indexes(&mut parents, previous, index);
        }
        if let Some(previous) = toggl_indexes.insert(mutation_toggl_key(mutation), index) {
            union_indexes(&mut parents, previous, index);
        }
    }

    let mut grouped = HashMap::<usize, Vec<IndexedMutation>>::new();
    for (index, mutation) in plan.mutations.iter().cloned().enumerate() {
        let root = find_index(&mut parents, index);
        grouped
            .entry(root)
            .or_default()
            .push(IndexedMutation { index, mutation });
    }

    let mut groups = grouped.into_values().collect::<Vec<_>>();
    groups.sort_by_key(|group| {
        group
            .first()
            .map(|mutation| mutation.index)
            .unwrap_or(usize::MAX)
    });
    groups
}

fn combine_group_reports(group_reports: Vec<IndexedGroupReport>) -> ExecutorReport {
    let mut indexed_statuses = Vec::new();
    let mut conflicts = Vec::new();
    for group_report in group_reports {
        indexed_statuses.extend(group_report.statuses);
        conflicts.extend(group_report.conflicts);
    }
    indexed_statuses.sort_by_key(|status| status.index);
    conflicts.sort_by_key(|conflict| conflict.index);

    let mut report = ExecutorReport {
        conflicts: conflicts
            .into_iter()
            .map(|indexed_conflict| indexed_conflict.conflict)
            .collect(),
        ..Default::default()
    };
    for indexed in indexed_statuses {
        match indexed.status {
            MutationStatus::Created
            | MutationStatus::Updated
            | MutationStatus::Deleted
            | MutationStatus::RecoveredExisting
            | MutationStatus::AdoptedExternal => report.succeeded += 1,
            MutationStatus::UnmanagedWorklog
            | MutationStatus::Conflict
            | MutationStatus::Error(_) => report.failed += 1,
        }
        report.statuses.push(indexed.status);
    }
    report
}

fn mutation_issue_key(mutation: &PlannedMutation) -> (String, String) {
    match mutation {
        PlannedMutation::Create(create) => {
            (create.jira_site_key.clone(), create.jira_issue_key.clone())
        }
        PlannedMutation::Update(update) => {
            (update.jira_site_key.clone(), update.jira_issue_key.clone())
        }
        PlannedMutation::Delete(delete) => {
            (delete.jira_site_key.clone(), delete.jira_issue_key.clone())
        }
    }
}

fn mutation_toggl_key(mutation: &PlannedMutation) -> (String, String) {
    match mutation {
        PlannedMutation::Create(create) => {
            toggl_key(&create.toggl_workspace_id, &create.toggl_entry_id)
        }
        PlannedMutation::Update(update) => {
            toggl_key(&update.toggl_workspace_id, &update.toggl_entry_id)
        }
        PlannedMutation::Delete(delete) => {
            toggl_key(&delete.toggl_workspace_id, &delete.toggl_entry_id)
        }
    }
}

fn union_indexes(parents: &mut [usize], left: usize, right: usize) {
    let left_root = find_index(parents, left);
    let right_root = find_index(parents, right);
    if left_root != right_root {
        parents[right_root] = left_root;
    }
}

fn find_index(parents: &mut [usize], index: usize) -> usize {
    if parents[index] != index {
        parents[index] = find_index(parents, parents[index]);
    }
    parents[index]
}

async fn execute_create(
    create: &PlannedCreate,
    jira: &JiraClient<impl Sleeper>,
    database: &Database,
    options: ExecutorOptions,
) -> Result<MutationStatus, ExecutorError> {
    if let Some(existing) = database.get_jira_worklog_link(
        &create.toggl_workspace_id,
        &create.toggl_entry_id,
        &create.jira_site_key,
    )? {
        if existing.jira_worklog_id.is_some() {
            return Ok(MutationStatus::RecoveredExisting);
        }
    }

    match find_existing_worklog(create, jira).await? {
        Some(ExistingWorklogMatch::Marked(found)) => {
            persist_link(database, create, &found.id, "created")?;
            record_create_attempt(
                database,
                create,
                options.sync_run_id,
                Some(&found.id),
                "created",
                None,
            )?;
            return Ok(MutationStatus::RecoveredExisting);
        }
        Some(ExistingWorklogMatch::External(found)) => {
            persist_link(database, create, &found.id, "adopted")?;
            record_create_attempt(
                database,
                create,
                options.sync_run_id,
                Some(&found.id),
                "adopted",
                Some("Already written by another sync or user; linked locally without changing Jira."),
            )?;
            return Ok(MutationStatus::AdoptedExternal);
        }
        None => {}
    }

    let marker = marker_for(
        &create.toggl_workspace_id,
        &create.toggl_entry_id,
        &create.source_hash,
    )?;
    let worklog = jira
        .create_worklog(&create.jira_issue_key, &create.draft, &marker)
        .await?;

    if options.crash_after_remote_create {
        return Err(ExecutorError::SimulatedCrashAfterRemoteCreate);
    }

    persist_link(database, create, &worklog.id, "created")?;
    record_create_attempt(
        database,
        create,
        options.sync_run_id,
        Some(&worklog.id),
        "created",
        None,
    )?;
    Ok(MutationStatus::Created)
}

async fn execute_update(
    update: &PlannedUpdate,
    jira: &JiraClient<impl Sleeper>,
    database: &Database,
    sync_run_id: Option<i64>,
) -> Result<MutationStatus, ExecutorError> {
    let marker = marker_for(
        &update.toggl_workspace_id,
        &update.toggl_entry_id,
        &update.source_hash,
    )?;
    jira.update_marked_worklog(
        &update.jira_issue_key,
        &update.jira_worklog_id,
        &update.draft,
        &marker,
    )
    .await?;
    jira.set_marker_property_or_comment_fallback(
        &update.jira_issue_key,
        &update.jira_worklog_id,
        &update.draft,
        &marker,
    )
    .await?;
    database.upsert_jira_worklog_link(&NewJiraWorklogLink {
        toggl_workspace_id: &update.toggl_workspace_id,
        toggl_entry_id: &update.toggl_entry_id,
        jira_site_key: &update.jira_site_key,
        jira_issue_key: &update.jira_issue_key,
        jira_worklog_id: Some(&update.jira_worklog_id),
        source_hash: &update.source_hash,
        rounded_duration_seconds: update.draft.time_spent_seconds,
        status: "updated",
    })?;
    record_update_attempt(database, update, sync_run_id, "updated", None)?;
    Ok(MutationStatus::Updated)
}

async fn execute_delete(
    delete: &PlannedDelete,
    jira: &JiraClient<impl Sleeper>,
    database: &Database,
    sync_run_id: Option<i64>,
) -> Result<MutationStatus, ExecutorError> {
    let marker = marker_for(
        &delete.toggl_workspace_id,
        &delete.toggl_entry_id,
        &delete.source_hash,
    )?;
    jira.delete_marked_worklog(&delete.jira_issue_key, &delete.jira_worklog_id, &marker)
        .await?;
    database.mark_jira_worklog_link_deleted(
        &delete.toggl_workspace_id,
        &delete.toggl_entry_id,
        &delete.jira_site_key,
    )?;
    record_delete_attempt(database, delete, sync_run_id, "deleted", None)?;
    Ok(MutationStatus::Deleted)
}

async fn find_existing_worklog(
    create: &PlannedCreate,
    jira: &JiraClient<impl Sleeper>,
) -> Result<Option<ExistingWorklogMatch>, ExecutorError> {
    let worklogs = jira.list_issue_worklogs(&create.jira_issue_key).await?;
    let expected_workspace_id = parse_i64("workspace_id", &create.toggl_workspace_id)?;
    let expected_entry_id = parse_i64("entry_id", &create.toggl_entry_id)?;
    let mut matching_external = None;

    for worklog in worklogs {
        let matches_draft = worklog_matches_draft(&worklog, create);
        match jira
            .read_marker_property(&create.jira_issue_key, &worklog.id)
            .await
        {
            Ok(marker)
                if marker.toggl_workspace_id == expected_workspace_id
                    && marker.toggl_entry_id == expected_entry_id =>
            {
                return Ok(Some(ExistingWorklogMatch::Marked(worklog)));
            }
            Ok(_) => {}
            Err(error) if is_missing_marker_lookup(&error) => {
                if comment_has_fallback_marker(
                    &worklog,
                    &create.toggl_workspace_id,
                    &create.toggl_entry_id,
                ) {
                    return Ok(Some(ExistingWorklogMatch::Marked(worklog)));
                }
                if matches_draft && matching_external.is_none() {
                    matching_external = Some(worklog);
                }
            }
            Err(error) => return Err(error.into()),
        }
    }

    Ok(matching_external.map(ExistingWorklogMatch::External))
}

fn worklog_matches_draft(worklog: &Worklog, create: &PlannedCreate) -> bool {
    if worklog.time_spent_seconds != Some(create.draft.time_spent_seconds) {
        return false;
    }

    let Some(comment) = worklog.comment.as_ref().map(collect_text) else {
        return false;
    };
    let Some(started) = worklog.started.as_deref() else {
        return false;
    };

    (started == create.draft.started.as_str() && comment.trim() == create.draft.comment.trim())
        || (same_wall_clock_started(started, &create.draft.started)
            && equivalent_worklog_comment(&comment, &create.draft.comment, &create.jira_issue_key))
}

fn same_wall_clock_started(left: &str, right: &str) -> bool {
    wall_clock_started_prefix(left) == wall_clock_started_prefix(right)
}

fn wall_clock_started_prefix(started: &str) -> Option<String> {
    let datetime = started
        .split_once('+')
        .map(|(datetime, _)| datetime)
        .or_else(|| started.rsplit_once('-').map(|(datetime, _)| datetime))
        .unwrap_or(started);
    let datetime = datetime.strip_suffix('Z').unwrap_or(datetime);
    let (date, time) = datetime.split_once('T')?;
    let time = time.split_once('.').map(|(time, _)| time).unwrap_or(time);
    let mut parts = time.split(':');
    let hour = parts.next()?;
    let minute = parts.next()?;
    let second = parts.next().unwrap_or("00");
    Some(format!("{date}T{hour}:{minute}:{second}"))
}

fn equivalent_worklog_comment(left: &str, right: &str, issue_key: &str) -> bool {
    normalized_worklog_comment(left, issue_key) == normalized_worklog_comment(right, issue_key)
}

fn normalized_worklog_comment(comment: &str, issue_key: &str) -> String {
    comment
        .replace(issue_key, " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn persist_link(
    database: &Database,
    create: &PlannedCreate,
    worklog_id: &str,
    status: &'static str,
) -> Result<(), ExecutorError> {
    database.upsert_jira_worklog_link(&NewJiraWorklogLink {
        toggl_workspace_id: &create.toggl_workspace_id,
        toggl_entry_id: &create.toggl_entry_id,
        jira_site_key: &create.jira_site_key,
        jira_issue_key: &create.jira_issue_key,
        jira_worklog_id: Some(worklog_id),
        source_hash: &create.source_hash,
        rounded_duration_seconds: create.draft.time_spent_seconds,
        status,
    })?;
    Ok(())
}

fn marker_for(
    toggl_workspace_id: &str,
    toggl_entry_id: &str,
    source_hash: &str,
) -> Result<TogglSyncMarker, ExecutorError> {
    Ok(TogglSyncMarker {
        toggl_workspace_id: parse_i64("workspace_id", toggl_workspace_id)?,
        toggl_entry_id: parse_i64("entry_id", toggl_entry_id)?,
        source_hash: source_hash.to_owned(),
        synced_at: current_timestamp(),
        tool_version: TOOL_VERSION.to_owned(),
    })
}

fn parse_i64(field: &'static str, value: &str) -> Result<i64, ExecutorError> {
    value
        .parse::<i64>()
        .map_err(|_| ExecutorError::InvalidTogglId {
            field,
            value: value.to_owned(),
        })
}

fn current_timestamp() -> String {
    current_rfc3339_utc()
}

fn comment_has_fallback_marker(worklog: &Worklog, workspace_id: &str, entry_id: &str) -> bool {
    let Some(comment) = &worklog.comment else {
        return false;
    };
    let text = collect_text(comment);
    text.trim_end().ends_with(&format!(
        "[toggl-sync:workspace={workspace_id};entry={entry_id}]"
    ))
}

fn collect_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(collect_text).collect::<Vec<_>>().join(""),
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                return text.to_owned();
            }
            if let Some(content) = map.get("content") {
                return collect_text(content);
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn is_missing_marker_lookup(error: &JiraError) -> bool {
    matches!(
        error,
        JiraError::MarkerVerificationFailed(MarkerVerification::Missing)
    )
}

fn is_unmanaged_worklog_error(error: &ExecutorError) -> bool {
    matches!(
        error,
        ExecutorError::Jira(JiraError::IssueNotFound | JiraError::MarkerVerificationFailed(_))
    )
}

fn push_success(report: &mut ExecutorReport, status: MutationStatus) {
    report.succeeded += 1;
    report.statuses.push(status);
}

fn push_conflict(report: &mut ExecutorReport, key: (String, String)) {
    report.failed += 1;
    report.statuses.push(MutationStatus::Conflict);
    report.conflicts.push(ExecutorConflict::MoveDeleteFailed {
        toggl_workspace_id: key.0,
        toggl_entry_id: key.1,
    });
}

fn record_create_attempt(
    database: &Database,
    create: &PlannedCreate,
    sync_run_id: Option<i64>,
    worklog_id: Option<&str>,
    status: &str,
    error_message: Option<&str>,
) -> Result<(), ExecutorError> {
    database.insert_sync_attempt(&NewSyncAttempt {
        sync_run_id,
        toggl_workspace_id: &create.toggl_workspace_id,
        toggl_entry_id: &create.toggl_entry_id,
        jira_site_key: Some(&create.jira_site_key),
        jira_issue_key: Some(&create.jira_issue_key),
        jira_worklog_id: worklog_id,
        status,
        error_message,
    })?;
    Ok(())
}

fn record_update_attempt(
    database: &Database,
    update: &PlannedUpdate,
    sync_run_id: Option<i64>,
    status: &str,
    error_message: Option<&str>,
) -> Result<(), ExecutorError> {
    database.insert_sync_attempt(&NewSyncAttempt {
        sync_run_id,
        toggl_workspace_id: &update.toggl_workspace_id,
        toggl_entry_id: &update.toggl_entry_id,
        jira_site_key: Some(&update.jira_site_key),
        jira_issue_key: Some(&update.jira_issue_key),
        jira_worklog_id: Some(&update.jira_worklog_id),
        status,
        error_message,
    })?;
    Ok(())
}

fn record_delete_attempt(
    database: &Database,
    delete: &PlannedDelete,
    sync_run_id: Option<i64>,
    status: &str,
    error_message: Option<&str>,
) -> Result<(), ExecutorError> {
    database.insert_sync_attempt(&NewSyncAttempt {
        sync_run_id,
        toggl_workspace_id: &delete.toggl_workspace_id,
        toggl_entry_id: &delete.toggl_entry_id,
        jira_site_key: Some(&delete.jira_site_key),
        jira_issue_key: Some(&delete.jira_issue_key),
        jira_worklog_id: Some(&delete.jira_worklog_id),
        status,
        error_message,
    })?;
    Ok(())
}

fn toggl_key(workspace_id: &str, entry_id: &str) -> (String, String) {
    (workspace_id.to_owned(), entry_id.to_owned())
}
