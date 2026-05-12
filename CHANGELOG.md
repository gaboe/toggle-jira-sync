# Changelog

## 0.1.15

- Add unsigned GitHub Release workflow for crates.io CLI publishing and Tauri desktop bundles.
- Add desktop app release setup documentation.
- Use Bun for the desktop frontend package workflow.
- Use crates.io trusted publishing for release automation.
- Add the Windows desktop bundle icon.
- Publish GitHub Releases publicly instead of as drafts.

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
