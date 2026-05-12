use std::fmt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use rusqlite::{params, params_from_iter, Connection, ErrorCode, OptionalExtension};

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../../migrations/001_sync_ledger.sql")),
    (
        2,
        include_str!("../../migrations/002_jira_issue_site_cache.sql"),
    ),
];
const DEFAULT_STALE_LOCK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const SYNC_LOCK_NAME: &str = "sync";

#[derive(Debug)]
pub enum DbError {
    Sqlite(rusqlite::Error),
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
}

impl fmt::Display for DbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "sqlite error: {error}"),
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
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::DuplicateJiraWorklogLink { .. }
            | Self::SyncAlreadyRunning { .. }
            | Self::UnknownTable(_) => None,
        }
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
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
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { connection })
    }

    pub fn run_migrations(&self) -> DbResult<()> {
        self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );",
        )?;

        for (version, sql) in MIGRATIONS {
            let already_applied: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = ?1)",
                [version],
                |row| row.get(0),
            )?;

            if !already_applied {
                self.connection.execute_batch(&format!(
                    "BEGIN IMMEDIATE;\n{sql}\nINSERT OR IGNORE INTO schema_migrations (version) VALUES ({version});\nCOMMIT;"
                ))?;
            }
        }

        Ok(())
    }

    pub fn table_names(&self) -> DbResult<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )?;
        let tables = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(tables)
    }

    pub fn insert_jira_worklog_link(&self, link: &NewJiraWorklogLink<'_>) -> DbResult<()> {
        let result = self.connection.execute(
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
            Err(error) => Err(DbError::Sqlite(error)),
        }
    }

    pub fn upsert_jira_worklog_link(&self, link: &NewJiraWorklogLink<'_>) -> DbResult<()> {
        self.connection.execute(
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
        self.connection
            .query_row(
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
                |row| {
                    Ok(StoredJiraWorklogLink {
                        toggl_workspace_id: row.get(0)?,
                        toggl_entry_id: row.get(1)?,
                        jira_site_key: row.get(2)?,
                        jira_issue_key: row.get(3)?,
                        jira_worklog_id: row.get(4)?,
                        source_hash: row.get(5)?,
                        rounded_duration_seconds: row.get(6)?,
                        status: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(DbError::Sqlite)
    }

    pub fn list_active_jira_worklog_links(&self) -> DbResult<Vec<StoredJiraWorklogLink>> {
        let mut statement = self.connection.prepare(
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
        )?;

        let links = statement
            .query_map([], |row| {
                Ok(StoredJiraWorklogLink {
                    toggl_workspace_id: row.get(0)?,
                    toggl_entry_id: row.get(1)?,
                    jira_site_key: row.get(2)?,
                    jira_issue_key: row.get(3)?,
                    jira_worklog_id: row.get(4)?,
                    source_hash: row.get(5)?,
                    rounded_duration_seconds: row.get(6)?,
                    status: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(links)
    }

    pub fn get_jira_issue_site_cache(
        &self,
        issue_key: &str,
    ) -> DbResult<Option<StoredJiraIssueSiteCache>> {
        self.connection
            .query_row(
                "SELECT issue_key, jira_site_key, discovered_at, confirmed_at, source
                 FROM jira_issue_site_cache
                 WHERE issue_key = ?1",
                [issue_key],
                |row| {
                    Ok(StoredJiraIssueSiteCache {
                        issue_key: row.get(0)?,
                        jira_site_key: row.get(1)?,
                        discovered_at: row.get(2)?,
                        confirmed_at: row.get(3)?,
                        source: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(DbError::Sqlite)
    }

    pub fn upsert_jira_issue_site_cache(&self, cache: &NewJiraIssueSiteCache<'_>) -> DbResult<()> {
        self.connection.execute(
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
        let mut statement = self.connection.prepare(
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
                    WHEN link.jira_worklog_id IS NOT NULL AND link.status IN ('created', 'updated') THEN 'synced'
                    WHEN entry.extracted_issue_key IS NULL THEN 'skipped'
                    WHEN entry.rounded_duration_seconds = 0 THEN 'skipped'
                    ELSE 'not_synced'
                END AS derived_status,
                CASE
                    WHEN latest_attempt.status = 'error' THEN COALESCE(latest_attempt.error_message, 'last sync attempt failed')
                    WHEN entry.status = 'error' THEN 'toggl entry is marked error'
                    WHEN link.status = 'error' THEN 'jira worklog link is marked error'
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
              ORDER BY entry.started_at IS NULL, entry.started_at DESC, entry.toggl_workspace_id, entry.toggl_entry_id
              LIMIT ?1",
        )?;

        let rows = statement
            .query_map([limit as i64], |row| {
                Ok(StoredStatusEntry {
                    workspace: row.get(0)?,
                    entry: row.get(1)?,
                    started_at: row.get(2)?,
                    stopped_at: row.get(3)?,
                    rounded_duration_seconds: row.get(4)?,
                    issue_key: row.get(5)?,
                    site: row.get(6)?,
                    worklog_id: row.get(7)?,
                    status: row.get(8)?,
                    reason: row.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

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
        Ok(self.connection.execute(&sql, params_from_iter(params))?)
    }

    pub fn mark_jira_worklog_link_deleted(
        &self,
        toggl_workspace_id: &str,
        toggl_entry_id: &str,
        jira_site_key: &str,
    ) -> DbResult<()> {
        self.connection.execute(
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
        self.connection.execute(
            "INSERT INTO sync_runs (run_id, mode, status) VALUES (?1, ?2, ?3)",
            params![run.run_id, run.mode, run.status],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn upsert_sync_cursor(&self, cursor: &NewSyncCursor<'_>) -> DbResult<()> {
        self.connection.execute(
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
        self.connection.execute(
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
        self.connection.execute(
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
        self.connection.execute(
            "INSERT INTO recovery_findings (
                toggl_workspace_id,
                toggl_entry_id,
                jira_site_key,
                jira_issue_key,
                jira_worklog_id,
                source_hash,
                status,
                marker_source
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
        Ok(self.connection.query_row(&sql, [], |row| row.get(0))?)
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

        self.connection.execute(
            "DELETE FROM locks
             WHERE name = ?1 AND expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            [SYNC_LOCK_NAME],
        )?;

        let inserted = self.connection.execute(
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
            params![SYNC_LOCK_NAME, owner, token, expires_after_seconds],
        )?;

        if inserted == 1 {
            return Ok(SyncRunLock {
                database: self,
                token,
                released: false,
            });
        }

        let existing = self.connection.query_row(
            "SELECT owner, expires_at FROM locks WHERE name = ?1",
            [SYNC_LOCK_NAME],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        );

        match existing {
            Ok((owner, expires_at)) => Err(DbError::SyncAlreadyRunning {
                lock_name: SYNC_LOCK_NAME,
                owner: Some(owner),
                expires_at: Some(expires_at),
            }),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                self.acquire_sync_lock_with_timeout(owner, stale_timeout)
            }
            Err(error) => Err(DbError::Sqlite(error)),
        }
    }

    fn release_sync_lock(&self, token: &str) -> DbResult<()> {
        self.connection.execute(
            "DELETE FROM locks WHERE name = ?1 AND token = ?2",
            params![SYNC_LOCK_NAME, token],
        )?;
        Ok(())
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

fn new_lock_token(owner: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}:{}:{nanos}", std::process::id(), owner)
}

fn is_constraint_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == ErrorCode::ConstraintViolation
    )
}
