use std::fmt;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use futures::executor::block_on;
use turso::{params, params_from_iter, Connection, IntoParams, Row, Value};

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../../migrations/001_sync_ledger.sql")),
    (
        2,
        include_str!("../../migrations/002_jira_issue_site_cache.sql"),
    ),
    (3, include_str!("../../migrations/003_adopted_status.sql")),
];
const DEFAULT_STALE_LOCK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const SYNC_LOCK_NAME: &str = "sync";

#[derive(Debug)]
pub enum DbError {
    Turso(turso::Error),
    DuplicateJiraWorklogLink {
        toggl_workspace_id: String,
        toggl_entry_id: String,
        jira_site_key: String,
    },
    SyncAlreadyRunning {
        lock_name: &'static str,
        owner: Option<String>,
        expires_at: Option<String>,
    },
    UnknownTable(String),
    NonUtf8Path(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Turso(error) => write!(formatter, "turso error: {error}"),
            Self::DuplicateJiraWorklogLink {
                toggl_workspace_id,
                toggl_entry_id,
                jira_site_key,
            } => write!(
                formatter,
                "duplicate Jira worklog link for Toggl workspace {toggl_workspace_id}, entry {toggl_entry_id}, site {jira_site_key}"
            ),
            Self::SyncAlreadyRunning {
                lock_name,
                owner,
                expires_at,
            } => write!(
                formatter,
                "{lock_name} is already running{}{}",
                owner
                    .as_ref()
                    .map(|owner| format!(" by {owner}"))
                    .unwrap_or_default(),
                expires_at
                    .as_ref()
                    .map(|expires_at| format!(" until {expires_at}"))
                    .unwrap_or_default()
            ),
            Self::UnknownTable(table_name) => write!(formatter, "unknown DB table {table_name}"),
            Self::NonUtf8Path(path) => {
                write!(formatter, "local DB path is not valid UTF-8: {path}")
            }
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Turso(error) => Some(error),
            Self::DuplicateJiraWorklogLink { .. }
            | Self::SyncAlreadyRunning { .. }
            | Self::UnknownTable(_)
            | Self::NonUtf8Path(_) => None,
        }
    }
}

impl From<turso::Error> for DbError {
    fn from(error: turso::Error) -> Self {
        Self::Turso(error)
    }
}

pub type DbResult<T> = Result<T, DbError>;

#[derive(Debug)]
pub struct Database {
    connection: Connection,
}

#[derive(Debug, Clone, Copy)]
pub struct NewJiraWorklogLink<'a> {
    pub toggl_workspace_id: &'a str,
    pub toggl_entry_id: &'a str,
    pub jira_site_key: &'a str,
    pub jira_issue_key: &'a str,
    pub jira_worklog_id: Option<&'a str>,
    pub source_hash: &'a str,
    pub rounded_duration_seconds: i64,
    pub status: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredJiraWorklogLink {
    pub toggl_workspace_id: String,
    pub toggl_entry_id: String,
    pub jira_site_key: String,
    pub jira_issue_key: String,
    pub jira_worklog_id: Option<String>,
    pub source_hash: String,
    pub rounded_duration_seconds: i64,
    pub status: String,
}

