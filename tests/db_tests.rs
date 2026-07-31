use std::{thread, time::Duration};

use rusqlite::Connection;
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
fn db_opens_existing_rusqlite_file() {
    let file = NamedTempFile::new().expect("temp sqlite file should be created");
    let connection = Connection::open(file.path()).expect("legacy sqlite file should open");
    connection
        .execute_batch(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );",
        )
        .expect("legacy schema table should be created");
    for (version, sql) in [
        (1, include_str!("../migrations/001_sync_ledger.sql")),
        (
            2,
            include_str!("../migrations/002_jira_issue_site_cache.sql"),
        ),
        (3, include_str!("../migrations/003_adopted_status.sql")),
    ] {
        connection
            .execute_batch(sql)
            .expect("legacy migration should run");
        connection
            .execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [version],
            )
            .expect("legacy migration version should insert");
    }
    connection
        .execute(
            "INSERT INTO toggl_entries (
                toggl_workspace_id,
                toggl_entry_id,
                source_hash,
                rounded_duration_seconds,
                status
            ) VALUES ('workspace-1', 'entry-1', 'sha256:entry', 1800, 'planned')",
            [],
        )
        .expect("legacy row should insert");
    drop(connection);

    let db = Database::open(file.path()).expect("turso should open legacy sqlite file");
    db.run_migrations()
        .expect("turso migrations should accept legacy file");

    assert_eq!(db.count_rows("toggl_entries").unwrap(), 1);
}

