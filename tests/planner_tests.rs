use toggl_jira_sync::{
    sync::planner::{
        plan_sync, ExistingWorklogLink, IssueSiteMapping, PlannedMutation, PlannerInput,
        PlannerIssue, PlannerOutcome, SkipCause,
    },
    toggl::TogglTimeEntry,
};

fn entry(entry_id: &str, description: &str) -> TogglTimeEntry {
    TogglTimeEntry {
        workspace_id: "700001".to_owned(),
        entry_id: entry_id.to_owned(),
        start: "2024-05-02T01:00:00Z".to_owned(),
        duration_seconds: 1_805,
        description: Some(description.to_owned()),
        updated_at: "2024-05-02T03:06:40Z".to_owned(),
        deleted_at: None,
        skip_reason: None,
    }
}

fn deleted_entry(entry_id: &str, description: &str) -> TogglTimeEntry {
    TogglTimeEntry {
        deleted_at: Some("2024-05-02T03:06:40Z".to_owned()),
        ..entry(entry_id, description)
    }
}

fn short_entry(entry_id: &str, description: &str) -> TogglTimeEntry {
    TogglTimeEntry {
        duration_seconds: 29,
        ..entry(entry_id, description)
    }
}

fn marked_link(entry_id: &str, issue_key: &str, source_hash: &str) -> ExistingWorklogLink {
    ExistingWorklogLink {
        toggl_workspace_id: "700001".to_owned(),
        toggl_entry_id: entry_id.to_owned(),
        jira_site_key: "sabservis".to_owned(),
        jira_issue_key: issue_key.to_owned(),
        jira_worklog_id: "10001".to_owned(),
        source_hash: source_hash.to_owned(),
        marked_toggl_managed: true,
    }
}

fn input(entries: Vec<TogglTimeEntry>) -> PlannerInput {
    PlannerInput {
        entries,
        issue_site_mappings: vec![
            IssueSiteMapping {
                issue_key: "SAB-123".to_owned(),
                jira_site_key: "sabservis".to_owned(),
            },
            IssueSiteMapping {
                issue_key: "SAB-456".to_owned(),
                jira_site_key: "sabservis".to_owned(),
            },
            IssueSiteMapping {
                issue_key: "SAB-789".to_owned(),
                jira_site_key: "sabservis".to_owned(),
            },
            IssueSiteMapping {
                issue_key: "BLOGIC-456".to_owned(),
                jira_site_key: "blogic".to_owned(),
            },
        ],
        existing_links: Vec::new(),
    }
}

#[test]
fn planner_create_uses_regex_key_extraction_rounding_and_hash_inputs() {
    let plan = plan_sync(input(vec![entry(
        "900001",
        "Pairing on SAB-123 sync planner",
    )]))
    .expect("planner should not fail globally");

    assert_eq!(plan.mutations.len(), 1);
    let PlannedMutation::Create(create) = &plan.mutations[0] else {
        panic!("expected create mutation, got {:?}", plan.mutations);
    };
    assert_eq!(create.toggl_entry_id, "900001");
    assert_eq!(create.jira_site_key, "sabservis");
    assert_eq!(create.jira_issue_key, "SAB-123");
    assert_eq!(create.draft.time_spent_seconds, 1_800);
    assert_eq!(create.draft.started, "2024-05-02T01:00:00.000+0000");
    assert_eq!(create.draft.comment, "Pairing on SAB-123 sync planner");
    assert_eq!(
        create.source_hash,
        toggl_jira_sync::sync::planner::compute_source_hash(
            "SAB-123",
            "sabservis",
            1_800,
            "2024-05-02T01:00:00Z",
            "Pairing on SAB-123 sync planner",
        )
    );
}

#[test]
fn planner_ignores_section_numbers_that_look_like_issue_keys() {
    let plan = plan_sync(input(vec![entry(
        "900001",
        "US-1.3: Example section title",
    )]))
    .expect("planner should not fail globally");

    assert!(plan.mutations.is_empty());
    assert!(matches!(
        plan.entries[0].outcome,
        PlannerOutcome::Skip(SkipCause::MissingIssueKey)
    ));
}