#[derive(Debug, Clone, Copy)]
pub struct NewJiraIssueSiteCache<'a> {
    pub issue_key: &'a str,
    pub jira_site_key: &'a str,
    pub source: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredJiraIssueSiteCache {
    pub issue_key: String,
    pub jira_site_key: String,
    pub discovered_at: String,
    pub confirmed_at: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredStatusEntry {
    pub workspace: String,
    pub entry: String,
    pub started_at: Option<String>,
    pub stopped_at: Option<String>,
    pub rounded_duration_seconds: i64,
    pub issue_key: Option<String>,
    pub site: Option<String>,
    pub worklog_id: Option<String>,
    pub status: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRetryableTogglEntry {
    pub workspace: String,
    pub entry: String,
    pub description: Option<String>,
    pub rounded_duration_seconds: i64,
    pub started_at: String,
    pub stopped_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPendingLocalTogglEntry {
    pub workspace: String,
    pub entry: String,
    pub description: Option<String>,
    pub rounded_duration_seconds: i64,
    pub started_at: String,
    pub stopped_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLinkedLocalTogglEntry {
    pub workspace: String,
    pub entry: String,
    pub description: Option<String>,
    pub rounded_duration_seconds: i64,
    pub started_at: String,
    pub stopped_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredOpenTogglEntry {
    pub workspace: String,
    pub entry: String,
    pub started_at: String,
}

#[derive(Debug, Clone, Copy)]
pub struct NewSyncRun<'a> {
    pub run_id: &'a str,
    pub mode: &'a str,
    pub status: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct NewSyncCursor<'a> {
    pub toggl_workspace_id: &'a str,
    pub last_toggl_since: Option<i64>,
    pub last_successful_sync_at: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct NewTogglEntry<'a> {
    pub toggl_workspace_id: &'a str,
    pub toggl_entry_id: &'a str,
    pub description: Option<&'a str>,
    pub extracted_issue_key: Option<&'a str>,
    pub source_hash: &'a str,
    pub rounded_duration_seconds: i64,
    pub status: &'a str,
    pub started_at: Option<&'a str>,
    pub stopped_at: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct NewSyncAttempt<'a> {
    pub sync_run_id: Option<i64>,
    pub toggl_workspace_id: &'a str,
    pub toggl_entry_id: &'a str,
    pub jira_site_key: Option<&'a str>,
    pub jira_issue_key: Option<&'a str>,
    pub jira_worklog_id: Option<&'a str>,
    pub status: &'a str,
    pub error_message: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct NewRecoveryFinding<'a> {
    pub toggl_workspace_id: &'a str,
    pub toggl_entry_id: &'a str,
    pub jira_site_key: &'a str,
    pub jira_issue_key: &'a str,
    pub jira_worklog_id: &'a str,
    pub source_hash: Option<&'a str>,
    pub status: &'a str,
    pub marker_source: &'a str,
}

#[derive(Debug)]
pub struct SyncRunLock<'a> {
    database: &'a Database,
    token: String,
    released: bool,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> DbResult<Self> {
        let path = path.as_ref();
        // Lossy conversion would open (and create) a different file than the caller asked for.
        let path = path
            .to_str()
            .ok_or_else(|| DbError::NonUtf8Path(path.display().to_string()))?;
        match Self::connect(path) {
            Ok(database) => Ok(database),
            Err(error) => {
                if !is_stale_wal_index_error(&error) {
                    return Err(error);
                }
                match quarantine_stale_wal_index(path) {
                    Some(quarantined) => {
                        // Loud on purpose: the truncated WAL took uncheckpointed transactions
                        // with it, and if those included jira_worklog_links rows the next sync
                        // re-posts worklogs that already exist in Jira. `recover` repairs those.
                        let warning = format!(
                            "local DB WAL index was stale and has been set aside as {quarantined}; \
                             transactions that were only in the discarded WAL are lost. \
                             Run `toggl-jira-sync recover` to check Jira for duplicate worklogs."
                        );
                        tracing::warn!("{warning}");
                        eprintln!("warning: {warning}");
                        // Keep the original error if the retry fails; it is the real diagnostic.
                        Self::connect(path).map_err(|_| error)
                    }
                    None => Err(error),
                }
            }
        }
    }

    fn connect(path: &str) -> DbResult<Self> {
        let builder = turso::Builder::new_local(path);
        let builder = if path == ":memory:" {
            builder
        } else {
            builder.experimental_multiprocess_wal(true)
        };
        let db = block_on(builder.build())?;
        let connection = db.connect()?;
        block_on(connection.execute("PRAGMA foreign_keys = ON", ()))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { connection })
    }

    pub fn run_migrations(&self) -> DbResult<()> {
        block_on(self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );",
        ))?;

        for (version, sql) in MIGRATIONS {
            let already_applied: bool = self
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = ?1)",
                    (*version,),
                )?
                .map(|row| row_bool(&row, 0))
                .transpose()?
                .unwrap_or(false);

            if !already_applied {
                block_on(self.connection.execute_batch(format!(
                    "BEGIN IMMEDIATE;\n{sql}\nINSERT OR IGNORE INTO schema_migrations (version) VALUES ({version});\nCOMMIT;"
                )))?;
            }
        }

        Ok(())
    }

    pub fn table_names(&self) -> DbResult<Vec<String>> {
        let rows = self.query_all(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
            (),
        )?;
        let tables = rows
            .iter()
            .map(|row| row_string(row, 0))
            .collect::<DbResult<Vec<_>>>()?;

        Ok(tables)
    }

    pub fn insert_jira_worklog_link(&self, link: &NewJiraWorklogLink<'_>) -> DbResult<()> {
        let result = self.execute(
            "INSERT INTO jira_worklog_links (
                toggl_workspace_id,
                toggl_entry_id,
                jira_site_key,
                jira_issue_key,
                jira_worklog_id,
                source_hash,
                rounded_duration_seconds,
                status,
                last_synced_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            params![
                link.toggl_workspace_id,
                link.toggl_entry_id,
                link.jira_site_key,
                link.jira_issue_key,
                link.jira_worklog_id,
                link.source_hash,
                link.rounded_duration_seconds,
                link.status,
            ],
        );

        match result {
            Ok(_) => Ok(()),
            Err(error) if is_constraint_violation(&error) => {
                Err(DbError::DuplicateJiraWorklogLink {
                    toggl_workspace_id: link.toggl_workspace_id.to_owned(),
                    toggl_entry_id: link.toggl_entry_id.to_owned(),
                    jira_site_key: link.jira_site_key.to_owned(),
                })
            }
            Err(error) => Err(error),
        }
    }

    pub fn upsert_jira_worklog_link(&self, link: &NewJiraWorklogLink<'_>) -> DbResult<()> {
        self.execute(
            "INSERT INTO jira_worklog_links (
                toggl_workspace_id,
                toggl_entry_id,
                jira_site_key,
                jira_issue_key,
                jira_worklog_id,
                source_hash,
                rounded_duration_seconds,
                status,
                deleted_at,
                last_seen_at,
                last_synced_at,
                updated_at
            ) VALUES (
                ?1,
                ?2,
                ?3,
                ?4,
                ?5,
                ?6,
                ?7,
                ?8,
                NULL,
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            )
            ON CONFLICT(toggl_workspace_id, toggl_entry_id, jira_site_key) DO UPDATE SET
                jira_issue_key = excluded.jira_issue_key,
                jira_worklog_id = excluded.jira_worklog_id,
                source_hash = excluded.source_hash,
                rounded_duration_seconds = excluded.rounded_duration_seconds,
                status = excluded.status,
                deleted_at = NULL,
                last_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                last_synced_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            params![
                link.toggl_workspace_id,
                link.toggl_entry_id,
                link.jira_site_key,
                link.jira_issue_key,
                link.jira_worklog_id,
                link.source_hash,
                link.rounded_duration_seconds,
                link.status,
            ],
        )?;
        Ok(())
    }

    pub fn get_jira_worklog_link(
        &self,
        toggl_workspace_id: &str,
        toggl_entry_id: &str,
        jira_site_key: &str,
    ) -> DbResult<Option<StoredJiraWorklogLink>> {
        self.query_one(
            "SELECT
                    toggl_workspace_id,
                    toggl_entry_id,
                    jira_site_key,
                    jira_issue_key,
                    jira_worklog_id,
                    source_hash,
                    rounded_duration_seconds,
                    status
                 FROM jira_worklog_links
                 WHERE toggl_workspace_id = ?1
                   AND toggl_entry_id = ?2
                   AND jira_site_key = ?3
                   AND deleted_at IS NULL",
            params![toggl_workspace_id, toggl_entry_id, jira_site_key],
        )
        .and_then(|row| row.map(|row| jira_worklog_link_from_row(&row)).transpose())
    }

    pub fn list_active_jira_worklog_links(&self) -> DbResult<Vec<StoredJiraWorklogLink>> {
        let rows = self.query_all(
            "SELECT
                toggl_workspace_id,
                toggl_entry_id,
                jira_site_key,
                jira_issue_key,
                jira_worklog_id,
                source_hash,
                rounded_duration_seconds,
                status
             FROM jira_worklog_links
             WHERE deleted_at IS NULL
               AND jira_worklog_id IS NOT NULL
             ORDER BY toggl_workspace_id, toggl_entry_id, jira_site_key",
            (),
        )?;

        let links = rows
            .iter()
            .map(jira_worklog_link_from_row)
            .collect::<DbResult<Vec<_>>>()?;

        Ok(links)
    }

    pub fn get_jira_issue_site_cache(
        &self,
        issue_key: &str,
    ) -> DbResult<Option<StoredJiraIssueSiteCache>> {
        self.query_one(
            "SELECT issue_key, jira_site_key, discovered_at, confirmed_at, source
                 FROM jira_issue_site_cache
                 WHERE issue_key = ?1",
            (issue_key,),
        )
        .and_then(|row| {
            row.map(|row| jira_issue_site_cache_from_row(&row))
                .transpose()
        })
    }

    pub fn upsert_jira_issue_site_cache(&self, cache: &NewJiraIssueSiteCache<'_>) -> DbResult<()> {
        self.execute(
            "INSERT INTO jira_issue_site_cache (
                issue_key,
                jira_site_key,
                discovered_at,
                confirmed_at,
                source,
                updated_at
            ) VALUES (
                ?1,
                ?2,
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                ?3,
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            )
            ON CONFLICT(issue_key) DO UPDATE SET
                jira_site_key = excluded.jira_site_key,
                confirmed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                source = excluded.source,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            params![cache.issue_key, cache.jira_site_key, cache.source],
        )?;
        Ok(())
    }

    pub fn list_status_entries(&self, limit: usize) -> DbResult<Vec<StoredStatusEntry>> {
        let rows = self.query_all(
            "WITH latest_attempt AS (
                SELECT attempt.*
                FROM sync_attempts attempt
                INNER JOIN (
                    SELECT toggl_workspace_id, toggl_entry_id, MAX(id) AS id
                    FROM sync_attempts
                    GROUP BY toggl_workspace_id, toggl_entry_id
                ) latest ON latest.id = attempt.id
             )
             SELECT
                entry.toggl_workspace_id,
                entry.toggl_entry_id,
                entry.started_at,
                entry.stopped_at,
                entry.rounded_duration_seconds,
                COALESCE(link.jira_issue_key, latest_attempt.jira_issue_key, entry.extracted_issue_key) AS issue_key,
                COALESCE(link.jira_site_key, latest_attempt.jira_site_key) AS site,
                COALESCE(link.jira_worklog_id, latest_attempt.jira_worklog_id) AS worklog_id,
                CASE
                    WHEN latest_attempt.status = 'error' OR entry.status = 'error' OR link.status = 'error' THEN 'error'
                    WHEN link.jira_worklog_id IS NOT NULL AND link.status IN ('created', 'updated', 'adopted') THEN 'synced'
                    WHEN entry.extracted_issue_key IS NULL THEN 'skipped'
                    WHEN entry.rounded_duration_seconds = 0 THEN 'skipped'
                    ELSE 'not_synced'
                END AS derived_status,
                CASE
                    WHEN latest_attempt.status = 'error' THEN COALESCE(latest_attempt.error_message, 'last sync attempt failed')
                    WHEN entry.status = 'error' THEN 'toggl entry is marked error'
                    WHEN link.status = 'error' THEN 'jira worklog link is marked error'
                    WHEN link.jira_worklog_id IS NOT NULL AND link.status = 'adopted' THEN 'Already written by another sync or user; linked locally without changing Jira.'
                    WHEN link.jira_worklog_id IS NOT NULL AND link.status IN ('created', 'updated') THEN NULL
                    WHEN entry.rounded_duration_seconds = 0 AND entry.stopped_at IS NULL THEN 'running entry'
                    WHEN entry.extracted_issue_key IS NULL THEN 'no valid Jira issue key found in Toggl description'
                    WHEN entry.rounded_duration_seconds = 0 THEN 'rounded duration is zero'
                    ELSE NULL
                END AS reason
              FROM toggl_entries entry
             LEFT JOIN jira_worklog_links link
                ON link.toggl_workspace_id = entry.toggl_workspace_id
               AND link.toggl_entry_id = entry.toggl_entry_id
               AND link.deleted_at IS NULL
              LEFT JOIN latest_attempt
                ON latest_attempt.toggl_workspace_id = entry.toggl_workspace_id
               AND latest_attempt.toggl_entry_id = entry.toggl_entry_id
              WHERE entry.deleted_at IS NULL
                AND NOT (
                    entry.extracted_issue_key IS NULL
                    AND COALESCE(NULLIF(TRIM(entry.description), ''), '') = ''
                    AND latest_attempt.id IS NULL
                    AND link.id IS NULL
                    AND entry.stopped_at IS NOT NULL
                    AND entry.stopped_at < datetime('now', '-1 day')
                )
              ORDER BY entry.started_at IS NULL, entry.started_at DESC, entry.toggl_workspace_id, entry.toggl_entry_id
              LIMIT ?1",
            (limit as i64,),
        )?;
        let rows = rows
            .iter()
            .map(status_entry_from_row)
            .collect::<DbResult<Vec<_>>>()?;

        Ok(rows)
    }

    pub fn list_retryable_error_toggl_entries(
        &self,
        toggl_workspace_id: &str,
    ) -> DbResult<Vec<StoredRetryableTogglEntry>> {
        let rows = self.query_all(
            "WITH latest_attempt AS (
                SELECT attempt.*
                FROM sync_attempts attempt
                INNER JOIN (
                    SELECT toggl_workspace_id, toggl_entry_id, MAX(id) AS id
                    FROM sync_attempts
                    GROUP BY toggl_workspace_id, toggl_entry_id
                ) latest ON latest.id = attempt.id
             )
             SELECT
                entry.toggl_workspace_id,
                entry.toggl_entry_id,
                entry.description,
                entry.rounded_duration_seconds,
                entry.started_at,
                entry.stopped_at
               FROM toggl_entries entry
               LEFT JOIN latest_attempt
                 ON latest_attempt.toggl_workspace_id = entry.toggl_workspace_id
                AND latest_attempt.toggl_entry_id = entry.toggl_entry_id
              WHERE entry.toggl_workspace_id = ?1
                AND entry.deleted_at IS NULL
                AND entry.started_at IS NOT NULL
                AND (entry.status = 'error' OR latest_attempt.status = 'error')
                AND NOT EXISTS (
                    SELECT 1
                      FROM jira_worklog_links link
                     WHERE link.toggl_workspace_id = entry.toggl_workspace_id
                       AND link.toggl_entry_id = entry.toggl_entry_id
                       AND link.deleted_at IS NULL
                       AND link.jira_worklog_id IS NOT NULL
                )
              ORDER BY entry.started_at DESC, entry.toggl_entry_id",
            (toggl_workspace_id,),
        )?;
        let rows = rows
            .iter()
            .map(retryable_toggl_entry_from_row)
            .collect::<DbResult<Vec<_>>>()?;

        Ok(rows)
    }

    pub fn list_pending_local_toggl_entries(
        &self,
        toggl_workspace_id: &str,
    ) -> DbResult<Vec<StoredPendingLocalTogglEntry>> {
        let rows = self.query_all(
            "WITH latest_attempt AS (
                SELECT attempt.*
                FROM sync_attempts attempt
                INNER JOIN (
                    SELECT toggl_workspace_id, toggl_entry_id, MAX(id) AS id
                    FROM sync_attempts
                    GROUP BY toggl_workspace_id, toggl_entry_id
                ) latest ON latest.id = attempt.id
             )
             SELECT
                entry.toggl_workspace_id,
                entry.toggl_entry_id,
                entry.description,
                entry.rounded_duration_seconds,
                entry.started_at,
                entry.stopped_at
               FROM toggl_entries entry
               LEFT JOIN latest_attempt
                 ON latest_attempt.toggl_workspace_id = entry.toggl_workspace_id
                AND latest_attempt.toggl_entry_id = entry.toggl_entry_id
              WHERE entry.toggl_workspace_id = ?1
                AND entry.deleted_at IS NULL
                AND entry.started_at IS NOT NULL
                AND entry.stopped_at IS NOT NULL
                AND entry.rounded_duration_seconds > 0
                AND entry.extracted_issue_key IS NOT NULL
                AND (
                    entry.status = 'planned'
                    OR latest_attempt.status IN ('planned', 'error')
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM jira_worklog_links link
                     WHERE link.toggl_workspace_id = entry.toggl_workspace_id
                       AND link.toggl_entry_id = entry.toggl_entry_id
                       AND link.deleted_at IS NULL
                       AND link.jira_worklog_id IS NOT NULL
                )
              ORDER BY entry.started_at DESC, entry.toggl_entry_id",
            (toggl_workspace_id,),
        )?;
        let rows = rows
            .iter()
            .map(pending_toggl_entry_from_row)
            .collect::<DbResult<Vec<_>>>()?;

        Ok(rows)
    }

    pub fn list_active_linked_local_toggl_entries_since(
        &self,
        toggl_workspace_id: &str,
        started_at_since: &str,
    ) -> DbResult<Vec<StoredLinkedLocalTogglEntry>> {
        let rows = self.query_all(
            "SELECT
                entry.toggl_workspace_id,
                entry.toggl_entry_id,
                entry.description,
                entry.rounded_duration_seconds,
                entry.started_at,
                entry.stopped_at
               FROM toggl_entries entry
              WHERE entry.toggl_workspace_id = ?1
                AND entry.deleted_at IS NULL
                AND entry.started_at IS NOT NULL
                AND entry.started_at >= ?2
                AND EXISTS (
                    SELECT 1
                      FROM jira_worklog_links link
                     WHERE link.toggl_workspace_id = entry.toggl_workspace_id
                       AND link.toggl_entry_id = entry.toggl_entry_id
                       AND link.deleted_at IS NULL
                       AND link.jira_worklog_id IS NOT NULL
                )
              ORDER BY entry.started_at ASC, entry.toggl_entry_id",
            params![toggl_workspace_id, started_at_since],
        )?;
        let rows = rows
            .iter()
            .map(linked_toggl_entry_from_row)
            .collect::<DbResult<Vec<_>>>()?;

        Ok(rows)
    }

    pub fn count_toggl_entries(&self, toggl_workspace_id: &str) -> DbResult<usize> {
        let count = self
            .query_one(
                "SELECT COUNT(*)
               FROM toggl_entries
              WHERE toggl_workspace_id = ?1",
                (toggl_workspace_id,),
            )?
            .map(|row| row_i64(&row, 0))
            .transpose()?
            .unwrap_or(0);
        Ok(count as usize)
    }

    pub fn list_open_zero_duration_toggl_entries(
        &self,
        toggl_workspace_id: &str,
    ) -> DbResult<Vec<StoredOpenTogglEntry>> {
        let rows = self.query_all(
            "SELECT toggl_workspace_id, toggl_entry_id, started_at
               FROM toggl_entries
              WHERE toggl_workspace_id = ?1
                AND deleted_at IS NULL
                AND stopped_at IS NULL
                AND rounded_duration_seconds = 0
                AND started_at IS NOT NULL
              ORDER BY started_at ASC, toggl_entry_id",
            (toggl_workspace_id,),
        )?;
        let rows = rows
            .iter()
            .map(open_toggl_entry_from_row)
            .collect::<DbResult<Vec<_>>>()?;

        Ok(rows)
    }

    pub fn mark_missing_toggl_entries_deleted(
        &self,
        toggl_workspace_id: &str,
        started_at_since: &str,
        seen_entry_ids: &[String],
    ) -> DbResult<usize> {
        let placeholders = std::iter::repeat_n("?", seen_entry_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let not_seen_clause = if seen_entry_ids.is_empty() {
            String::new()
        } else {
            format!(" AND toggl_entry_id NOT IN ({placeholders})")
        };
        let sql = format!(
            "UPDATE toggl_entries
             SET status = 'deleted',
                 deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE toggl_workspace_id = ?
               AND deleted_at IS NULL
               AND started_at >= ?{not_seen_clause}"
        );
        let params = std::iter::once(toggl_workspace_id.to_owned())
            .chain(std::iter::once(started_at_since.to_owned()))
            .chain(seen_entry_ids.iter().cloned())
            .collect::<Vec<_>>();
        self.execute(&sql, params_from_iter(params))
            .map(|updated| updated as usize)
    }

    pub fn mark_toggl_entry_deleted(
        &self,
        toggl_workspace_id: &str,
        toggl_entry_id: &str,
    ) -> DbResult<()> {
        self.execute(
            "UPDATE toggl_entries
             SET status = 'deleted',
                 deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE toggl_workspace_id = ?1
               AND toggl_entry_id = ?2",
            params![toggl_workspace_id, toggl_entry_id],
        )?;
        Ok(())
    }

    pub fn mark_jira_worklog_link_deleted(
        &self,
        toggl_workspace_id: &str,
        toggl_entry_id: &str,
        jira_site_key: &str,
    ) -> DbResult<()> {
        self.execute(
            "UPDATE jira_worklog_links
             SET status = 'deleted',
                 deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                 last_synced_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE toggl_workspace_id = ?1
               AND toggl_entry_id = ?2
               AND jira_site_key = ?3",
            params![toggl_workspace_id, toggl_entry_id, jira_site_key],
        )?;
        Ok(())
    }

    pub fn insert_sync_run(&self, run: &NewSyncRun<'_>) -> DbResult<i64> {
        self.execute(
            "INSERT INTO sync_runs (run_id, mode, status) VALUES (?1, ?2, ?3)",
            params![run.run_id, run.mode, run.status],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn finish_sync_run(
        &self,
        sync_run_id: i64,
        status: &str,
        error_message: Option<&str>,
    ) -> DbResult<()> {
        self.execute(
            "UPDATE sync_runs
             SET status = ?2,
                 finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                 error_message = ?3,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
            params![sync_run_id, status, error_message],
        )?;
        Ok(())
    }

    pub fn upsert_sync_cursor(&self, cursor: &NewSyncCursor<'_>) -> DbResult<()> {
        self.execute(
            "INSERT INTO sync_cursors (
                toggl_workspace_id,
                last_toggl_since,
                last_successful_sync_at
            ) VALUES (?1, ?2, ?3)
            ON CONFLICT(toggl_workspace_id) DO UPDATE SET
                last_toggl_since = excluded.last_toggl_since,
                last_successful_sync_at = excluded.last_successful_sync_at,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            params![
                cursor.toggl_workspace_id,
                cursor.last_toggl_since,
                cursor.last_successful_sync_at,
            ],
        )?;
        Ok(())
    }

    pub fn upsert_toggl_entry(&self, entry: &NewTogglEntry<'_>) -> DbResult<()> {
        self.execute(
            "INSERT INTO toggl_entries (
                toggl_workspace_id,
                toggl_entry_id,
                description,
                extracted_issue_key,
                source_hash,
                rounded_duration_seconds,
                status,
                started_at,
                stopped_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(toggl_workspace_id, toggl_entry_id) DO UPDATE SET
                description = excluded.description,
                extracted_issue_key = excluded.extracted_issue_key,
                source_hash = excluded.source_hash,
                rounded_duration_seconds = excluded.rounded_duration_seconds,
                status = excluded.status,
                started_at = excluded.started_at,
                stopped_at = excluded.stopped_at,
                last_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            params![
                entry.toggl_workspace_id,
                entry.toggl_entry_id,
                entry.description,
                entry.extracted_issue_key,
                entry.source_hash,
                entry.rounded_duration_seconds,
                entry.status,
                entry.started_at,
                entry.stopped_at,
            ],
        )?;
        Ok(())
    }

    pub fn insert_sync_attempt(&self, attempt: &NewSyncAttempt<'_>) -> DbResult<i64> {
        self.execute(
            "INSERT INTO sync_attempts (
                sync_run_id,
                toggl_workspace_id,
                toggl_entry_id,
                jira_site_key,
                jira_issue_key,
                jira_worklog_id,
                status,
                error_message
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                attempt.sync_run_id,
                attempt.toggl_workspace_id,
                attempt.toggl_entry_id,
                attempt.jira_site_key,
                attempt.jira_issue_key,
                attempt.jira_worklog_id,
                attempt.status,
                attempt.error_message,
            ],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn insert_recovery_finding(&self, finding: &NewRecoveryFinding<'_>) -> DbResult<i64> {
        self.execute(
            "INSERT INTO recovery_findings (
                toggl_workspace_id,
                toggl_entry_id,
                jira_site_key,
                jira_issue_key,
                jira_worklog_id,
                source_hash,
                status,
                marker_source
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(toggl_workspace_id, toggl_entry_id, jira_site_key, jira_worklog_id)
            DO UPDATE SET
                jira_issue_key = excluded.jira_issue_key,
                source_hash = excluded.source_hash,
                status = excluded.status,
                marker_source = excluded.marker_source,
                found_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            params![
                finding.toggl_workspace_id,
                finding.toggl_entry_id,
                finding.jira_site_key,
                finding.jira_issue_key,
                finding.jira_worklog_id,
                finding.source_hash,
                finding.status,
                finding.marker_source,
            ],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn count_rows(&self, table_name: &str) -> DbResult<i64> {
        const ALLOWED_TABLES: &[&str] = &[
            "schema_migrations",
            "sync_runs",
            "sync_cursors",
            "toggl_entries",
            "jira_worklog_links",
            "jira_issue_site_cache",
            "sync_attempts",
            "recovery_findings",
            "locks",
        ];

        if !ALLOWED_TABLES.contains(&table_name) {
            return Err(DbError::UnknownTable(table_name.to_owned()));
        }

        let sql = format!("SELECT COUNT(*) FROM {table_name}");
        self.query_one(&sql, ())?
            .map(|row| row_i64(&row, 0))
            .transpose()?
            .ok_or_else(|| DbError::Turso(turso::Error::QueryReturnedNoRows))
    }

    pub fn acquire_sync_lock(&self, owner: &str) -> DbResult<SyncRunLock<'_>> {
        self.acquire_sync_lock_with_timeout(owner, DEFAULT_STALE_LOCK_TIMEOUT)
    }

    pub fn acquire_sync_lock_with_timeout(
        &self,
        owner: &str,
        stale_timeout: Duration,
    ) -> DbResult<SyncRunLock<'_>> {
        let expires_after_seconds = stale_timeout.as_secs().max(1) as i64;
        let token = new_lock_token(owner);

        self.execute(
            "DELETE FROM locks
             WHERE name = ?1 AND expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            (SYNC_LOCK_NAME,),
        )?;

        let inserted = self.execute(
            "INSERT OR IGNORE INTO locks (
                name,
                owner,
                token,
                acquired_at,
                heartbeat_at,
                expires_at,
                updated_at
            ) VALUES (
                ?1,
                ?2,
                ?3,
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+' || ?4 || ' seconds'),
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            )",
            params![SYNC_LOCK_NAME, owner, token.as_str(), expires_after_seconds],
        )?;

        if inserted == 1 {
            return Ok(SyncRunLock {
                database: self,
                token,
                released: false,
            });
        }

        let existing = self.query_one(
            "SELECT owner, expires_at FROM locks WHERE name = ?1",
            (SYNC_LOCK_NAME,),
        );

        match existing {
            Ok(Some(row)) => Err(DbError::SyncAlreadyRunning {
                lock_name: SYNC_LOCK_NAME,
                owner: Some(row_string(&row, 0)?),
                expires_at: Some(row_string(&row, 1)?),
            }),
            Ok(None) => self.acquire_sync_lock_with_timeout(owner, stale_timeout),
            Err(error) => Err(error),
        }
    }

    fn release_sync_lock(&self, token: &str) -> DbResult<()> {
        self.execute(
            "DELETE FROM locks WHERE name = ?1 AND token = ?2",
            params![SYNC_LOCK_NAME, token],
        )?;
        Ok(())
    }

    fn execute(&self, sql: &str, params: impl IntoParams) -> DbResult<u64> {
        block_on(self.connection.execute(sql, params)).map_err(DbError::Turso)
    }

    fn query_one(&self, sql: &str, params: impl IntoParams) -> DbResult<Option<Row>> {
        block_on(async {
            let mut rows = self.connection.query(sql, params).await?;
            rows.next().await
        })
        .map_err(DbError::Turso)
    }

    fn query_all(&self, sql: &str, params: impl IntoParams) -> DbResult<Vec<Row>> {
        block_on(async {
            let mut rows = self.connection.query(sql, params).await?;
            let mut collected = Vec::new();
            while let Some(row) = rows.next().await? {
                collected.push(row);
            }
            Ok(collected)
        })
        .map_err(DbError::Turso)
    }
}

impl SyncRunLock<'_> {
    pub fn release(mut self) -> DbResult<()> {
        self.database.release_sync_lock(&self.token)?;
        self.released = true;
        Ok(())
    }
}

