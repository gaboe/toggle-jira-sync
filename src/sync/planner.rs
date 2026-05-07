use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::fmt;
use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::jira::WorklogDraft;
use crate::toggl::{SkipReason as TogglSkipReason, TogglTimeEntry};

static ISSUE_KEY_REGEX: OnceLock<Regex> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannerInput {
    pub entries: Vec<TogglTimeEntry>,
    pub issue_site_mappings: Vec<IssueSiteMapping>,
    pub existing_links: Vec<ExistingWorklogLink>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueSiteMapping {
    pub issue_key: String,
    pub jira_site_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingWorklogLink {
    pub toggl_workspace_id: String,
    pub toggl_entry_id: String,
    pub jira_site_key: String,
    pub jira_issue_key: String,
    pub jira_worklog_id: String,
    pub source_hash: String,
    pub marked_toggl_managed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    pub entries: Vec<PlannedEntry>,
    pub mutations: Vec<PlannedMutation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedEntry {
    pub toggl_workspace_id: String,
    pub toggl_entry_id: String,
    pub outcome: PlannerOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannerOutcome {
    Create,
    Update,
    Delete,
    Move {
        from_issue_key: String,
        to_issue_key: String,
    },
    Skip(SkipCause),
    Error(PlannerIssue),
    NoOp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedMutation {
    Create(PlannedCreate),
    Update(PlannedUpdate),
    Delete(PlannedDelete),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCreate {
    pub toggl_workspace_id: String,
    pub toggl_entry_id: String,
    pub jira_site_key: String,
    pub jira_issue_key: String,
    pub draft: WorklogDraft,
    pub source_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedUpdate {
    pub toggl_workspace_id: String,
    pub toggl_entry_id: String,
    pub jira_site_key: String,
    pub jira_issue_key: String,
    pub jira_worklog_id: String,
    pub draft: WorklogDraft,
    pub previous_source_hash: String,
    pub source_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedDelete {
    pub toggl_workspace_id: String,
    pub toggl_entry_id: String,
    pub jira_site_key: String,
    pub jira_issue_key: String,
    pub jira_worklog_id: String,
    pub source_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipCause {
    MissingIssueKey,
    RoundedDurationZero,
    RunningEntry,
    MissingManagedWorklog,
    UnmarkedWorklog,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannerIssue {
    MultipleIssueKeys { issue_keys: Vec<String> },
    UnresolvedIssueSite { issue_key: String },
    AmbiguousIssueSite { issue_key: String },
}

impl fmt::Display for PlannerIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MultipleIssueKeys { issue_keys } => {
                write!(
                    formatter,
                    "multiple issue keys found: {}",
                    issue_keys.join(", ")
                )
            }
            Self::UnresolvedIssueSite { issue_key } => {
                write!(
                    formatter,
                    "Jira issue {issue_key} was not found on any enabled Jira site"
                )
            }
            Self::AmbiguousIssueSite { issue_key } => {
                write!(
                    formatter,
                    "Jira issue {issue_key} exists on multiple enabled Jira sites"
                )
            }
        }
    }
}

pub fn plan_sync(input: PlannerInput) -> Result<SyncPlan, Infallible> {
    let links = index_links(input.existing_links);
    let issue_sites = index_issue_site_mappings(input.issue_site_mappings);
    let mut entries = Vec::with_capacity(input.entries.len());
    let mut mutations = Vec::new();

    for entry in input.entries {
        let key = (entry.workspace_id.clone(), entry.entry_id.clone());
        let (outcome, entry_mutations) = plan_entry(&entry, &issue_sites, links.get(&key));

        mutations.extend(entry_mutations);
        entries.push(PlannedEntry {
            toggl_workspace_id: entry.workspace_id,
            toggl_entry_id: entry.entry_id,
            outcome,
        });
    }

    Ok(SyncPlan { entries, mutations })
}

pub fn compute_source_hash(
    issue_key: &str,
    site_key: &str,
    rounded_duration_seconds: i64,
    started: &str,
    comment: &str,
) -> String {
    let mut hasher = Sha256::new();
    for part in [
        issue_key,
        site_key,
        &rounded_duration_seconds.to_string(),
        started,
        comment,
    ] {
        hasher.update(part.len().to_string().as_bytes());
        hasher.update(b":");
        hasher.update(part.as_bytes());
        hasher.update(b"|");
    }

    format!("sha256:{}", hex_encode(&hasher.finalize()))
}

fn plan_entry(
    entry: &TogglTimeEntry,
    issue_sites: &HashMap<String, Vec<String>>,
    existing_link: Option<&ExistingWorklogLink>,
) -> (PlannerOutcome, Vec<PlannedMutation>) {
    if existing_link.is_some_and(|link| !link.marked_toggl_managed) {
        return (PlannerOutcome::Skip(SkipCause::UnmarkedWorklog), Vec::new());
    }

    if entry.deleted_at.is_some() {
        return match existing_link {
            Some(link) => {
                let delete = delete_for_link(link);
                (
                    PlannerOutcome::Delete,
                    vec![PlannedMutation::Delete(delete)],
                )
            }
            None => (
                PlannerOutcome::Skip(SkipCause::MissingManagedWorklog),
                Vec::new(),
            ),
        };
    }

    if entry.skip_reason == Some(TogglSkipReason::RunningEntry) || entry.is_running() {
        return (PlannerOutcome::Skip(SkipCause::RunningEntry), Vec::new());
    }

    let comment = entry.description.clone().unwrap_or_default();
    let issue_key = match extract_single_issue_key(&comment) {
        Ok(issue_key) => issue_key,
        Err(outcome) => return (outcome, Vec::new()),
    };
    let site_key = match resolve_site_key(&issue_key, issue_sites) {
        Ok(site_key) => site_key,
        Err(issue) => return (PlannerOutcome::Error(issue), Vec::new()),
    };
    let rounded_duration_seconds = round_to_nearest_minute(entry.duration_seconds);
    if rounded_duration_seconds == 0 {
        return (
            PlannerOutcome::Skip(SkipCause::RoundedDurationZero),
            Vec::new(),
        );
    }

    let source_hash = compute_source_hash(
        &issue_key,
        &site_key,
        rounded_duration_seconds,
        &entry.start,
        &comment,
    );
    let draft = WorklogDraft {
        started: jira_started_datetime(&entry.start),
        time_spent_seconds: rounded_duration_seconds,
        comment,
    };
    let create = PlannedCreate {
        toggl_workspace_id: entry.workspace_id.clone(),
        toggl_entry_id: entry.entry_id.clone(),
        jira_site_key: site_key,
        jira_issue_key: issue_key,
        draft,
        source_hash,
    };

    match existing_link {
        None => (
            PlannerOutcome::Create,
            vec![PlannedMutation::Create(create)],
        ),
        Some(link)
            if link.jira_site_key != create.jira_site_key
                || link.jira_issue_key != create.jira_issue_key =>
        {
            let delete = delete_for_link(link);
            let from_issue_key = link.jira_issue_key.clone();
            let to_issue_key = create.jira_issue_key.clone();
            (
                PlannerOutcome::Move {
                    from_issue_key,
                    to_issue_key,
                },
                vec![
                    PlannedMutation::Delete(delete),
                    PlannedMutation::Create(create),
                ],
            )
        }
        Some(link) if link.source_hash == create.source_hash => (PlannerOutcome::NoOp, Vec::new()),
        Some(link) => {
            let update = PlannedUpdate {
                toggl_workspace_id: create.toggl_workspace_id,
                toggl_entry_id: create.toggl_entry_id,
                jira_site_key: create.jira_site_key,
                jira_issue_key: create.jira_issue_key,
                jira_worklog_id: link.jira_worklog_id.clone(),
                draft: create.draft,
                previous_source_hash: link.source_hash.clone(),
                source_hash: create.source_hash,
            };
            (
                PlannerOutcome::Update,
                vec![PlannedMutation::Update(update)],
            )
        }
    }
}

fn jira_started_datetime(started: &str) -> String {
    if let Some(prefix) = started.strip_suffix('Z') {
        if prefix.contains('.') {
            format!("{prefix}+0000")
        } else {
            format!("{prefix}.000+0000")
        }
    } else if started.len() >= 6 {
        let offset_start = started.len() - 6;
        let offset = &started[offset_start..];
        if (offset.starts_with('+') || offset.starts_with('-')) && offset.as_bytes()[3] == b':' {
            let datetime = &started[..offset_start];
            let offset = offset.replace(':', "");
            if datetime.contains('.') {
                format!("{datetime}{offset}")
            } else {
                format!("{datetime}.000{offset}")
            }
        } else {
            started.to_owned()
        }
    } else {
        started.to_owned()
    }
}

pub fn extract_issue_keys(comment: &str) -> Vec<String> {
    let regex = ISSUE_KEY_REGEX.get_or_init(|| {
        Regex::new(r"\b[A-Z][A-Z0-9]+-\d+\b").expect("issue key regex must compile")
    });
    let mut issue_keys = Vec::new();
    let mut seen = HashSet::new();

    for matched in regex.find_iter(comment) {
        let issue_key = matched.as_str().to_owned();
        if seen.insert(issue_key.clone()) {
            issue_keys.push(issue_key);
        }
    }

    issue_keys
}

fn extract_single_issue_key(comment: &str) -> Result<String, PlannerOutcome> {
    let mut issue_keys = extract_issue_keys(comment);

    match issue_keys.len() {
        0 => Err(PlannerOutcome::Skip(SkipCause::MissingIssueKey)),
        1 => Ok(issue_keys.remove(0)),
        _ => Err(PlannerOutcome::Error(PlannerIssue::MultipleIssueKeys {
            issue_keys,
        })),
    }
}

pub fn resolve_site_key(
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

pub fn round_to_nearest_minute(duration_seconds: i64) -> i64 {
    if duration_seconds <= 0 {
        return 0;
    }

    ((duration_seconds + 30) / 60) * 60
}

fn delete_for_link(link: &ExistingWorklogLink) -> PlannedDelete {
    PlannedDelete {
        toggl_workspace_id: link.toggl_workspace_id.clone(),
        toggl_entry_id: link.toggl_entry_id.clone(),
        jira_site_key: link.jira_site_key.clone(),
        jira_issue_key: link.jira_issue_key.clone(),
        jira_worklog_id: link.jira_worklog_id.clone(),
        source_hash: link.source_hash.clone(),
    }
}

fn index_links(links: Vec<ExistingWorklogLink>) -> HashMap<(String, String), ExistingWorklogLink> {
    links
        .into_iter()
        .map(|link| {
            (
                (link.toggl_workspace_id.clone(), link.toggl_entry_id.clone()),
                link,
            )
        })
        .collect()
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

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