#[test]
fn planner_unchanged_source_hash_is_no_op() {
    let desired_hash = toggl_jira_sync::sync::planner::compute_source_hash(
        "SAB-123",
        "sabservis",
        1_800,
        "2024-05-02T01:00:00Z",
        "Pairing on SAB-123 sync planner",
    );
    let mut input = input(vec![entry("900001", "Pairing on SAB-123 sync planner")]);
    input.existing_links = vec![marked_link("900001", "SAB-123", &desired_hash)];

    let plan = plan_sync(input).expect("planner should not fail globally");

    assert!(plan.mutations.is_empty());
    assert!(matches!(plan.entries[0].outcome, PlannerOutcome::NoOp));
}

#[test]
fn planner_changed_duration_or_comment_updates_marked_worklog() {
    let old_hash = toggl_jira_sync::sync::planner::compute_source_hash(
        "SAB-123",
        "sabservis",
        1_200,
        "2024-05-02T01:00:00Z",
        "Old SAB-123 comment",
    );
    let mut input = input(vec![entry("900001", "Updated SAB-123 comment")]);
    input.existing_links = vec![marked_link("900001", "SAB-123", &old_hash)];

    let plan = plan_sync(input).expect("planner should not fail globally");

    assert_eq!(plan.mutations.len(), 1);
    assert!(matches!(plan.mutations[0], PlannedMutation::Update(_)));
}

#[test]
fn planner_soft_deleted_known_marked_link_deletes_worklog() {
    let mut input = input(vec![deleted_entry("900001", "SAB-123 removed")]);
    input.existing_links = vec![marked_link("900001", "SAB-123", "sha256:old")];

    let plan = plan_sync(input).expect("planner should not fail globally");

    assert_eq!(plan.mutations.len(), 1);
    let PlannedMutation::Delete(delete) = &plan.mutations[0] else {
        panic!("expected delete mutation, got {:?}", plan.mutations);
    };
    assert_eq!(delete.jira_issue_key, "SAB-123");
    assert_eq!(delete.jira_worklog_id, "10001");
}

#[test]
fn planner_issue_key_change_deletes_old_and_creates_new() {
    let mut input = input(vec![entry("900001", "Moved to SAB-456")]);
    input.existing_links = vec![marked_link("900001", "SAB-123", "sha256:old")];

    let plan = plan_sync(input).expect("planner should not fail globally");

    assert_eq!(plan.mutations.len(), 2);
    assert!(matches!(
        plan.entries[0].outcome,
        PlannerOutcome::Move { .. }
    ));

    let PlannedMutation::Delete(delete) = &plan.mutations[0] else {
        panic!("expected first move step to delete old worklog");
    };
    let PlannedMutation::Create(create) = &plan.mutations[1] else {
        panic!("expected second move step to create new worklog");
    };
    assert_eq!(delete.jira_issue_key, "SAB-123");
    assert_eq!(create.jira_issue_key, "SAB-456");
}

#[test]
fn planner_no_issue_key_skips_without_mutation() {
    let plan = plan_sync(input(vec![entry("900001", "No ticket in this entry")]))
        .expect("planner should not fail globally");

    assert!(plan.mutations.is_empty());
    assert!(matches!(
        plan.entries[0].outcome,
        PlannerOutcome::Skip(SkipCause::MissingIssueKey)
    ));
}

#[test]
fn planner_duration_that_rounds_to_zero_skips_without_mutation() {
    let plan = plan_sync(input(vec![short_entry("900001", "Tiny SAB-123 task")]))
        .expect("planner should not fail globally");

    assert!(plan.mutations.is_empty());
    assert!(matches!(
        plan.entries[0].outcome,
        PlannerOutcome::Skip(SkipCause::RoundedDurationZero)
    ));
}

#[test]
fn planner_multiple_issue_keys_reports_error_without_mutation() {
    let plan = plan_sync(input(vec![entry("900001", "Touches SAB-123 and SAB-456")]))
        .expect("planner should not fail globally");

    assert!(plan.mutations.is_empty());
    assert!(matches!(
        plan.entries[0].outcome,
        PlannerOutcome::Error(PlannerIssue::MultipleIssueKeys { .. })
    ));
}