#[test]
fn db_opens_after_another_tool_checkpointed_the_wal_away() {
    let file = NamedTempFile::new().expect("temp sqlite file should be created");
    let db = Database::open(file.path()).expect("turso should create the db");
    db.run_migrations().expect("migrations should run");
    // Enough writes that the WAL index describes many frames, the way a long-running app
    // leaves it between checkpoints.
    for index in 0..500 {
        let entry_id = format!("entry-{index}");
        let source_hash = format!("sha256:entry-{index}");
        db.upsert_toggl_entry(&NewTogglEntry {
            toggl_workspace_id: "workspace-1",
            toggl_entry_id: &entry_id,
            description: Some("CORE-1 work"),
            extracted_issue_key: Some("CORE-1"),
            source_hash: &source_hash,
            rounded_duration_seconds: 1800,
            status: "planned",
            started_at: Some("2026-07-30T06:00:00Z"),
            stopped_at: Some("2026-07-30T06:30:00Z"),
        })
        .expect("entry should upsert");
    }
    // Leak the handle so the WAL stays uncheckpointed, the way a killed process leaves it.
    std::mem::forget(db);

    // Copy that on-disk state to a path no live handle owns, the way a fresh process finds
    // it after a crash, then let another SQLite tool checkpoint the WAL away.
    let dir = tempfile::tempdir().expect("temp dir");
    let recovered = dir.path().join("toggl-jira-sync.sqlite");
    for suffix in ["", "-wal", "-shm", "-tshm"] {
        let from = format!("{}{suffix}", file.path().display());
        if std::path::Path::new(&from).exists() {
            std::fs::copy(&from, format!("{}{suffix}", recovered.display())).expect("copy sidecar");
        }
    }
    let wal = format!("{}-wal", recovered.display());
    let wal_bytes = std::fs::metadata(&wal).map(|meta| meta.len()).unwrap_or(0);
    assert!(
        wal_bytes > 0,
        "wal should hold frames, got {wal_bytes} bytes"
    );
    std::fs::write(&wal, b"").expect("wal should be emptied");

    // Without the recovery the open fails with "short read on WAL frame". Whatever lived only
    // in the discarded WAL is gone, but the DB is usable again instead of permanently wedged.
    let db = Database::open(&recovered).expect("turso should recover from a stale wal index");
    db.run_migrations()
        .expect("migrations should run after recovery");
    db.count_rows("toggl_entries")
        .expect("recovered db should be queryable");
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
fn db_finish_sync_run_marks_run_terminal() {
    let (file, db) = temp_database();
    db.run_migrations().expect("migrations should run");
    let sync_run_id = db
        .insert_sync_run(&NewSyncRun {
            run_id: "run-finish",
            mode: "sync",
            status: "running",
        })
        .expect("sync run should insert");

    db.finish_sync_run(sync_run_id, "error", Some("rate limited"))
        .expect("sync run should finish");

    let connection = Connection::open(file.path()).expect("sqlite db should open");
    let (status, finished_at, error_message): (String, Option<String>, Option<String>) = connection
        .query_row(
            "SELECT status, finished_at, error_message FROM sync_runs WHERE id = ?1",
            [sync_run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("sync run should be readable");
    assert_eq!(status, "error");
    assert!(finished_at.is_some());
    assert_eq!(error_message.as_deref(), Some("rate limited"));
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

    for (entry_id, description, issue_key, status, started_at) in [
        (
            "entry-synced",
            Some("SAB-101 work"),
            Some("SAB-101"),
            "created",
            "2024-05-02T01:00:00Z",
        ),
        (
            "entry-not-synced",
            Some("SAB-102 work"),
            Some("SAB-102"),
            "planned",
            "2024-05-02T02:00:00Z",
        ),
        (
            "entry-error",
            Some("SAB-103 work"),
            Some("SAB-103"),
            "error",
            "2024-05-02T03:00:00Z",
        ),
        (
            "entry-skipped",
            Some("admin work without a ticket"),
            None,
            "planned",
            "2024-05-02T04:00:00Z",
        ),
    ] {
        db.upsert_toggl_entry(&NewTogglEntry {
            toggl_workspace_id: "workspace-1",
            toggl_entry_id: entry_id,
            description,
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
    assert!(statuses.contains(&(
        "entry-skipped",
        "skipped",
        Some("no valid Jira issue key found in Toggl description")
    )));
}

#[test]
fn blank_placeholder_entries_without_attempts_or_links_are_hidden_after_grace_period() {
    let (_file, db) = temp_database();
    db.run_migrations().expect("migrations should run");

    db.upsert_toggl_entry(&NewTogglEntry {
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "blank-stale-entry",
        description: None,
        extracted_issue_key: None,
        source_hash: "sha256:blank-stale",
        rounded_duration_seconds: 7920,
        status: "planned",
        started_at: Some("2024-05-07T12:14:00Z"),
        stopped_at: Some("2024-05-07T14:27:00Z"),
    })
    .expect("blank stale entry should insert");
    db.upsert_toggl_entry(&NewTogglEntry {
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "described-skipped-entry",
        description: Some("admin work without a ticket"),
        extracted_issue_key: None,
        source_hash: "sha256:described-skipped",
        rounded_duration_seconds: 1800,
        status: "planned",
        started_at: Some("2024-05-08T12:14:00Z"),
        stopped_at: Some("2024-05-08T12:44:00Z"),
    })
    .expect("described skipped entry should insert");

    let rows = db.list_status_entries(10).expect("status rows should load");

    assert!(!rows.iter().any(|row| row.entry == "blank-stale-entry"));
    assert!(rows
        .iter()
        .any(|row| row.entry == "described-skipped-entry"));
}

#[test]
fn deleted_toggl_entries_are_hidden_from_status_rows() {
    let (_file, db) = temp_database();
    db.run_migrations().expect("migrations should run");

    db.upsert_toggl_entry(&NewTogglEntry {
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "stale-entry",
        description: Some("old local entry without issue"),
        extracted_issue_key: None,
        source_hash: "sha256:stale",
        rounded_duration_seconds: 7920,
        status: "planned",
        started_at: Some("2026-05-07T12:14:00Z"),
        stopped_at: Some("2026-05-07T14:27:00Z"),
    })
    .expect("stale entry should insert");
    db.upsert_toggl_entry(&NewTogglEntry {
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "seen-entry",
        description: Some("CORE-1 active entry"),
        extracted_issue_key: Some("CORE-1"),
        source_hash: "sha256:seen",
        rounded_duration_seconds: 1800,
        status: "planned",
        started_at: Some("2026-05-08T12:14:00Z"),
        stopped_at: Some("2026-05-08T12:44:00Z"),
    })
    .expect("seen entry should insert");

    let deleted = db
        .mark_missing_toggl_entries_deleted(
            "workspace-1",
            "2026-05-01T00:00:00Z",
            &["seen-entry".to_owned()],
        )
        .expect("missing entries should be marked deleted");

    assert_eq!(deleted, 1);
    let rows = db.list_status_entries(10).expect("status rows should load");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].entry, "seen-entry");
}

#[test]
fn retryable_error_entries_include_unsynced_historical_failures() {
    let (_file, db) = temp_database();
    db.run_migrations().expect("migrations should run");
    let run_id = db
        .insert_sync_run(&NewSyncRun {
            run_id: "retry-run-1",
            mode: "sync",
            status: "completed",
        })
        .expect("sync run should insert");

    for (entry_id, status, started_at) in [
        ("retry-entry", "error", "2024-05-02T02:30:00Z"),
        ("linked-entry", "error", "2024-05-02T03:30:00Z"),
        ("deleted-entry", "error", "2024-05-02T04:30:00Z"),
        ("planned-entry", "planned", "2024-05-02T05:30:00Z"),
    ] {
        db.upsert_toggl_entry(&NewTogglEntry {
            toggl_workspace_id: "workspace-1",
            toggl_entry_id: entry_id,
            description: Some("CORE-269 implementation"),
            extracted_issue_key: Some("CORE-269"),
            source_hash: &format!("sha256:{entry_id}"),
            rounded_duration_seconds: 1800,
            status,
            started_at: Some(started_at),
            stopped_at: Some("2024-05-02T03:00:00Z"),
        })
        .expect("toggl entry should upsert");
    }

    db.insert_sync_attempt(&NewSyncAttempt {
        sync_run_id: Some(run_id),
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "planned-entry",
        jira_site_key: None,
        jira_issue_key: Some("CORE-269"),
        jira_worklog_id: None,
        status: "error",
        error_message: Some("old resolver error"),
    })
    .expect("error attempt should insert");
    db.upsert_jira_worklog_link(&NewJiraWorklogLink {
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "linked-entry",
        jira_site_key: "core",
        jira_issue_key: "CORE-269",
        jira_worklog_id: Some("10001"),
        source_hash: "sha256:linked-entry",
        rounded_duration_seconds: 1800,
        status: "created",
    })
    .expect("linked entry should have active worklog");
    db.mark_toggl_entry_deleted("workspace-1", "deleted-entry")
        .expect("deleted entry should mark deleted");

    let retryable = db
        .list_retryable_error_toggl_entries("workspace-1")
        .expect("retryable entries should load");
    let ids = retryable
        .iter()
        .map(|entry| entry.entry.as_str())
        .collect::<Vec<_>>();

    assert_eq!(ids, vec!["planned-entry", "retry-entry"]);
    assert_eq!(
        retryable[0].description.as_deref(),
        Some("CORE-269 implementation")
    );
    assert_eq!(retryable[0].rounded_duration_seconds, 1800);
    assert_eq!(retryable[0].started_at, "2024-05-02T05:30:00Z");
    assert_eq!(
        retryable[0].stopped_at.as_deref(),
        Some("2024-05-02T03:00:00Z")
    );
}

#[test]
fn pending_local_entries_include_completed_planned_rows_without_active_links() {
    let (_file, db) = temp_database();
    db.run_migrations().expect("migrations should run");
    let sync_run_id = db
        .insert_sync_run(&NewSyncRun {
            run_id: "pending-local-run",
            mode: "dry_run",
            status: "planned",
        })
        .expect("sync run should insert");

    for (entry_id, issue_key, duration, status, stopped_at) in [
        (
            "pending-entry",
            Some("CORE-297"),
            9_660,
            "planned",
            Some("2024-05-02T07:11:00Z"),
        ),
        (
            "latest-planned-entry",
            Some("CORE-297"),
            1_800,
            "created",
            Some("2024-05-02T08:30:00Z"),
        ),
        (
            "no-issue-entry",
            None,
            1_800,
            "planned",
            Some("2024-05-02T09:30:00Z"),
        ),
        ("running-entry", Some("CORE-297"), 0, "planned", None),
        (
            "linked-entry",
            Some("CORE-297"),
            1_800,
            "planned",
            Some("2024-05-02T10:30:00Z"),
        ),
    ] {
        db.upsert_toggl_entry(&NewTogglEntry {
            toggl_workspace_id: "workspace-1",
            toggl_entry_id: entry_id,
            description: Some("CORE-297 implementation"),
            extracted_issue_key: issue_key,
            source_hash: "sha256:pending",
            rounded_duration_seconds: duration,
            status,
            started_at: Some("2024-05-02T04:30:00Z"),
            stopped_at,
        })
        .expect("toggl entry should upsert");
    }

    db.insert_sync_attempt(&NewSyncAttempt {
        sync_run_id: Some(sync_run_id),
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "latest-planned-entry",
        jira_site_key: None,
        jira_issue_key: Some("CORE-297"),
        jira_worklog_id: None,
        status: "planned",
        error_message: None,
    })
    .expect("planned attempt should insert");
    db.upsert_jira_worklog_link(&NewJiraWorklogLink {
        toggl_workspace_id: "workspace-1",
        toggl_entry_id: "linked-entry",
        jira_site_key: "sabservis",
        jira_issue_key: "CORE-297",
        jira_worklog_id: Some("10001"),
        source_hash: "sha256:linked",
        rounded_duration_seconds: 1_800,
        status: "created",
    })
    .expect("active link should insert");

    let pending = db
        .list_pending_local_toggl_entries("workspace-1")
        .expect("pending entries should load");
    let ids = pending
        .iter()
        .map(|entry| entry.entry.as_str())
        .collect::<Vec<_>>();

    assert_eq!(ids, vec!["latest-planned-entry", "pending-entry"]);
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
