use std::{thread, time::Duration};

use tempfile::NamedTempFile;
use toggl_jira_sync::db::{
    Database, DbError, NewJiraIssueSiteCache, NewJiraWorklogLink, NewRecoveryFinding,
    NewSyncAttempt, NewSyncCursor, NewSyncRun, NewTogglEntry,
};

fn temp_database() -> (NamedTempFile, Database) {
    let file = NamedTempFile::new().expect("temp sqlite file should be created");
    let database = Database::open(file.path()).expect("temp sqlite database should open");
    (file, database)
}

#[test]
fn db_migrations_create_expected_tables() {
    let (_file, db) = temp_database();

    db.run_migrations().expect("migrations should run");

    let tables = db.table_names().expect("table names should be readable");
    for table in [
        "schema_migrations",
        "sync_runs",
        "sync_cursors",
        "toggl_entries",
        "jira_worklog_links",
        "jira_issue_site_cache",
        "sync_attempts",
        "recovery_findings",
        "locks",
    ] {
        assert!(
            tables.iter().any(|name| name == table),
            "expected table {table} in {tables:?}"
        );
    }
}

#[test]
fn db_jira_issue_site_cache_upserts_by_issue_key() {
    let (_file, db) = temp_database();
    db.run_migrations().expect("migrations should run");

    assert!(db
        .get_jira_issue_site_cache("SAB-123")
        .expect("cache lookup should succeed")
        .is_none());

    db.upsert_jira_issue_site_cache(&NewJiraIssueSiteCache {
        issue_key: "SAB-123",
        jira_site_key: "sabservis",
        source: "discovery",
    })
    .expect("cache insert should succeed");

    let cached = db
        .get_jira_issue_site_cache("SAB-123")
        .expect("cache lookup should succeed")
        .expect("cache row should exist");
    assert_eq!(cached.issue_key, "SAB-123");
    assert_eq!(cached.jira_site_key, "sabservis");
    assert_eq!(cached.source, "discovery");
    assert!(!cached.discovered_at.is_empty());
    assert!(!cached.confirmed_at.is_empty());

    db.upsert_jira_issue_site_cache(&NewJiraIssueSiteCache {
        issue_key: "SAB-123",
        jira_site_key: "blogic",
        source: "cache_refresh",
    })
    .expect("cache update should succeed");

    let refreshed = db
        .get_jira_issue_site_cache("SAB-123")
        .expect("cache lookup should succeed")
        .expect("cache row should still exist");
    assert_eq!(refreshed.jira_site_key, "blogic");
    assert_eq!(refreshed.source, "cache_refresh");
    assert_eq!(db.count_rows("jira_issue_site_cache").unwrap(), 1);
}

#[test]
fn db_jira_worklog_links_enforce_unique_toggl_entry_per_site() {
    let (_file, db) = temp_database();
    db.run_migrations().expect("migrations should run");

    let link = NewJiraWorklogLink {
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "entry-1",
        jira_site_key: "sabservis",
        jira_issue_key: "SAB-123",
        jira_worklog_id: Some("10001"),
        source_hash: "sha256:first",
        rounded_duration_seconds: 1800,
        status: "created",
    };

    db.insert_jira_worklog_link(&link)
        .expect("first link insert should succeed");

    let duplicate = NewJiraWorklogLink {
        jira_worklog_id: Some("10002"),
        source_hash: "sha256:second",
        ..link
    };
    let error = db
        .insert_jira_worklog_link(&duplicate)
        .expect_err("duplicate link should be rejected");

    match error {
        DbError::DuplicateJiraWorklogLink {
            toggl_workspace_id,
            toggl_entry_id,
            jira_site_key,
        } => {
            assert_eq!(toggl_workspace_id, "workspace-1");
            assert_eq!(toggl_entry_id, "entry-1");
            assert_eq!(jira_site_key, "sabservis");
        }
        error => panic!("expected typed duplicate error, got {error:?}"),
    }
}

#[test]
fn db_repository_apis_write_required_ledgers() {
    let (_file, db) = temp_database();
    db.run_migrations().expect("migrations should run");

    let sync_run_id = db
        .insert_sync_run(&NewSyncRun {
            run_id: "run-1",
            mode: "sync",
            status: "running",
        })
        .expect("sync run should insert");

    db.upsert_sync_cursor(&NewSyncCursor {
        toggl_workspace_id: "workspace-1",
        last_toggl_since: Some(1_714_604_800),
        last_successful_sync_at: Some("2024-05-02T03:06:40Z"),
    })
    .expect("sync cursor should upsert");

    db.upsert_toggl_entry(&NewTogglEntry {
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "entry-1",
        description: Some("SAB-123 implementation"),
        extracted_issue_key: Some("SAB-123"),
        source_hash: "sha256:entry",
        rounded_duration_seconds: 1800,
        status: "planned",
        started_at: Some("2024-05-02T02:30:00Z"),
        stopped_at: Some("2024-05-02T03:00:00Z"),
    })
    .expect("toggl entry should upsert");

    db.insert_sync_attempt(&NewSyncAttempt {
        sync_run_id: Some(sync_run_id),
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "entry-1",
        jira_site_key: Some("sabservis"),
        jira_issue_key: Some("SAB-123"),
        jira_worklog_id: None,
        status: "planned",
        error_message: None,
    })
    .expect("sync attempt should insert");

    db.insert_recovery_finding(&NewRecoveryFinding {
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "entry-1",
        jira_site_key: "sabservis",
        jira_issue_key: "SAB-123",
        jira_worklog_id: "10001",
        source_hash: Some("sha256:entry"),
        status: "orphaned",
        marker_source: "property",
    })
    .expect("recovery finding should insert");

    assert_eq!(db.count_rows("sync_runs").unwrap(), 1);
    assert_eq!(db.count_rows("sync_cursors").unwrap(), 1);
    assert_eq!(db.count_rows("toggl_entries").unwrap(), 1);
    assert_eq!(db.count_rows("sync_attempts").unwrap(), 1);
    assert_eq!(db.count_rows("recovery_findings").unwrap(), 1);
}