impl Drop for SyncRunLock<'_> {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.database.release_sync_lock(&self.token);
        }
    }
}

/// Does this open failure mean the `-tshm` WAL index outlived the WAL it describes?
///
/// Only that one failure may trigger recovery. `Corrupt`, `NotAdb`, permission errors, and a
/// transient `Busy` from another process must never reach the sidecar-removing path.
fn is_stale_wal_index_error(error: &DbError) -> bool {
    let DbError::Turso(error) = error else {
        return false;
    };
    let message = error.to_string();
    message.contains("short read on WAL frame")
}

/// Move a stale turso WAL index aside so the next open rebuilds it, returning its new path.
///
/// The `-tshm` index outlives the `-wal` file, so another SQLite tool that checkpoints and
/// truncates the WAL leaves the index pointing at frames that no longer exist, and every
/// later open fails. Those frames are already gone by the time we get here — whatever they
/// held is lost either way — so this trades unrecoverable transactions for a usable DB, and
/// the caller warns about it. The index is renamed rather than deleted so the previous state
/// can still be inspected afterwards.
fn quarantine_stale_wal_index(path: &str) -> Option<String> {
    if path == ":memory:" {
        return None;
    }
    // A non-empty WAL may still hold readable frames; leave it and the index alone.
    let wal_bytes = fs::metadata(format!("{path}-wal"))
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if wal_bytes != 0 {
        return None;
    }
    let index = format!("{path}-tshm");
    if !Path::new(&index).exists() {
        return None;
    }
    let quarantined = format!("{index}.stale");
    fs::rename(&index, &quarantined).ok()?;
    let _ = fs::remove_file(format!("{path}-shm"));
    let _ = fs::remove_file(format!("{path}-wal"));
    Some(quarantined)
}

