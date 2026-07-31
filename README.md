# Toggl Jira Sync

Local CLI for syncing Toggl time entries into Jira worklogs. It keeps sync state in a local Turso database file, so repeated runs can skip already-synced entries and recover from interrupted runs.

This is an independent community tool. It is not an official Toggl, Jira, or Atlassian product, and it is not endorsed by those companies.

## Setup

## Installation

From crates.io:

```sh
cargo install toggl-jira-sync
```

From GitHub:

```sh
cargo install --git https://github.com/gaboe/toggle-jira-sync
```

Update an existing Cargo install:

```sh
cargo install toggl-jira-sync --force
```

For local development from this repository:

```sh
cargo run -- config setup
```

GitHub install works directly from the repository and is useful for testing unreleased changes. The crates.io install is the recommended stable path.

### 1. Build or run the CLI

From this repository:

```sh
cargo run -- config setup
```

If you use the local alias:

```sh
tjs config setup
```

### 2. Enter setup values

`config setup` creates:

- config: `~/.config/toggl-jira-sync/config.toml`
- credentials: `~/.config/toggl-jira-sync/credentials.env`
- local database path in config: `toggl-jira-sync.sqlite`
- OS scheduler job for hourly sync, when scheduler installation is supported by the current OS

The setup prompt asks only for values that cannot be derived:

```text
Toggl API token:
Toggl workspace id fallback, only if workspace discovery is skipped or fails
Jira site URL:
Jira email:
Jira API token:
```

Notes:

- In an interactive terminal, the Toggl API token is used to discover workspaces automatically.
- If one Toggl workspace is found, it is selected automatically.
- If multiple workspaces are found, the CLI shows a numbered list.
- `Jira site URL` is enough; the internal site key is derived from the URL. Example: `https://sabservis.atlassian.net` becomes `sabservis`.
- No Jira issue prefixes are configured. Jira site selection is resolved dynamically per issue and cached in SQLite.
- The local database file is created automatically on first command that opens the DB. Existing SQLite state files are opened in place by the Turso-backed runtime.
- Turso local state should be opened by one app process at a time. Close the TUI/server before running another command against the same DB path.
- Setup installs an hourly OS scheduler job by default. Use `tjs schedule status`, `tjs schedule set --disabled`, or `tjs schedule uninstall` to inspect or disable it.

### 3. Check the generated config

```sh
tjs config show
```

Secrets are redacted by default. To inspect local secret values explicitly:

```sh
tjs config show --show-secrets
```

### 4. Validate config

```sh
tjs config validate --config ~/.config/toggl-jira-sync/config.toml
```

## Daily usage

Launch the TUI:

```sh
tjs
```

You can also open it explicitly:

```sh
tjs tui
```

The TUI shows recent local sync state from SQLite and lets you run common actions without leaving the terminal:

- search and filter entries
- open matching Jira or Toggl links
- press `d` to run a dry-run sync
- press `s` to run a real sync
- press `a` to toggle the hourly OS scheduler

Local day-to-day commands use the same backend HTTP server core as the GUI. `tjs`, `tjs tui`, `tjs status`, `tjs sync`, and `tjs schedule ...` start an embedded loopback server automatically for the command lifetime; you do not need to run `tjs server` separately for local use.

It is the main day-to-day view:

```text
Toggl -> Jira Sync TUI  normal
Issue search: - | Date/time filter: - | rows=3/3 | OS schedule: on every 60m

date        start end   duration   issue    site     worklog status  reason
2026-05-04 15:59 17:43 1h 44m     PROJ-123 acme     26410   synced  -
2026-05-04 18:00 18:30 30m        PROJ-456 acme     -       skipped running entry
2026-05-05 09:15 10:00 45m        PROJ-789 acme     -       error   issue not found

Issue: PROJ-123 | Worklog: 26410 | Reason: -
Issue URL: https://acme.atlassian.net/browse/PROJ-123
Worklog URL: https://acme.atlassian.net/browse/PROJ-123?focusedWorklogId=26410
```

While the TUI is open, it also runs an hourly sync through the embedded local server. The OS scheduler is separate and keeps hourly sync running when the TUI is not open.

Run a safe preview first:

```sh
tjs sync --dry-run
```

Run the real sync:

```sh
tjs sync
```

Inspect local sync state:

```sh
tjs status
```

Recover after an interrupted or uncertain sync:

```sh
tjs recover
```

Manage the hourly OS scheduler:

```sh
tjs schedule status
tjs schedule set --interval-minutes 60 --enabled
tjs schedule set --disabled
tjs schedule uninstall
```

## How Jira site resolution works

For each Toggl entry, the CLI extracts a Jira issue key from the description, for example `PROJ-123`.

Resolution flow:

1. Check SQLite cache for `issue_key -> jira_site_key`.
2. If cached, use it without Jira discovery.
3. If not cached, query every enabled Jira site with `GET /rest/api/3/issue/{issueKey}`.
4. If exactly one site has the issue, save that mapping to SQLite.
5. If zero or multiple sites match, report an error instead of creating a worklog on the wrong site.

This means setup does not need issue prefixes.

## Files created locally

```text
~/.config/toggl-jira-sync/config.toml
~/.config/toggl-jira-sync/credentials.env
toggl-jira-sync.sqlite
```

`credentials.env` stores raw local credentials and is written with user-only permissions on Unix/macOS.
