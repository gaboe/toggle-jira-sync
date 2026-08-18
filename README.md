# Toggl Jira Sync

Toggl Jira Sync is an independent community tool that reads Toggl time entries, finds Jira issue keys in their descriptions, and creates or updates Jira worklogs. Sync state stays in local files so repeated runs are safe and recoverable. It is not an official Toggl, Jira, or Atlassian product.

## Five-minute setup

1. Choose one of the two installation paths below.
2. Configure Toggl and every Jira site that may contain your issue keys.
3. Test and save the credentials.
4. Run a dry-run, review the planned changes, then run the real sync.

## Install: choose one

### CLI/TUI via Cargo

Install the released command from crates.io:

```sh
cargo install toggl-jira-sync
```

The installed command is `toggl-jira-sync`. If you want a shorter local alias, add it yourself:

```sh
alias tjs='toggl-jira-sync'
```

Run setup, then launch the terminal UI with no subcommand:

```sh
toggl-jira-sync config setup
toggl-jira-sync
```

### Desktop app via GitHub Releases

Download the `v0.1.32` release, or the latest release, from [GitHub Releases](https://github.com/gaboe/toggle-jira-sync/releases). Choose the macOS, Windows, or Linux asset for your machine. macOS builds include Apple Silicon and Intel assets; Linux releases include AppImage, Debian/Ubuntu, and Fedora/RHEL packages where available.

The desktop builds are unsigned. Your operating system may show an extra warning before the app can be opened.

On macOS, move the app to `/Applications` before enabling the OS schedule so the saved executable path remains stable. For a Linux AppImage, move it to a stable location such as `~/Applications/` and keep it there before enabling the OS schedule; do not schedule an AppImage launched from a temporary mount.

Open the app and use **Configuration** to enter and test credentials, add Jira sites, and save. The desktop app uses the same local config, credentials, database, scheduler, and sync core as the CLI/TUI.

## Configure Toggl and Jira

For Toggl, provide an API token. Interactive CLI setup discovers the workspace automatically when possible; if discovery is unavailable or finds several workspaces, enter the workspace ID. The desktop Configuration screen has the same workspace and token fields.

Add one or more enabled Jira sites. For each site provide:

- the base URL, such as `https://acme.atlassian.net`;
- the Jira account email; and
- a Jira API token.

Use **Test Toggl credentials** and **Test Jira credentials** before saving. CLI setup writes the same values after its prompts. Credentials are kept in the local `credentials.env` file; config, credentials, and sync state remain on this machine and are not uploaded by this tool.

Put the Jira key in the Toggl description, for example:

```text
CORE-297 implementácia
```

If the description contains one Jira key, sync uses it. If it contains several keys, sync uses one only when exactly one of them resolves to an enabled Jira site; otherwise it reports `MultipleIssueKeys` and does not plan a worklog for that entry. You do not need to configure issue prefixes.

## Dry-run, review, and sync

Run a preview first. It reads Toggl and Jira and shows planned creates, updates, skips, or errors without writing Jira worklogs:

```sh
toggl-jira-sync sync --dry-run
```

Review the dry-run output or the desktop ledger. Then run the real sync:

```sh
toggl-jira-sync sync
```

The TUI has the same **Dry run** and **Sync** actions. The desktop app has **Preview changes** and **Sync** actions. Running sync records local links and statuses so later runs skip unchanged worklogs.

## Background sync when the app is closed

Saving setup attempts to enable the persistent OS job by default at 60 minutes. When installation succeeds, it runs the real sync every 60 minutes even when the desktop window or TUI is closed. The installed job launches the same executable with `sync --cleanup-deleted --config ...`; it does not reopen the desktop GUI. Configuration and credentials remain saved if scheduler reconciliation fails, but the desktop reports that the scheduler change failed so the save is only partial. In particular, a failed disable can leave the previous native job active until removal succeeds; verify with `toggl-jira-sync schedule status` and the native scheduler status commands below, then retry removal.

The platform job is:

- macOS: a `launchd` user agent at `~/Library/LaunchAgents/com.toggl-jira-sync.hourly.plist`;
- Linux: a `systemd --user` timer and service under `~/.config/systemd/user/`; and
- Windows: a Windows Task Scheduler task named `toggl-jira-sync` (with a local helper file under `%APPDATA%`).

The CLI can inspect or change the persistent job:

```sh
toggl-jira-sync schedule status
toggl-jira-sync schedule set --interval-minutes 60 --enabled
toggl-jira-sync schedule set --disabled
toggl-jira-sync schedule uninstall
```

In the desktop app, change **Schedule interval minutes** and **Enable OS schedule** in Configuration, then save. `status` reports the installed job path. You can also inspect the native job with `launchctl`, `systemctl --user`, or `schtasks /Query /TN toggl-jira-sync /V`.

This persistent OS job is separate from **Sync hourly while this app is open** in the desktop app. The latter is only an in-app timer and stops when the UI closes. The TUI’s hourly activity also ends when the TUI closes; only the OS job persists.

For a cautious first run, disable the OS job while reviewing the dry-run, run the real sync when it looks correct, then enable the job again.

## Local files

On macOS and Linux the defaults are:

```text
~/.config/toggl-jira-sync/config.toml
~/.config/toggl-jira-sync/credentials.env
toggl-jira-sync.sqlite
```

On Windows the config and credentials directory is `%APPDATA%\toggl-jira-sync\`. A relative SQLite path is resolved next to the config file. `credentials.env` is written with user-only permissions on Unix/macOS. Do not commit or share these files.

Use explicit paths when you need a separate local profile:

```sh
toggl-jira-sync sync --config /path/to/config.toml --db /path/to/state.sqlite
toggl-jira-sync config show --config /path/to/config.toml
```

## Jira site resolution

For each Toggl entry, the sync extracts Jira issue keys from the description and:

1. uses the cached `issue_key -> site` mapping when one exists;
2. uses the only extracted key directly when there is one;
3. when there are multiple keys, keeps only keys that resolve to an enabled site;
4. proceeds only when exactly one key remains, otherwise reports `multiple issue keys found`; and
5. for the chosen key, caches the site mapping when exactly one enabled site contains it.

If a key is ambiguous, enable only the relevant site or correct the site configuration, then preview again.

## Recovery and diagnostics

Inspect local results and validate configuration with:

```sh
toggl-jira-sync status
toggl-jira-sync config validate --config ~/.config/toggl-jira-sync/config.toml
toggl-jira-sync doctor --online
```

If a run was interrupted or local state is uncertain, reconcile it before syncing again:

```sh
toggl-jira-sync recover
toggl-jira-sync status
```

The sync lock rejects overlapping runs. If the local database was copied or restored while a process was using it, close the app/TUI first and run recovery. Never edit the SQLite file with another tool while a sync is active.

## Help

```sh
toggl-jira-sync --help
toggl-jira-sync sync --help
toggl-jira-sync schedule --help
```