fn new_lock_token(owner: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}:{}:{nanos}", std::process::id(), owner)
}

fn is_constraint_violation(error: &DbError) -> bool {
    matches!(error, DbError::Turso(turso::Error::Constraint(_)))
}

fn row_string(row: &Row, idx: usize) -> DbResult<String> {
    row.get(idx).map_err(DbError::Turso)
}

fn row_optional_string(row: &Row, idx: usize) -> DbResult<Option<String>> {
    match row.get_value(idx).map_err(DbError::Turso)? {
        Value::Null => Ok(None),
        Value::Text(value) => Ok(Some(value)),
        value => Err(DbError::Turso(turso::Error::ConversionFailure(format!(
            "expected nullable text at column {idx}, got {value:?}"
        )))),
    }
}

fn row_i64(row: &Row, idx: usize) -> DbResult<i64> {
    row.get(idx).map_err(DbError::Turso)
}

fn row_bool(row: &Row, idx: usize) -> DbResult<bool> {
    row_i64(row, idx).map(|value| value != 0)
}

fn jira_worklog_link_from_row(row: &Row) -> DbResult<StoredJiraWorklogLink> {
    Ok(StoredJiraWorklogLink {
        toggl_workspace_id: row_string(row, 0)?,
        toggl_entry_id: row_string(row, 1)?,
        jira_site_key: row_string(row, 2)?,
        jira_issue_key: row_string(row, 3)?,
        jira_worklog_id: row_optional_string(row, 4)?,
        source_hash: row_string(row, 5)?,
        rounded_duration_seconds: row_i64(row, 6)?,
        status: row_string(row, 7)?,
    })
}

