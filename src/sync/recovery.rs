use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::{Database, DbError, NewJiraWorklogLink, NewRecoveryFinding};
use crate::jira::{JiraClient, JiraError, MarkerVerification, Worklog};
use crate::sync::planner::{
    compute_source_hash, extract_issue_keys, round_to_nearest_minute, IssueSiteMapping,
    PlannerIssue,
};
use crate::toggl::TogglTimeEntry;

#[derive(Debug, Clone)]
pub struct RecoverySite {
    pub key: String,
    pub client: JiraClient,
}

#[derive(Debug)]
pub struct RecoveryInput<'a> {
    pub database: &'a Database,
    pub entries: Vec<TogglTimeEntry>,
    pub issue_site_mappings: Vec<IssueSiteMapping>,
    pub recovery_sites: Vec<RecoverySite>,
    pub recovery_scan_days: u32,
    pub requested_scan_days: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct RecoveryReport {
    pub mode: &'static str,
    pub scanned_entries: usize,
    pub scanned_issues: usize,
    pub scanned_worklogs: usize,
    pub recovered_links: usize,
    pub conflicts: Vec<RecoveryConflict>,
    pub warnings: Vec<RecoveryWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecoveryConflict {
    DuplicateMarkedWorklogs {
        toggl_workspace_id: String,
        toggl_entry_id: String,
        jira_site_key: String,
        jira_issue_key: String,
        jira_worklog_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecoveryWarning {
    RequestedWindowClamped {
        requested_days: u32,
        configured_days: u32,
    },
    IssueKeySkipped {
        issue_key: String,
        reason: String,
    },
    MissingRecoveryClient {
        jira_site_key: String,
        issue_key: String,
    },
}

#[derive(Debug)]
pub enum RecoveryError {
    Jira(JiraError),
    Db(DbError),
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Jira(error) => write!(formatter, "{error}"),
            Self::Db(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for RecoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Jira(error) => Some(error),
            Self::Db(error) => Some(error),
        }
    }
}

impl From<JiraError> for RecoveryError {
    fn from(error: JiraError) -> Self {
        Self::Jira(error)
    }
}

impl From<DbError> for RecoveryError {
    fn from(error: DbError) -> Self {
        Self::Db(error)
    }
}

#[derive(Debug, Clone)]
struct CandidateEntry {
    entry: TogglTimeEntry,
    issue_key: String,
    site_key: String,
    source_hash: String,
    rounded_duration_seconds: i64,
}

#[derive(Debug, Clone)]
struct FoundMarker {
    toggl_workspace_id: String,
    toggl_entry_id: String,
    jira_site_key: String,
    jira_issue_key: String,
    jira_worklog_id: String,
    source_hash: String,
    rounded_duration_seconds: i64,
    marker_source: &'static str,
}

pub async fn recover(input: RecoveryInput<'_>) -> Result<RecoveryReport, RecoveryError> {
    let mut report = RecoveryReport {
        mode: "recover",
        scanned_entries: input.entries.len(),
        ..RecoveryReport::default()
    };

    if let Some(requested_days) = input.requested_scan_days {
        if requested_days > input.recovery_scan_days {
            report
                .warnings
                .push(RecoveryWarning::RequestedWindowClamped {
                    requested_days,
                    configured_days: input.recovery_scan_days,
                });
        }
    }

    let candidates = collect_candidates(input.entries, input.issue_site_mappings, &mut report);
    let issue_scans = group_candidates_by_issue(&candidates);
    let sites = input
        .recovery_sites
        .into_iter()
        .map(|site| (site.key.clone(), site))
        .collect::<HashMap<_, _>>();

    let mut found = Vec::new();
    for ((site_key, issue_key), candidate_indexes) in issue_scans {
        let Some(site) = sites.get(&site_key) else {
            report
                .warnings
                .push(RecoveryWarning::MissingRecoveryClient {
                    jira_site_key: site_key,
                    issue_key,
                });
            continue;
        };

        report.scanned_issues += 1;
        let worklogs = site.client.list_issue_worklogs(&issue_key).await?;
        report.scanned_worklogs += worklogs.len();
        let candidate_entries = candidate_indexes
            .into_iter()
            .map(|index| &candidates[index])
            .collect::<Vec<_>>();

        for worklog in worklogs {
            if let Some(marker) =
                read_marker(&site.client, &issue_key, &worklog, &candidate_entries).await?
            {
                found.push(marker);
            }
        }
    }

    persist_findings(input.database, found, &mut report)?;
    Ok(report)
}

fn collect_candidates(
    entries: Vec<TogglTimeEntry>,
    issue_site_mappings: Vec<IssueSiteMapping>,
    report: &mut RecoveryReport,
) -> Vec<CandidateEntry> {
    let mut candidates = Vec::new();
    let issue_sites = index_issue_site_mappings(issue_site_mappings);

    for entry in entries {
        let description = entry.description.clone().unwrap_or_default();
        for issue_key in extract_issue_keys(&description) {
            let site_key = match resolve_site_key(&issue_key, &issue_sites) {
                Ok(site_key) => site_key,
                Err(issue) => {
                    report.warnings.push(RecoveryWarning::IssueKeySkipped {
                        issue_key: issue_key.clone(),
                        reason: issue.to_string(),
                    });
                    continue;
                }
            };
            let rounded_duration_seconds = round_to_nearest_minute(entry.duration_seconds);
            if rounded_duration_seconds == 0 || entry.deleted_at.is_some() || entry.is_running() {
                continue;
            }
            let source_hash = compute_source_hash(
                &issue_key,
                &site_key,
                rounded_duration_seconds,
                &entry.start,
                &description,
            );
            candidates.push(CandidateEntry {
                entry: entry.clone(),
                issue_key,
                site_key,
                source_hash,
                rounded_duration_seconds,
            });
        }
    }

    candidates
}

fn index_issue_site_mappings(mappings: Vec<IssueSiteMapping>) -> HashMap<String, Vec<String>> {
    let mut indexed = HashMap::<String, Vec<String>>::new();
    for mapping in mappings {
        let site_keys = indexed.entry(mapping.issue_key).or_default();
        if !site_keys.contains(&mapping.jira_site_key) {
            site_keys.push(mapping.jira_site_key);
        }
    }
    indexed
}

fn resolve_site_key(
    issue_key: &str,
    issue_sites: &HashMap<String, Vec<String>>,
) -> Result<String, PlannerIssue> {
    let Some(matches) = issue_sites.get(issue_key) else {
        return Err(PlannerIssue::UnresolvedIssueSite {
            issue_key: issue_key.to_owned(),
        });
    };

    match matches.as_slice() {
        [site_key] => Ok(site_key.clone()),
        [] => Err(PlannerIssue::UnresolvedIssueSite {
            issue_key: issue_key.to_owned(),
        }),
        _ => Err(PlannerIssue::AmbiguousIssueSite {
            issue_key: issue_key.to_owned(),
        }),
    }
}

fn group_candidates_by_issue(
    candidates: &[CandidateEntry],
) -> BTreeMap<(String, String), Vec<usize>> {
    let mut scans = BTreeMap::<(String, String), Vec<usize>>::new();
    for (index, candidate) in candidates.iter().enumerate() {
        scans
            .entry((candidate.site_key.clone(), candidate.issue_key.clone()))
            .or_default()
            .push(index);
    }
    scans
}

async fn read_marker(
    jira: &JiraClient,
    issue_key: &str,
    worklog: &Worklog,
    candidates: &[&CandidateEntry],
) -> Result<Option<FoundMarker>, RecoveryError> {
    match jira.read_marker_property(issue_key, &worklog.id).await {
        Ok(marker) => Ok(candidates.iter().find_map(|candidate| {
            let workspace_matches =
                candidate.entry.workspace_id == marker.toggl_workspace_id.to_string();
            let entry_matches = candidate.entry.entry_id == marker.toggl_entry_id.to_string();
            if workspace_matches && entry_matches {
                Some(FoundMarker {
                    toggl_workspace_id: candidate.entry.workspace_id.clone(),
                    toggl_entry_id: candidate.entry.entry_id.clone(),
                    jira_site_key: candidate.site_key.clone(),
                    jira_issue_key: candidate.issue_key.clone(),
                    jira_worklog_id: worklog.id.clone(),
                    source_hash: marker.source_hash.clone(),
                    rounded_duration_seconds: candidate.rounded_duration_seconds,
                    marker_source: "property",
                })
            } else {
                None
            }
        })),
        Err(error) if is_missing_marker_lookup(&error) => {
            Ok(read_comment_fallback(worklog, candidates))
        }
        Err(error) => Err(error.into()),
    }
}

fn read_comment_fallback(worklog: &Worklog, candidates: &[&CandidateEntry]) -> Option<FoundMarker> {
    let text = collect_text(worklog.comment.as_ref()?);
    candidates.iter().find_map(|candidate| {
        let suffix = format!(
            "[toggl-sync:workspace={};entry={}]",
            candidate.entry.workspace_id, candidate.entry.entry_id
        );
        if text.trim_end().ends_with(&suffix) {
            Some(FoundMarker {
                toggl_workspace_id: candidate.entry.workspace_id.clone(),
                toggl_entry_id: candidate.entry.entry_id.clone(),
                jira_site_key: candidate.site_key.clone(),
                jira_issue_key: candidate.issue_key.clone(),
                jira_worklog_id: worklog.id.clone(),
                source_hash: candidate.source_hash.clone(),
                rounded_duration_seconds: candidate.rounded_duration_seconds,
                marker_source: "comment_fallback",
            })
        } else {
            None
        }
    })
}

fn persist_findings(
    database: &Database,
    findings: Vec<FoundMarker>,
    report: &mut RecoveryReport,
) -> Result<(), RecoveryError> {
    let mut grouped = BTreeMap::<(String, String, String, String), Vec<FoundMarker>>::new();
    for finding in findings {
        grouped
            .entry((
                finding.toggl_workspace_id.clone(),
                finding.toggl_entry_id.clone(),
                finding.jira_site_key.clone(),
                finding.jira_issue_key.clone(),
            ))
            .or_default()
            .push(finding);
    }

    for ((workspace_id, entry_id, site_key, issue_key), mut matches) in grouped {
        matches.sort_by(|left, right| left.jira_worklog_id.cmp(&right.jira_worklog_id));
        let unique_worklog_ids = matches
            .iter()
            .map(|finding| finding.jira_worklog_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        if unique_worklog_ids.len() > 1 {
            for finding in &matches {
                insert_finding(database, finding, "error")?;
            }
            report
                .conflicts
                .push(RecoveryConflict::DuplicateMarkedWorklogs {
                    toggl_workspace_id: workspace_id,
                    toggl_entry_id: entry_id,
                    jira_site_key: site_key,
                    jira_issue_key: issue_key,
                    jira_worklog_ids: unique_worklog_ids,
                });
            continue;
        }

        let finding = matches
            .first()
            .expect("non-empty group should have first finding");
        insert_finding(database, finding, "created")?;
        database.upsert_jira_worklog_link(&NewJiraWorklogLink {
            toggl_workspace_id: &finding.toggl_workspace_id,
            toggl_entry_id: &finding.toggl_entry_id,
            jira_site_key: &finding.jira_site_key,
            jira_issue_key: &finding.jira_issue_key,
            jira_worklog_id: Some(&finding.jira_worklog_id),
            source_hash: &finding.source_hash,
            rounded_duration_seconds: finding.rounded_duration_seconds,
            status: "created",
        })?;
        report.recovered_links += 1;
    }

    Ok(())
}

fn insert_finding(
    database: &Database,
    finding: &FoundMarker,
    status: &'static str,
) -> Result<(), RecoveryError> {
    database.insert_recovery_finding(&NewRecoveryFinding {
        toggl_workspace_id: &finding.toggl_workspace_id,
        toggl_entry_id: &finding.toggl_entry_id,
        jira_site_key: &finding.jira_site_key,
        jira_issue_key: &finding.jira_issue_key,
        jira_worklog_id: &finding.jira_worklog_id,
        source_hash: Some(&finding.source_hash),
        status,
        marker_source: finding.marker_source,
    })?;
    Ok(())
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
