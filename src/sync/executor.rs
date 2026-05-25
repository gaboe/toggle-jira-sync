use std::collections::HashSet;
use std::fmt;

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
    UnmanagedWorklog,
    Conflict,
    Error(String),
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
                                None,
                                "error",
                                Some(error_message.as_str()),
                            )?;
                        }
                        return Err(error);
                    }
                }
            }
            PlannedMutation::Update(update) => match execute_update(update, jira, database).await {
                Ok(status) => push_success(&mut report, status),
                Err(error) if is_unmanaged_worklog_error(&error) => {
                    record_update_attempt(database, update, "error", Some("UnmanagedWorklog"))?;
                    report.failed += 1;
                    report.statuses.push(MutationStatus::UnmanagedWorklog);
                }
                Err(error) => {
                    report.failed += 1;
                    report
                        .statuses
                        .push(MutationStatus::Error(error.to_string()));
                    let error_message = error.to_string();
                    record_update_attempt(database, update, "error", Some(error_message.as_str()))?;
                    return Err(error);
                }
            },
            PlannedMutation::Delete(delete) => match execute_delete(delete, jira, database).await {
                Ok(status) => push_success(&mut report, status),
                Err(error) if is_unmanaged_worklog_error(&error) => {
                    record_delete_attempt(database, delete, "error", Some("UnmanagedWorklog"))?;
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
                    record_delete_attempt(database, delete, "error", Some(error_message.as_str()))?;
                    return Err(error);
                }
            },
        }
    }

    Ok(report)
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

    if let Some(found) = find_existing_marked_worklog(create, jira).await? {
        persist_link(database, create, &found.id, "created")?;
        record_create_attempt(database, create, Some(&found.id), "created", None)?;
        return Ok(MutationStatus::RecoveredExisting);
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
    record_create_attempt(database, create, Some(&worklog.id), "created", None)?;
    Ok(MutationStatus::Created)
}

async fn execute_update(
    update: &PlannedUpdate,
    jira: &JiraClient<impl Sleeper>,
    database: &Database,
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
    record_update_attempt(database, update, "updated", None)?;
    Ok(MutationStatus::Updated)
}

async fn execute_delete(
    delete: &PlannedDelete,
    jira: &JiraClient<impl Sleeper>,
    database: &Database,
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
    record_delete_attempt(database, delete, "deleted", None)?;
    Ok(MutationStatus::Deleted)
}

async fn find_existing_marked_worklog(
    create: &PlannedCreate,
    jira: &JiraClient<impl Sleeper>,
) -> Result<Option<Worklog>, ExecutorError> {
    let worklogs = jira.list_issue_worklogs(&create.jira_issue_key).await?;
    let expected_workspace_id = parse_i64("workspace_id", &create.toggl_workspace_id)?;
    let expected_entry_id = parse_i64("entry_id", &create.toggl_entry_id)?;

    for worklog in worklogs {
        match jira
            .read_marker_property(&create.jira_issue_key, &worklog.id)
            .await
        {
            Ok(marker)
                if marker.toggl_workspace_id == expected_workspace_id
                    && marker.toggl_entry_id == expected_entry_id =>
            {
                return Ok(Some(worklog));
            }
            Ok(_) => {}
            Err(error) if is_missing_marker_lookup(&error) => {
                if comment_has_fallback_marker(
                    &worklog,
                    &create.toggl_workspace_id,
                    &create.toggl_entry_id,
                ) {
                    return Ok(Some(worklog));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }

    Ok(None)
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
    worklog_id: Option<&str>,
    status: &str,
    error_message: Option<&str>,
) -> Result<(), ExecutorError> {
    database.insert_sync_attempt(&NewSyncAttempt {
        sync_run_id: None,
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
    status: &str,
    error_message: Option<&str>,
) -> Result<(), ExecutorError> {
    database.insert_sync_attempt(&NewSyncAttempt {
        sync_run_id: None,
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
    status: &str,
    error_message: Option<&str>,
) -> Result<(), ExecutorError> {
    database.insert_sync_attempt(&NewSyncAttempt {
        sync_run_id: None,
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
