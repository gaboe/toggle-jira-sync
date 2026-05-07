# Changelog

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