#[test]
fn planner_multiple_issue_keys_uses_only_resolved_key() {
    let plan = plan_sync(PlannerInput {
        entries: vec![entry("900001", "Touches CORE-269, US-1")],
        issue_site_mappings: vec![IssueSiteMapping {
            issue_key: "CORE-269".to_owned(),
            jira_site_key: "core".to_owned(),
        }],
        existing_links: Vec::new(),
    })
    .expect("planner should not fail globally");

    assert_eq!(plan.mutations.len(), 1);
    let PlannedMutation::Create(create) = &plan.mutations[0] else {
        panic!("expected create mutation, got {:?}", plan.mutations);
    };
    assert_eq!(create.jira_issue_key, "CORE-269");
}

#[test]
fn planner_unresolved_issue_site_skips_without_mutation() {
    let unknown = plan_sync(input(vec![entry("900001", "Unknown ABC-123")]))
        .expect("planner should not fail globally");
    assert!(unknown.mutations.is_empty());
    assert_eq!(
        unknown.entries[0].outcome,
        PlannerOutcome::Skip(SkipCause::UnresolvedIssueSite {
            issue_key: "ABC-123".to_owned()
        })
    );
}

#[test]
fn planner_ambiguous_issue_site_reports_error_without_mutation() {
    let mut ambiguous_input = input(vec![entry("900002", "Ambiguous SAB-123")]);
    ambiguous_input.issue_site_mappings.push(IssueSiteMapping {
        issue_key: "SAB-123".to_owned(),
        jira_site_key: "other-sab".to_owned(),
    });

    let ambiguous = plan_sync(ambiguous_input).expect("planner should not fail globally");
    assert!(ambiguous.mutations.is_empty());
    assert!(matches!(
        ambiguous.entries[0].outcome,
        PlannerOutcome::Error(PlannerIssue::AmbiguousIssueSite { .. })
    ));
}

#[test]
fn planner_issue_errors_have_actionable_messages() {
    assert_eq!(
        PlannerIssue::MultipleIssueKeys {
            issue_keys: vec!["SAB-123".to_owned(), "CORE-456".to_owned()]
        }
        .to_string(),
        "multiple issue keys found: SAB-123, CORE-456"
    );
    assert_eq!(
        PlannerIssue::UnresolvedIssueSite {
            issue_key: "SABV-7736".to_owned()
        }
        .to_string(),
        "Jira issue SABV-7736 was not found on any enabled Jira site"
    );
    assert_eq!(
        PlannerIssue::AmbiguousIssueSite {
            issue_key: "CORE-222".to_owned()
        }
        .to_string(),
        "Jira issue CORE-222 exists on multiple enabled Jira sites"
    );
}

#[test]
fn planner_uses_resolved_issue_site_mapping_not_prefixes() {
    let plan = plan_sync(PlannerInput {
        entries: vec![entry("900001", "SAB-123 belongs to discovered site")],
        issue_site_mappings: vec![IssueSiteMapping {
            issue_key: "SAB-123".to_owned(),
            jira_site_key: "second".to_owned(),
        }],
        existing_links: Vec::new(),
    })
    .expect("planner should not fail globally");

    let PlannedMutation::Create(create) = &plan.mutations[0] else {
        panic!("expected create mutation, got {:?}", plan.mutations);
    };
    assert_eq!(create.jira_site_key, "second");
}

#[test]
fn planner_never_mutates_unmarked_worklog_links() {
    let mut input = input(vec![deleted_entry("900001", "SAB-123 removed")]);
    input.existing_links = vec![ExistingWorklogLink {
        marked_toggl_managed: false,
        ..marked_link("900001", "SAB-123", "sha256:old")
    }];

    let plan = plan_sync(input).expect("planner should not fail globally");

    assert!(plan.mutations.is_empty());
    assert!(matches!(
        plan.entries[0].outcome,
        PlannerOutcome::Skip(SkipCause::UnmarkedWorklog)
    ));
}