#[test]
fn db_status_rows_derive_local_sync_state() {
    let (_file, db) = temp_database();
    db.run_migrations().expect("migrations should run");

    let run_id = db
        .insert_sync_run(&NewSyncRun {
            run_id: "status-run-1",
            mode: "sync",
            status: "completed",
        })
        .expect("sync run should insert");

    for (entry_id, issue_key, status, started_at) in [
        (
            "entry-synced",
            Some("SAB-101"),
            "created",
            "2024-05-02T01:00:00Z",
        ),
        (
            "entry-not-synced",
            Some("SAB-102"),
            "planned",
            "2024-05-02T02:00:00Z",
        ),
        (
            "entry-error",
            Some("SAB-103"),
            "error",
            "2024-05-02T03:00:00Z",
        ),
        ("entry-skipped", None, "planned", "2024-05-02T04:00:00Z"),
    ] {
        db.upsert_toggl_entry(&NewTogglEntry {
            toggl_workspace_id: "workspace-1",
            toggl_entry_id: entry_id,
            description: issue_key.map(|key| format!("{key} work")).as_deref(),
            extracted_issue_key: issue_key,
            source_hash: &format!("sha256:{entry_id}"),
            rounded_duration_seconds: 1800,
            status,
            started_at: Some(started_at),
            stopped_at: None,
        })
        .expect("toggl entry should upsert");
    }

    db.upsert_jira_worklog_link(&NewJiraWorklogLink {
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "entry-synced",
        jira_site_key: "sabservis",
        jira_issue_key: "SAB-101",
        jira_worklog_id: Some("10001"),
        source_hash: "sha256:entry-synced",
        rounded_duration_seconds: 1800,
        status: "created",
    })
    .expect("worklog link should upsert");

    db.insert_sync_attempt(&NewSyncAttempt {
        sync_run_id: Some(run_id),
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "entry-error",
        jira_site_key: Some("sabservis"),
        jira_issue_key: Some("SAB-103"),
        jira_worklog_id: None,
        status: "error",
        error_message: Some("Jira rejected worklog"),
    })
    .expect("error sync attempt should insert");

    let rows = db.list_status_entries(10).expect("status rows should load");
    let statuses: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                row.entry.as_str(),
                row.status.as_str(),
                row.reason.as_deref(),
            )
        })
        .collect();

    assert!(statuses.contains(&("entry-synced", "synced", None)));
    assert!(statuses.contains(&("entry-not-synced", "not_synced", None)));
    assert!(statuses.contains(&("entry-error", "error", Some("Jira rejected worklog"))));
    assert!(statuses.contains(&("entry-skipped", "skipped", Some("missing issue key"))));
}

#[test]
fn db_lock_rejects_second_owner() {
    let file = NamedTempFile::new().expect("temp sqlite file should be created");
    let first = Database::open(file.path()).expect("first connection should open");
    let second = Database::open(file.path()).expect("second connection should open");
    first.run_migrations().expect("migrations should run");
    second
        .run_migrations()
        .expect("migrations should be idempotent");

    let _guard = first
        .acquire_sync_lock("owner-1")
        .expect("first owner should acquire sync lock");
    let error = second
        .acquire_sync_lock("owner-2")
        .expect_err("second owner should be rejected");

    assert!(
        matches!(
            error,
            DbError::SyncAlreadyRunning {
                lock_name: "sync",
                owner: Some(ref owner),
                ..
            } if owner == "owner-1"
        ),
        "expected typed SyncAlreadyRunning error, got {error:?}"
    );
}

#[test]
fn db_lock_expired_owner_cannot_release_new_owner_lock() {
    let file = NamedTempFile::new().expect("temp sqlite file should be created");
    let first = Database::open(file.path()).expect("first connection should open");
    let second = Database::open(file.path()).expect("second connection should open");
    let third = Database::open(file.path()).expect("third connection should open");
    first.run_migrations().expect("migrations should run");

    let first_guard = first
        .acquire_sync_lock_with_timeout("owner-1", Duration::from_secs(1))
        .expect("first owner should acquire sync lock");
    thread::sleep(Duration::from_millis(1_100));

    let _second_guard = second
        .acquire_sync_lock("owner-2")
        .expect("second owner should acquire stale lock");
    drop(first_guard);

    let error = third
        .acquire_sync_lock("owner-3")
        .expect_err("third owner should still be rejected by owner-2 lock");

    assert!(
        matches!(
            error,
            DbError::SyncAlreadyRunning {
                owner: Some(ref owner),
                ..
            } if owner == "owner-2"
        ),
        "expected owner-2 lock to remain active, got {error:?}"
    );
}
