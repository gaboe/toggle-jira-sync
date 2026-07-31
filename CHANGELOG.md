# Changelog

## 0.1.25

- Recover from a stale turso WAL index instead of failing every open with `short read on WAL frame` after another SQLite tool checkpointed the WAL away.

## 0.1.24

- Back the local sync ledger with the Turso driver and clean up its `-wal`, `-shm`, `-tshm`, and `-journal` sidecars on local data deletion.
- Reinstall a missing OS scheduler job on startup so `schedule.enabled = true` can no longer drift into nothing being scheduled.
- Reconcile the OS scheduler job whenever the default config is saved.

## 0.1.23

- Ignore section numbers when extracting Jira issue keys from Toggl descriptions.

## 0.1.22

- Make dashboard metrics follow the selected calendar month and default to the current month on app start.

## 0.1.21

- Fix Windows desktop release builds by importing the cross-platform log writer trait.

## 0.1.20

- Add duplicate Jira worklog detection and explicit recovery repair for deterministic Toggl/Jira duplicates.
- Add recovery UI controls and API support for repairing duplicate worklogs.
- Fix Toggl deleted-entry handling by honoring `server_deleted_at` during cleanup.
- Keep normal sync conservative while continuing to adopt matching external worklogs.

## 0.1.17

- Add unsigned GitHub Release workflow for crates.io CLI publishing and Tauri desktop bundles.
- Add desktop app release setup documentation.
- Use Bun for the desktop frontend package workflow.
- Use crates.io trusted publishing for release automation.
- Add the Windows desktop bundle icon.
- Publish GitHub Releases publicly instead of as drafts.
- Use clearer GitHub Release names, asset base names, and install instructions.
- Verify public release notes with the corrected crates.io version URL.

## 0.1.6

- Show an animated TUI progress message while sync, dry-run, or hourly sync runs in the background.

## 0.1.5

- Start default initial sync at the first day of the current month so older completed Toggl entries are not pulled into a new install.

## 0.1.4

- Cap Toggl initial backfill to a safe API window so sync does not fail with `Since cannot be older than 3 months`.
- Show full sync error chains in the TUI footer.

## 0.1.3

- Improve Jira issue-key error messages and self-recovery in local status state.
- Fix sync reliability issues from review: real marker timestamps, HTTP timeouts, Jira rate-limit retry, background TUI sync, configured backfill window, safer recovery handling, and transactional migrations.

## 0.1.2

- Improve README TUI documentation with a text preview.
- Use a generic Jira issue key example.
- Remove agent-facing safety notes from the README.

## 0.1.1

- Update installation docs now that the crate is published on crates.io.

## 0.1.0

- Initial local Toggl to Jira sync CLI.
- SQLite-backed sync ledger and issue-site cache.
- Interactive config setup with local credentials file.
- Dynamic Jira issue-site discovery with cache reuse.
- Dry-run, sync, status, recover, doctor, and Ratatui TUI commands.
- Default `tjs` launch opens the TUI.
