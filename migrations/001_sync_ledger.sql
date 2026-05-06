CREATE TABLE IF NOT EXISTS sync_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL UNIQUE,
    mode TEXT NOT NULL CHECK (mode IN ('sync', 'dry_run', 'recover', 'doctor')),
    status TEXT NOT NULL CHECK (status IN ('planned', 'running', 'completed', 'error')),
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    finished_at TEXT,
    error_message TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS sync_cursors (
    toggl_workspace_id TEXT PRIMARY KEY NOT NULL,
    last_toggl_since INTEGER,
    last_successful_sync_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS toggl_entries (
    toggl_workspace_id TEXT NOT NULL,
    toggl_entry_id TEXT NOT NULL,
    description TEXT,
    extracted_issue_key TEXT,
    source_hash TEXT NOT NULL,
    rounded_duration_seconds INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('planned', 'created', 'updated', 'deleted', 'error', 'orphaned')),
    started_at TEXT,
    stopped_at TEXT,
    deleted_at TEXT,
    last_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_synced_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (toggl_workspace_id, toggl_entry_id)
);

CREATE TABLE IF NOT EXISTS jira_worklog_links (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    toggl_workspace_id TEXT NOT NULL,
    toggl_entry_id TEXT NOT NULL,
    jira_site_key TEXT NOT NULL,
    jira_issue_key TEXT NOT NULL,
    jira_worklog_id TEXT,
    source_hash TEXT NOT NULL,
    rounded_duration_seconds INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('planned', 'created', 'updated', 'deleted', 'error', 'orphaned')),
    deleted_at TEXT,
    last_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_synced_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (toggl_workspace_id, toggl_entry_id, jira_site_key)
);

CREATE INDEX IF NOT EXISTS idx_jira_worklog_links_worklog
    ON jira_worklog_links (jira_site_key, jira_worklog_id)
    WHERE jira_worklog_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS sync_attempts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sync_run_id INTEGER,
    toggl_workspace_id TEXT NOT NULL,
    toggl_entry_id TEXT NOT NULL,
    jira_site_key TEXT,
    jira_issue_key TEXT,
    jira_worklog_id TEXT,
    status TEXT NOT NULL CHECK (status IN ('planned', 'created', 'updated', 'deleted', 'error', 'orphaned')),
    error_message TEXT,
    attempted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (sync_run_id) REFERENCES sync_runs(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS recovery_findings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    toggl_workspace_id TEXT NOT NULL,
    toggl_entry_id TEXT NOT NULL,
    jira_site_key TEXT NOT NULL,
    jira_issue_key TEXT NOT NULL,
    jira_worklog_id TEXT NOT NULL,
    source_hash TEXT,
    status TEXT NOT NULL CHECK (status IN ('planned', 'created', 'updated', 'deleted', 'error', 'orphaned')),
    marker_source TEXT NOT NULL CHECK (marker_source IN ('property', 'comment_fallback')),
    found_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    reconciled_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (toggl_workspace_id, toggl_entry_id, jira_site_key, jira_worklog_id)
);

CREATE TABLE IF NOT EXISTS locks (
    name TEXT PRIMARY KEY NOT NULL,
    owner TEXT NOT NULL,
    token TEXT NOT NULL,
    acquired_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    heartbeat_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
