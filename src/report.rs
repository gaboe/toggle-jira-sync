use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::{
    db::StoredStatusEntry,
    sync::planner::{
        plan_sync, ExistingWorklogLink, IssueSiteMapping, PlannerInput, PlannerOutcome, SkipCause,
    },
    time::{format_duration, split_status_datetime},
    toggl::{TogglFetchResult, TogglFetchSkip},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunReport {
    pub mode: String,
    pub summary: DryRunSummary,
    pub entries: Vec<DryRunEntry>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunSummary {
    pub create_count: usize,
    pub update_count: usize,
    pub delete_count: usize,
    pub move_count: usize,
    pub no_op_count: usize,
    pub skipped_count: usize,
    pub errors_count: usize,
    pub rate_limit_retries_avoided_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunEntry {
    pub toggl_workspace_id: String,
    pub toggl_entry_id: String,
    pub action: PlannedAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub mode: String,
    pub summary: StatusSummary,
    pub entries: Vec<StatusEntry>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSummary {
    pub total_count: usize,
    pub synced_count: usize,
    pub not_synced_count: usize,
    pub error_count: usize,
    pub skipped_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusEntry {
    pub workspace: String,
    pub entry: String,
    pub started_at: Option<String>,
    pub stopped_at: Option<String>,
    pub duration_seconds: i64,
    pub issue_key: Option<String>,
    pub site: Option<String>,
    pub worklog_id: Option<String>,
    pub status: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlannedAction {
    Create,
    Update,
    Delete,
    Move,
    NoOp,
    Skipped,
    Error,
}

impl DryRunReport {
    pub fn from_fetch_result_with_resolved_sites(
        fetch: TogglFetchResult,
        issue_site_mappings: Vec<IssueSiteMapping>,
        existing_links: Vec<ExistingWorklogLink>,
    ) -> Self {
        let plan = plan_sync(PlannerInput {
            entries: fetch.entries,
            issue_site_mappings,
            existing_links,
        })
        .expect("planner does not fail globally");

        let mut report = Self {
            mode: "dry_run".to_owned(),
            summary: DryRunSummary::default(),
            entries: Vec::new(),
        };

        let mut planned_entry_ids = HashSet::new();
        for entry in plan.entries {
            planned_entry_ids.insert((
                entry.toggl_workspace_id.clone(),
                entry.toggl_entry_id.clone(),
            ));
            let (action, reason) = action_from_outcome(entry.outcome);
            report.increment(action);
            report.entries.push(DryRunEntry {
                toggl_workspace_id: entry.toggl_workspace_id,
                toggl_entry_id: entry.toggl_entry_id,
                action,
                reason,
            });
        }

        for skipped in fetch.skipped {
            if skipped.entry_id.as_ref().is_some_and(|entry_id| {
                planned_entry_ids.contains(&(skipped.workspace_id.clone(), entry_id.clone()))
            }) {
                continue;
            }
            report.push_fetch_skip(skipped);
        }

        report
    }

    pub fn to_json_string(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    pub fn to_human_string(&self) -> String {
        format!(
            "dry-run: create={} update={} delete={} move={} no-op={} skipped={} errors={}",
            self.summary.create_count,
            self.summary.update_count,
            self.summary.delete_count,
            self.summary.move_count,
            self.summary.no_op_count,
            self.summary.skipped_count,
            self.summary.errors_count
        )
    }

    fn push_fetch_skip(&mut self, skipped: TogglFetchSkip) {
        self.summary.skipped_count += 1;
        self.entries.push(DryRunEntry {
            toggl_workspace_id: skipped.workspace_id,
            toggl_entry_id: skipped.entry_id.unwrap_or_else(|| "<fetch>".to_owned()),
            action: PlannedAction::Skipped,
            reason: Some(format!("{:?}", skipped.reason)),
        });
    }

    fn increment(&mut self, action: PlannedAction) {
        match action {
            PlannedAction::Create => self.summary.create_count += 1,
            PlannedAction::Update => self.summary.update_count += 1,
            PlannedAction::Delete => self.summary.delete_count += 1,
            PlannedAction::Move => self.summary.move_count += 1,
            PlannedAction::NoOp => self.summary.no_op_count += 1,
            PlannedAction::Skipped => self.summary.skipped_count += 1,
            PlannedAction::Error => self.summary.errors_count += 1,
        }
    }
}

impl StatusReport {
    pub fn from_rows(rows: Vec<StoredStatusEntry>) -> Self {
        let mut report = Self {
            mode: "status".to_owned(),
            summary: StatusSummary::default(),
            entries: Vec::with_capacity(rows.len()),
        };

        for row in rows {
            report.summary.total_count += 1;
            match row.status.as_str() {
                "synced" => report.summary.synced_count += 1,
                "not_synced" => report.summary.not_synced_count += 1,
                "error" => report.summary.error_count += 1,
                "skipped" => report.summary.skipped_count += 1,
                _ => {}
            }
            report.entries.push(StatusEntry {
                workspace: row.workspace,
                entry: row.entry,
                started_at: row.started_at,
                stopped_at: row.stopped_at,
                duration_seconds: row.rounded_duration_seconds,
                issue_key: row.issue_key,
                site: row.site,
                worklog_id: row.worklog_id,
                status: row.status,
                reason: row.reason,
            });
        }

        report
    }

    pub fn to_json_string(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    pub fn to_human_string(&self) -> String {
        let mut lines = vec![format!(
            "status: total={} synced={} not_synced={} error={} skipped={}",
            self.summary.total_count,
            self.summary.synced_count,
            self.summary.not_synced_count,
            self.summary.error_count,
            self.summary.skipped_count
        )];
        lines.push(format_status_row(&[
            "date", "start", "end", "duration", "issue", "site", "worklog", "status", "reason",
        ]));

        for entry in &self.entries {
            let (date, start) = split_status_datetime(&entry.started_at);
            let (_, end) = split_status_datetime(&entry.stopped_at);
            lines.push(format_status_row(&[
                &date,
                &start,
                &end,
                &format_duration(entry.duration_seconds),
                option_or_dash(&entry.issue_key),
                option_or_dash(&entry.site),
                option_or_dash(&entry.worklog_id),
                &entry.status,
                option_or_dash(&entry.reason),
            ]));
        }

        lines.join("\n")
    }
}

fn format_status_row(columns: &[&str; 9]) -> String {
    format!(
        "{:<10}  {:<5}  {:<5}  {:>8}  {:<10}  {:<10}  {:<7}  {:<8}  {}",
        columns[0],
        columns[1],
        columns[2],
        columns[3],
        columns[4],
        columns[5],
        columns[6],
        columns[7],
        columns[8]
    )
}

fn option_or_dash(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("-")
}

fn action_from_outcome(outcome: PlannerOutcome) -> (PlannedAction, Option<String>) {
    match outcome {
        PlannerOutcome::Create => (PlannedAction::Create, None),
        PlannerOutcome::Update => (PlannedAction::Update, None),
        PlannerOutcome::Delete => (PlannedAction::Delete, None),
        PlannerOutcome::Move {
            from_issue_key,
            to_issue_key,
        } => (
            PlannedAction::Move,
            Some(format!(
                "issue key changed from {from_issue_key} to {to_issue_key}"
            )),
        ),
        PlannerOutcome::NoOp => (PlannedAction::NoOp, None),
        PlannerOutcome::Skip(SkipCause::MissingManagedWorklog) => (
            PlannedAction::Delete,
            Some("missing managed Jira worklog".to_owned()),
        ),
        PlannerOutcome::Skip(cause) => (PlannedAction::Skipped, Some(skip_reason(cause))),
        PlannerOutcome::Error(issue) => (PlannedAction::Error, Some(issue.to_string())),
    }
}

fn skip_reason(cause: SkipCause) -> String {
    match cause {
        SkipCause::MissingIssueKey => {
            "no valid Jira issue key found in Toggl description".to_owned()
        }
        SkipCause::RoundedDurationZero => "rounded duration is zero".to_owned(),
        SkipCause::RunningEntry => "running entry".to_owned(),
        SkipCause::MissingManagedWorklog => "missing managed Jira worklog".to_owned(),
        SkipCause::UnmarkedWorklog => "unmarked Jira worklog".to_owned(),
    }
}