fn jira_issue_site_cache_from_row(row: &Row) -> DbResult<StoredJiraIssueSiteCache> {
    Ok(StoredJiraIssueSiteCache {
        issue_key: row_string(row, 0)?,
        jira_site_key: row_string(row, 1)?,
        discovered_at: row_string(row, 2)?,
        confirmed_at: row_string(row, 3)?,
        source: row_string(row, 4)?,
    })
}

fn status_entry_from_row(row: &Row) -> DbResult<StoredStatusEntry> {
    Ok(StoredStatusEntry {
        workspace: row_string(row, 0)?,
        entry: row_string(row, 1)?,
        started_at: row_optional_string(row, 2)?,
        stopped_at: row_optional_string(row, 3)?,
        rounded_duration_seconds: row_i64(row, 4)?,
        issue_key: row_optional_string(row, 5)?,
        site: row_optional_string(row, 6)?,
        worklog_id: row_optional_string(row, 7)?,
        status: row_string(row, 8)?,
        reason: row_optional_string(row, 9)?,
    })
}

fn retryable_toggl_entry_from_row(row: &Row) -> DbResult<StoredRetryableTogglEntry> {
    Ok(StoredRetryableTogglEntry {
        workspace: row_string(row, 0)?,
        entry: row_string(row, 1)?,
        description: row_optional_string(row, 2)?,
        rounded_duration_seconds: row_i64(row, 3)?,
        started_at: row_string(row, 4)?,
        stopped_at: row_optional_string(row, 5)?,
    })
}

fn pending_toggl_entry_from_row(row: &Row) -> DbResult<StoredPendingLocalTogglEntry> {
    Ok(StoredPendingLocalTogglEntry {
        workspace: row_string(row, 0)?,
        entry: row_string(row, 1)?,
        description: row_optional_string(row, 2)?,
        rounded_duration_seconds: row_i64(row, 3)?,
        started_at: row_string(row, 4)?,
        stopped_at: row_optional_string(row, 5)?,
    })
}

fn linked_toggl_entry_from_row(row: &Row) -> DbResult<StoredLinkedLocalTogglEntry> {
    Ok(StoredLinkedLocalTogglEntry {
        workspace: row_string(row, 0)?,
        entry: row_string(row, 1)?,
        description: row_optional_string(row, 2)?,
        rounded_duration_seconds: row_i64(row, 3)?,
        started_at: row_string(row, 4)?,
        stopped_at: row_optional_string(row, 5)?,
    })
}

fn open_toggl_entry_from_row(row: &Row) -> DbResult<StoredOpenTogglEntry> {
    Ok(StoredOpenTogglEntry {
        workspace: row_string(row, 0)?,
        entry: row_string(row, 1)?,
        started_at: row_string(row, 2)?,
    })
}
