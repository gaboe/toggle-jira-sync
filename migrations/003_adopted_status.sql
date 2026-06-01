CREATE TABLE jira_worklog_links_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    toggl_workspace_id TEXT NOT NULL,
    toggl_entry_id TEXT NOT NULL,
    jira_site_key TEXT NOT NULL,
    jira_issue_key TEXT NOT NULL,
    jira_worklog_id TEXT,
    source_hash TEXT NOT NULL,
    rounded_duration_seconds INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('planned', 'created', 'updated', 'deleted', 'error', 'orphaned', 'adopted')),
    deleted_at TEXT,
    last_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_synced_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (toggl_workspace_id, toggl_entry_id, jira_site_key)
);

INSERT INTO jira_worklog_links_new (
    id,
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
    created_at,
    updated_at
)
SELECT
    id,
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
    created_at,
    updated_at
FROM jira_worklog_links;

DROP TABLE jira_worklog_links;
ALTER TABLE jira_worklog_links_new RENAME TO jira_worklog_links;

CREATE INDEX IF NOT EXISTS idx_jira_worklog_links_worklog
    ON jira_worklog_links (jira_site_key, jira_worklog_id)
    WHERE jira_worklog_id IS NOT NULL;

CREATE TABLE sync_attempts_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sync_run_id INTEGER,
    toggl_workspace_id TEXT NOT NULL,
    toggl_entry_id TEXT NOT NULL,
    jira_site_key TEXT,
    jira_issue_key TEXT,
    jira_worklog_id TEXT,
    status TEXT NOT NULL CHECK (status IN ('planned', 'created', 'updated', 'deleted', 'error', 'orphaned', 'adopted')),
    error_message TEXT,
    attempted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (sync_run_id) REFERENCES sync_runs(id) ON DELETE SET NULL
);

INSERT INTO sync_attempts_new (
    id,
    sync_run_id,
    toggl_workspace_id,
    toggl_entry_id,
    jira_site_key,
    jira_issue_key,
    jira_worklog_id,
    status,
    error_message,
    attempted_at,
    created_at
)
SELECT
    id,
    sync_run_id,
    toggl_workspace_id,
    toggl_entry_id,
    jira_site_key,
    jira_issue_key,
    jira_worklog_id,
    status,
    error_message,
    attempted_at,
    created_at
FROM sync_attempts;

DROP TABLE sync_attempts;
ALTER TABLE sync_attempts_new RENAME TO sync_attempts;
