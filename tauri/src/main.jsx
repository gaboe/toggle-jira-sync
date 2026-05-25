import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { createEffect, createMemo, createSignal, For, Match, onCleanup, onMount, Show, Switch } from "solid-js";
import { render } from "solid-js/web";
import toast, { Toaster } from "solid-toast";
import "./styles.css";

const backgroundSyncIntervalMs = 60 * 60 * 1000;
const minimumBusyMs = 650;
const desktopApiBaseUrl = window.__TJS_API_BASE_URL__ || "";
const webApiBaseUrl = import.meta.env.VITE_TJS_API_BASE_URL || "";
const apiBaseUrl = (desktopApiBaseUrl || webApiBaseUrl).replace(/\/$/, "");
const tenantId = import.meta.env.VITE_TJS_TENANT_ID || "";
const tenantToken = import.meta.env.VITE_TJS_TENANT_TOKEN || "";
const tenantApiPrefix = tenantId ? `/api/tenants/${encodeURIComponent(tenantId)}` : "/api";
const isTenantMode = Boolean(tenantId);

async function apiRequest(path, options = {}) {
  if (!apiBaseUrl) {
    return tauriInvoke(options.tauriCommand, options.tauriArgs || {});
  }
  const response = await fetch(`${apiBaseUrl}${path}`, {
    method: options.method || "GET",
    headers: {
      ...(options.body ? { "Content-Type": "application/json" } : {}),
      ...(tenantToken ? { Authorization: `Bearer ${tenantToken}` } : {}),
    },
    body: options.body ? JSON.stringify(options.body) : undefined,
  });
  const payload = await response.json().catch(() => null);
  if (!response.ok) throw new Error(payload?.error || `Request failed with ${response.status}`);
  return payload;
}

const api = {
  snapshot: () => apiRequest(`${tenantApiPrefix}/snapshot`, { tauriCommand: "snapshot" }),
  configSnapshot: () => apiRequest(`${tenantApiPrefix}/config`, { tauriCommand: "config_snapshot" }),
  dryRun: () => apiRequest(`${tenantApiPrefix}/sync/dry-run`, { method: "POST", tauriCommand: "dry_run" }),
  sync: () => apiRequest(`${tenantApiPrefix}/sync`, { method: "POST", tauriCommand: "sync" }),
  saveConfig: (update) => apiRequest("/api/config", { method: "PUT", body: update, tauriCommand: "save_config", tauriArgs: { update } }),
  deleteLocalData: () => apiRequest("/api/local-data", { method: "DELETE", tauriCommand: "delete_local_data" }),
  exportConfig: () => apiRequest("/api/config/export", { method: "POST", tauriCommand: "export_config" }),
};

function splitDateTime(value) {
  if (!value) return ["-", "-"];
  const datetime = new Date(value);
  if (!Number.isNaN(datetime.getTime())) {
    const year = datetime.getFullYear();
    const month = String(datetime.getMonth() + 1).padStart(2, "0");
    const day = String(datetime.getDate()).padStart(2, "0");
    const hours = String(datetime.getHours()).padStart(2, "0");
    const minutes = String(datetime.getMinutes()).padStart(2, "0");
    return [`${year}-${month}-${day}`, `${hours}:${minutes}`];
  }
  const [date, time = ""] = value.split("T");
  return [date || "-", time.slice(0, 5) || "-"];
}

function formatDuration(seconds) {
  const abs = Math.abs(seconds);
  const hours = Math.floor(abs / 3600);
  const minutes = Math.floor((abs % 3600) / 60);
  if (hours > 0 && minutes > 0) return `${hours}h ${minutes}m`;
  if (hours > 0) return `${hours}h`;
  return `${minutes}m`;
}

function formatRunningDuration(startedAt, now) {
  const started = Date.parse(startedAt);
  if (!Number.isFinite(started)) return "running";
  return `${formatDuration(Math.max(0, Math.floor((now - started) / 1000)))} · ticking`;
}

function formatClock(value) {
  return new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" }).format(value);
}

function monthKey(date) {
  return date.slice(0, 7);
}

function monthLabel(key) {
  const [year, month] = key.split("-").map(Number);
  return new Intl.DateTimeFormat(undefined, { month: "long", year: "numeric" }).format(new Date(year, month - 1, 1));
}

function configMonthToInput(value) {
  const match = /^(\d{2})\.(\d{4})$/.exec(value || "");
  if (!match) return "";
  return `${match[2]}-${match[1]}`;
}

function inputMonthToConfig(value) {
  const match = /^(\d{4})-(\d{2})$/.exec(value || "");
  if (!match) return null;
  return `${match[2]}.${match[1]}`;
}

function envPrefixForSiteKey(siteKey) {
  return (siteKey || "")
    .replace(/[^A-Za-z0-9]+/g, "_")
    .replace(/^_+|_+$/g, "")
    .toUpperCase();
}

function deriveJiraSiteKey(baseUrl) {
  const trimmed = (baseUrl || "").trim();
  const withoutScheme = trimmed.replace(/^https?:\/\//, "");
  const host = withoutScheme.split(/[/?#]/)[0]?.replace(/\.+$/, "") || "";
  const tenant = host.endsWith(".atlassian.net") ? host.slice(0, -".atlassian.net".length) : host;
  return tenant
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

function jiraEnvNames(siteKey) {
  const prefix = envPrefixForSiteKey(siteKey);
  return {
    emailEnv: prefix ? `${prefix}_JIRA_EMAIL` : "",
    apiTokenEnv: prefix ? `${prefix}_JIRA_API_TOKEN` : "",
  };
}

function scheduleText(schedule, nextBackgroundSyncAt) {
  if (!schedule.enabled) return "OS schedule off";
  const interval = `${schedule.interval_minutes}m`;
  if (!nextBackgroundSyncAt) return `OS schedule every ${interval}`;
  return `OS schedule every ${interval} · app sync next at ${formatClock(nextBackgroundSyncAt)}`;
}

function waitForMinimumBusy(startedAt) {
  const remaining = minimumBusyMs - (performance.now() - startedAt);
  if (remaining <= 0) return Promise.resolve();
  return new Promise((resolve) => setTimeout(resolve, remaining));
}

function formatElapsedMs(milliseconds) {
  const totalSeconds = Math.max(0, Math.floor(milliseconds / 1000));
  if (totalSeconds < 60) return `${totalSeconds}s`;
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return seconds === 0 ? `${minutes}m` : `${minutes}m ${seconds}s`;
}

function syncActivityCopy(label, command) {
  const background = label === "background sync";
  if (command === "dry_run") {
    return {
      runningTitle: "Previewing current changes",
      runningMessage: "Checking Toggl entries and Jira matches. No Jira worklogs are written during preview.",
      successTitle: "Preview complete",
      successMessage: "Review the ledger below before you write anything to Jira.",
      errorTitle: "Preview failed",
      toastLoading: "Previewing changes…",
      toastSuccess: "Preview complete.",
      toastError: "Preview failed",
      completeLabel: "ready",
    };
  }
  if (background) {
    return {
      runningTitle: "Background sync in progress",
      runningMessage: "The hourly app sync is checking recent Toggl entries and updating Jira in the background.",
      successTitle: "Background sync complete",
      successMessage: "The latest hourly sync finished. Review the ledger below if anything needs attention.",
      errorTitle: "Background sync failed",
      toastLoading: "Running background sync…",
      toastSuccess: "Background sync complete.",
      toastError: "Background sync failed",
      completeLabel: "synced",
    };
  }
  return {
    runningTitle: "Writing worklogs to Jira",
    runningMessage: "Reading recent Toggl entries and writing matching Jira worklogs. Keep this window open until it finishes.",
    successTitle: "Sync complete",
    successMessage: "The latest write finished. Review the ledger below for synced, skipped, or error rows.",
    errorTitle: "Sync failed",
    toastLoading: "Writing worklogs to Jira…",
    toastSuccess: "Sync complete.",
    toastError: "Sync failed",
    completeLabel: "synced",
  };
}

function summarizeActivityResult(summary, completeLabel) {
  const nextSummary = summary || { synced_count: 0, skipped_count: 0, error_count: 0 };
  return `${nextSummary.synced_count} ${completeLabel} · ${nextSummary.skipped_count} skipped · ${nextSummary.error_count} errors`;
}

function reasonKind(reason) {
  const normalized = (reason || "").toLowerCase();
  if (/toggl.*403|failed to fetch toggl entries.*403|toggl http error: 403/.test(normalized)) return "toggl-auth";
  if (/exists on multiple enabled jira sites|exists on multiple enabled sites/.test(normalized)) return "ambiguous-site";
  if (/was not found on any enabled jira site|was not found on any enabled site/.test(normalized)) return "missing-site";
  if (/no valid jira issue key found in toggl description/.test(normalized)) return "missing-issue-key";
  if (/multiple issue keys found/.test(normalized)) return "multiple-issue-keys";
  return null;
}

function summarizeReason(reason) {
  const value = reason || "-";
  switch (reasonKind(value)) {
    case "toggl-auth":
      return "Toggl auth rejected the request";
    case "ambiguous-site":
      return "Issue exists on multiple enabled Jira sites";
    case "missing-site":
      return "Issue was not found on any enabled Jira site";
    case "missing-issue-key":
      return "No Jira issue key found in the Toggl description";
    case "multiple-issue-keys":
      return "Multiple Jira issue keys were found";
    default:
      return value;
  }
}

function reasonGuidance(row, readOnlyConfig) {
  const togglConfigStep = readOnlyConfig ? "View tenant configuration, then ask the tenant administrator to update the Toggl token or workspace settings." : "Open Configuration and update the saved Toggl token or workspace settings.";
  const jiraConfigStep = readOnlyConfig ? "View tenant configuration, then ask the tenant administrator to update the Jira site list." : "Open Configuration and update the enabled Jira site list.";
  switch (reasonKind(row?.reason)) {
    case "toggl-auth":
      return {
        title: "Reconnect Toggl before trying again",
        body: "Toggl rejected the request. The saved API token may be expired, wrong for this workspace, or missing the right workspace access.",
        steps: [
          togglConfigStep,
          readOnlyConfig ? "Confirm the current workspace ID still belongs to the saved tenant token." : "Paste a current Toggl API token and confirm the workspace ID matches that token.",
          "Run Preview changes again after saving.",
        ],
        configCta: true,
      };
    case "ambiguous-site":
      return {
        title: "Narrow this issue to one enabled Jira site",
        body: "This issue key resolved on more than one enabled Jira site, so sync stopped rather than writing to the wrong place.",
        steps: [
          jiraConfigStep,
          readOnlyConfig ? "Ask the tenant administrator to disable the extra site or add the missing tenant so this issue resolves to one place." : "Add the missing site if you expect one more Jira tenant, or disable the extra site that should not receive this issue.",
          "Run Preview changes again after the site list is correct.",
        ],
        configCta: true,
      };
    case "missing-site":
      return {
        title: "Add or enable the Jira site that owns this issue",
        body: "The issue key could not be found on any enabled Jira site, so there is nowhere safe to write this worklog yet.",
        steps: [
          jiraConfigStep,
          readOnlyConfig ? "Ask the tenant administrator to add another Jira site or enable the right one if this issue lives in a different tenant." : "Add another Jira site or enable the right one if this issue lives in a different Jira tenant.",
          `Check that ${row.issue !== "-" ? row.issue : "the issue key"} exists in Jira, then preview again.`,
        ],
        configCta: true,
      };
    case "missing-issue-key":
      return {
        title: "Add one Jira issue key to the Toggl description",
        body: "The Toggl entry description does not contain a valid Jira issue key, so the sync cannot decide where to create the worklog.",
        steps: [
          "Edit the Toggl entry description and add one issue key such as PROJ-123.",
          "Keep only one Jira issue key in the description.",
          "Run Preview changes again after the Toggl entry is updated.",
        ],
        configCta: false,
      };
    case "multiple-issue-keys":
      return {
        title: "Keep only one Jira issue key in the Toggl description",
        body: "The description matched more than one Jira issue key, so sync stopped instead of guessing which issue should receive the worklog.",
        steps: [
          "Edit the Toggl entry description and leave only the single Jira issue key that should receive the worklog.",
          "Remove any extra key-like text from notes or copied links.",
          "Run Preview changes again after the Toggl entry is updated.",
        ],
        configCta: false,
      };
    default:
      return null;
  }
}

function siteResolutionPrompt(rows, readOnlyConfig) {
  const affected = rows.filter((row) => ["ambiguous-site", "missing-site"].includes(reasonKind(row.reason)));
  if (affected.length === 0) return null;
  const hasAmbiguous = affected.some((row) => reasonKind(row.reason) === "ambiguous-site");
  return {
    count: affected.length,
    title: hasAmbiguous ? "Jira site selection needs attention" : "A Jira site is missing from configuration",
    message: readOnlyConfig
      ? `${affected.length} worklogs need Jira site changes. View tenant configuration, then ask the tenant administrator to add or enable the right site.`
      : `${affected.length} worklogs need Jira site changes. Open Configuration to add another Jira site or enable the right one, then preview again.`,
    buttonLabel: readOnlyConfig ? "View tenant config" : "Open configuration",
  };
}

function App() {
  const [view, setView] = createSignal("overview");
  const [snapshot, setSnapshot] = createSignal(null);
  const [selected, setSelected] = createSignal(0);
  const [issueFilter, setIssueFilter] = createSignal("");
  const [dateFilter, setDateFilter] = createSignal("");
  const [loadError, setLoadError] = createSignal("");
  const [busyCommand, setBusyCommand] = createSignal(null);
  const [activity, setActivity] = createSignal(null);
  const [now, setNow] = createSignal(Date.now());
  const [guiBackgroundSyncEnabled, setGuiBackgroundSyncEnabled] = createSignal(true);
  const [nextBackgroundSyncAt, setNextBackgroundSyncAt] = createSignal(null);
  const [selectedMonth, setSelectedMonth] = createSignal(null);
  let backgroundTimer;

  const config = createMemo(() => snapshot()?.config);
  const schedule = createMemo(() => snapshot()?.schedule || { enabled: false, interval_minutes: 60 });
  const summary = createMemo(() => snapshot()?.status.summary || { total_count: 0, synced_count: 0, skipped_count: 0, error_count: 0 });
  const rows = createMemo(() => (snapshot()?.status.entries || []).map((entry) => rowView(entry, config(), now())));
  const months = createMemo(() => [...new Set(rows().map((row) => monthKey(row.date)).filter((key) => key !== "-"))]);
  const activeMonth = createMemo(() => {
    const selected = selectedMonth();
    if (selected && months().includes(selected)) return selected;
    return months()[0] || null;
  });
  const visibleRows = createMemo(() => {
    const issueNeedle = issueFilter().trim().toLowerCase();
    const dateNeedle = dateFilter().trim().toLowerCase();
    return rows()
      .map((row, index) => ({ row, index }))
      .filter(({ row }) => !activeMonth() || monthKey(row.date) === activeMonth())
      .filter(({ row }) => !issueNeedle || row.issue.toLowerCase().includes(issueNeedle))
      .filter(({ row }) => !dateNeedle || [row.date, row.time].join(" ").toLowerCase().includes(dateNeedle));
  });
  const selectedRow = createMemo(() => visibleRows()[selected()]?.row || visibleRows()[0]?.row);

  createEffect(() => {
    if (selected() >= visibleRows().length) setSelected(Math.max(0, visibleRows().length - 1));
  });

  createEffect(() => {
    const selected = selectedMonth();
    if (selected && !months().includes(selected)) setSelectedMonth(months()[0] || null);
    if (!selected && months().length > 0) setSelectedMonth(months()[0]);
  });

  createEffect(() => {
    view();
    window.scrollTo({ top: 0, left: 0 });
  });

  createEffect(() => {
    clearTimeout(backgroundTimer);
    if (isTenantMode || !schedule().enabled || !guiBackgroundSyncEnabled()) {
      setNextBackgroundSyncAt(null);
      return;
    }
    const delay = Math.max(1, schedule().interval_minutes) * 60 * 1000 || backgroundSyncIntervalMs;
    setNextBackgroundSyncAt(new Date(Date.now() + delay));
    backgroundTimer = setTimeout(() => runAction("background sync", "sync"), delay);
  });

  onMount(() => {
    refresh();
    const interval = setInterval(() => setNow(Date.now()), 1_000);
    onCleanup(() => {
      clearInterval(interval);
      clearTimeout(backgroundTimer);
    });
  });

  async function refresh() {
    try {
      setSnapshot(await api.snapshot());
      setLoadError("");
    } catch (error) {
      try {
        const fallback = await api.configSnapshot();
        setSnapshot({ ...fallback, status: { summary: { total_count: 0, synced_count: 0, skipped_count: 0, error_count: 0 }, entries: [] } });
        setLoadError("");
        toast.error(`Sync state unavailable: ${String(error)}`);
      } catch (configError) {
        setLoadError(`Configuration unavailable: ${String(configError)}`);
        toast.error(`Configuration unavailable: ${String(configError)}`);
      }
    }
  }

  async function runAction(label, command) {
    if (busyCommand()) return;
    const startedAt = performance.now();
    const startedOn = Date.now();
    const copy = syncActivityCopy(label, command);
    const loadingToast = toast.loading(copy.toastLoading);
    setBusyCommand(command);
    setActivity({ status: "running", command, title: copy.runningTitle, message: copy.runningMessage, startedAt: startedOn });
    try {
      const nextSnapshot = command === "dry_run" ? await api.dryRun() : await api.sync();
      setSnapshot(nextSnapshot);
      await waitForMinimumBusy(startedAt);
      toast.dismiss(loadingToast);
      toast.success(copy.toastSuccess);
      setActivity({
        status: "success",
        command,
        title: copy.successTitle,
        message: summarizeActivityResult(nextSnapshot?.status?.summary, copy.completeLabel),
        detail: copy.successMessage,
        startedAt: startedOn,
        finishedAt: Date.now(),
      });
    } catch (error) {
      toast.dismiss(loadingToast);
      const message = String(error);
      toast.error(`${copy.toastError}: ${message}`);
      setActivity({
        status: "error",
        command,
        title: copy.errorTitle,
        message,
        detail: copy.runningMessage,
        startedAt: startedOn,
        finishedAt: Date.now(),
      });
    } finally {
      setBusyCommand(null);
    }
  }

  async function saveConfig(event) {
    event.preventDefault();
    const form = event.currentTarget.elements;
    const siteCards = [...event.currentTarget.querySelectorAll("[data-jira-site]")];
    setGuiBackgroundSyncEnabled(form.gui_background_sync_enabled.checked);
    const update = {
      toggl_workspace_id: Number(form.toggl_workspace_id.value),
      toggl_api_token_env: form.toggl_api_token_env.value.trim(),
      toggl_api_token_value: form.toggl_api_token_value.value.trim() || null,
      sqlite_path: form.sqlite_path.value.trim(),
      initial_backfill_from_month: inputMonthToConfig(form.initial_backfill_from_month.value),
      initial_backfill_days: Number(form.initial_backfill_days.value),
      recovery_from_month: inputMonthToConfig(form.recovery_from_month.value),
      recovery_scan_days: Number(form.recovery_scan_days.value),
      schedule_enabled: form.schedule_enabled.checked,
      schedule_interval_minutes: Number(form.schedule_interval_minutes.value),
      jira_sites: siteCards.map((siteCard) => {
        const value = (name) => siteCard.querySelector(`[name="${name}"]`)?.value.trim() || "";
        const checked = (name) => Boolean(siteCard.querySelector(`[name="${name}"]`)?.checked);
        const baseUrl = value("jira_base_url");
        const key = value("jira_key") || deriveJiraSiteKey(baseUrl);
        const envNames = jiraEnvNames(key);
        return {
          key,
          base_url: baseUrl,
          email_env: value("jira_email_env") || envNames.emailEnv,
          email_value: value("jira_email_value") || null,
          api_token_env: value("jira_api_token_env") || envNames.apiTokenEnv,
          api_token_value: value("jira_api_token_value") || null,
          enabled: checked("jira_enabled"),
        };
      }),
    };
    const loadingToast = toast.loading("Saving configuration…");
    try {
      await api.saveConfig(update);
      await refresh();
      toast.dismiss(loadingToast);
      toast.success("Configuration saved.");
    } catch (error) {
      toast.dismiss(loadingToast);
      toast.error(String(error));
    }
  }

  async function deleteLocalData() {
    if (!window.confirm("Delete the local SQLite sync data? Config and saved credentials stay in place.")) return;
    const loadingToast = toast.loading("Deleting local sync data…");
    try {
      const result = await api.deleteLocalData();
      await refresh();
      toast.dismiss(loadingToast);
      toast.success(result.deleted ? `Deleted local sync data: ${result.path}` : `No local sync data found: ${result.path}`);
    } catch (error) {
      toast.dismiss(loadingToast);
      toast.error(String(error));
    }
  }

  async function exportConfig() {
    const loadingToast = toast.loading("Exporting configuration…");
    try {
      const result = await api.exportConfig();
      toast.dismiss(loadingToast);
      toast.custom(
        (toastState) => (
          <div class="export-toast">
            <div>
              <strong>Configuration exported</strong>
              <p>{result.path}</p>
            </div>
            <button type="button" onClick={() => { openUrl(result.path); toast.dismiss(toastState.id); }}>Open</button>
          </div>
        ),
        { duration: 8000, position: "top-right" },
      );
    } catch (error) {
      toast.dismiss(loadingToast);
      toast.error(`Export failed: ${String(error)}`);
    }
  }

  async function openUrl(url) {
    if (!url) return;
    await tauriInvoke("open_url", { url }).catch((error) => toast.error(String(error)));
  }

  return (
      <AppShell
        view={view()}
        setView={setView}
      actions={
        <>
          <ActionButton label="Preview changes" busyLabel="Previewing…" command="dry_run" busyCommand={busyCommand()} onClick={() => runAction("preview", "dry_run")} secondary description="No Jira writes" />
          <ActionButton label="Write to Jira" busyLabel="Writing…" command="sync" busyCommand={busyCommand()} onClick={() => runAction("sync", "sync")} description="Creates worklogs" />
        </>
      }
    >
        <Switch>
          <Match when={view() === "overview"}>
            <section class="view-surface">
              <Overview
                summary={summary()}
                rows={visibleRows()}
                allRows={rows()}
                selected={selected()}
                selectedRow={selectedRow()}
                issueFilter={issueFilter()}
                dateFilter={dateFilter()}
                setIssueFilter={setIssueFilter}
                setDateFilter={setDateFilter}
                setSelected={setSelected}
                openUrl={openUrl}
                scheduleText={scheduleText(schedule(), nextBackgroundSyncAt())}
                months={months()}
                activeMonth={activeMonth()}
                setSelectedMonth={setSelectedMonth}
                activity={activity()}
                now={now()}
                setView={setView}
                readOnlyConfig={isTenantMode}
              />
            </section>
          </Match>
          <Match when={view() === "configuration"}>
            <section class="view-surface">
              <Show when={config()} fallback={<div class="panel empty-detail">{loadError() || "Loading configuration…"}</div>}>
                <Configuration config={config()} guiBackgroundSyncEnabled={guiBackgroundSyncEnabled()} saveConfig={saveConfig} deleteLocalData={deleteLocalData} exportConfig={exportConfig} readOnly={isTenantMode} />
              </Show>
            </section>
          </Match>
        </Switch>
    </AppShell>
  );
}

function AppShell(props) {
  const title = () => (props.view === "configuration" ? "Configuration" : "Worklog overview");
  const description = () => (
    props.view === "configuration"
      ? "Local paths, credentials, and scheduling stay explicit and reversible."
      : "Review the local ledger, preview changes, then write to Jira when the state looks right."
  );
  return (
    <div class="app-shell">
      <Toaster position="top-right" gutter={10} />
      <div class="shell-frame">
        <aside class="shell-nav">
          <div class="nav-brand" aria-label="Toggl Jira Sync">
            <strong>TJS</strong>
          </div>
      <NavTabs view={props.view} setView={props.setView} readOnlyConfig={isTenantMode} />
        </aside>
        <div class="shell-main">
          <TopBar title={title()} description={description()} actions={props.actions} />
          <main class="workspace">{props.children}</main>
        </div>
      </div>
    </div>
  );
}

function TopBar(props) {
  return (
    <header class="topbar">
      <div class="topbar-copy">
        <p class="eyebrow">Toggl to Jira sync</p>
        <div class="topbar-heading">
          <h1>{props.title}</h1>
          <p class="topbar-description">{props.description}</p>
        </div>
      </div>
      <div class="top-actions">{props.actions}</div>
    </header>
  );
}

function NavTabs(props) {
  return (
    <nav class="tabs" aria-label="Main navigation">
      <button title="Overview" aria-label="Overview, ledger and sync status" classList={{ active: props.view === "overview" }} onClick={() => props.setView("overview")}>
        <span class="nav-mark">O</span>
        <span class="nav-label">Overview</span>
      </button>
      <button title="Configuration" aria-label="Configuration, paths, secrets, and schedule" classList={{ active: props.view === "configuration" }} onClick={() => props.setView("configuration")}>
        <span class="nav-mark">C</span>
        <span class="nav-label">{props.readOnlyConfig ? "Tenant config" : "Configuration"}</span>
      </button>
    </nav>
  );
}

function ActionButton(props) {
  const busy = () => props.busyCommand === props.command;
  return <button class={props.secondary ? "secondary" : "primary"} classList={{ loading: busy() }} disabled={Boolean(props.busyCommand)} aria-busy={busy()} title={props.description} onClick={props.onClick}><span>{busy() ? props.busyLabel || "Checking…" : props.label}</span><small>{props.description}</small></button>;
}

function Overview(props) {
  const activeMonthIndex = () => props.months.indexOf(props.activeMonth);
  const previousMonth = () => props.months[activeMonthIndex() + 1];
  const nextMonth = () => props.months[activeMonthIndex() - 1];
  const configPrompt = createMemo(() => siteResolutionPrompt(props.allRows, props.readOnlyConfig));
  return (
    <>
      <SummaryMetrics summary={props.summary} />
      <Show when={props.activity}><SyncActivity activity={props.activity} now={props.now} /></Show>
      <Show when={configPrompt()}>
        {(prompt) => (
          <section class="panel overview-prompt">
            <div class="overview-prompt-copy">
              <p class="panel-kicker">Configuration</p>
              <h2>{prompt().title}</h2>
              <p>{prompt().message}</p>
            </div>
            <button type="button" class="secondary" onClick={() => props.setView("configuration")}>{prompt().buttonLabel}</button>
          </section>
        )}
      </Show>
      <div class="worklog-layout">
        <section class="panel worklog-list">
          <WorklogHeader
            rows={props.rows}
            allRows={props.allRows}
            activeMonth={props.activeMonth}
            scheduleText={props.scheduleText}
            previousMonth={previousMonth()}
            nextMonth={nextMonth()}
            setSelectedMonth={props.setSelectedMonth}
            issueFilter={props.issueFilter}
            dateFilter={props.dateFilter}
            setIssueFilter={props.setIssueFilter}
            setDateFilter={props.setDateFilter}
          />
          <WorklogTable rows={props.rows} selected={props.selected} setSelected={props.setSelected} />
        </section>
        <IssuePanel row={props.selectedRow} openUrl={props.openUrl} setView={props.setView} readOnlyConfig={props.readOnlyConfig} />
      </div>
    </>
  );
}

function SyncActivity(props) {
  const duration = () => {
    if (!props.activity) return "0s";
    const finishedAt = props.activity.finishedAt || props.now;
    return formatElapsedMs(finishedAt - props.activity.startedAt);
  };
  const timing = () => {
    if (!props.activity) return "";
    if (props.activity.status === "running") {
      return `Started at ${formatClock(new Date(props.activity.startedAt))} · ${duration()} elapsed`;
    }
    return `Last run took ${duration()} · finished at ${formatClock(new Date(props.activity.finishedAt || props.activity.startedAt))}`;
  };
  const lozengeTone = () => (props.activity.status === "success" ? "synced" : props.activity.status);
  const lozengeLabel = () => (props.activity.status === "success" ? "complete" : props.activity.status);
  return (
    <section class={`panel sync-activity ${props.activity.status}`}>
      <div class="sync-activity-copy">
        <p class="panel-kicker">Sync activity</p>
        <div class="sync-activity-heading">
          <h2>{props.activity.title}</h2>
          <span class={`lozenge ${lozengeTone()}`}>{lozengeLabel()}</span>
        </div>
        <p>{props.activity.message}</p>
      </div>
      <div class="sync-activity-meta">
        <p class="activity-timing">{timing()}</p>
        <Show when={props.activity.detail}><p class="activity-detail">{props.activity.detail}</p></Show>
      </div>
    </section>
  );
}

function SummaryMetrics(props) {
  return (
    <div class="metrics" aria-label="Sync summary">
      <Metric label="Entries" value={props.summary.total_count} />
      <Metric label="Synced" value={props.summary.synced_count} tone="synced" />
      <Metric label="Skipped" value={props.summary.skipped_count} tone="skipped" />
      <Metric label="Errors" value={props.summary.error_count} tone="error" />
    </div>
  );
}

function WorklogHeader(props) {
  return (
    <div class="panel-head worklog-toolbar">
      <div class="panel-copy">
        <p class="panel-kicker">Recent ledger</p>
        <h2>Worklogs</h2>
        <p>{props.rows.length} of {props.allRows.length} worklogs · {props.activeMonth ? monthLabel(props.activeMonth) : "All months"} · {props.scheduleText}</p>
      </div>
      <div class="toolbar-controls">
        <MonthPager activeMonth={props.activeMonth} previousMonth={props.previousMonth} nextMonth={props.nextMonth} setSelectedMonth={props.setSelectedMonth} />
        <WorklogFilters issueFilter={props.issueFilter} dateFilter={props.dateFilter} setIssueFilter={props.setIssueFilter} setDateFilter={props.setDateFilter} />
      </div>
    </div>
  );
}

function MonthPager(props) {
  return (
    <div class="month-pager" aria-label="Month pagination">
      <Show when={props.previousMonth}>
        <button class="ghost" onClick={() => props.setSelectedMonth(props.previousMonth)}>‹</button>
      </Show>
      <span>{props.activeMonth ? monthLabel(props.activeMonth) : "No worklogs"}</span>
      <Show when={props.nextMonth}>
        <button class="ghost" onClick={() => props.setSelectedMonth(props.nextMonth)}>›</button>
      </Show>
    </div>
  );
}

function WorklogFilters(props) {
  return (
    <div class="filters" role="group" aria-label="Worklog filters">
      <label class="filter-field">
        <span>Issue</span>
        <input value={props.issueFilter} onInput={(event) => props.setIssueFilter(event.currentTarget.value)} placeholder="PROJ-123" aria-label="Filter by issue key" />
      </label>
      <label class="filter-field">
        <span>Date or time</span>
        <input value={props.dateFilter} onInput={(event) => props.setDateFilter(event.currentTarget.value)} placeholder="2026-05 or 09:30" aria-label="Filter by date or time" />
      </label>
      <button type="button" class="ghost" onClick={() => { props.setIssueFilter(""); props.setDateFilter(""); }}>Reset</button>
    </div>
  );
}

function WorklogTable(props) {
  return (
    <div class="table-wrap">
      <table>
        <thead><tr><th>Date</th><th>Time</th><th>Duration</th><th>Issue</th><th>Site</th><th>Worklog</th><th>Status</th><th>Reason</th></tr></thead>
        <tbody>
          <For each={props.rows}>{({ row }, index) => (
            <WorklogRow row={row} index={index()} selected={index() === props.selected} setSelected={props.setSelected} />
          )}</For>
        </tbody>
      </table>
    </div>
  );
}

function WorklogRow(props) {
  return (
    <tr
      classList={{ selected: props.selected }}
      tabindex="0"
      aria-selected={props.selected}
      onClick={() => props.setSelected(props.index)}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          props.setSelected(props.index);
        }
      }}
    >
      <td>{props.row.date}</td>
      <td>{props.row.time}</td>
      <td>{props.row.duration}</td>
      <td class="issue-cell">{props.row.issue}</td>
      <td>{props.row.site}</td>
      <td>{props.row.worklog}</td>
      <td><StatusLozenge status={props.row.status} /></td>
      <td class="reason-cell" title={props.row.reason}><span>{props.row.reasonSummary}</span></td>
    </tr>
  );
}

function Metric(props) {
  return <article class={`metric ${props.tone || ""}`}><span>{props.label}</span><strong>{props.value}</strong></article>;
}

function StatusLozenge(props) {
  return <span class={`lozenge ${props.status.replace("_", "-")}`}>{props.status}</span>;
}

function IssuePanel(props) {
  const guidance = () => props.row ? reasonGuidance(props.row, props.readOnlyConfig) : null;
  return (
    <aside class="issue-panel">
      <Show when={props.row} fallback={<p class="empty-detail">Select a worklog to inspect its Jira links.</p>}>
        {(row) => <>
          <div class="detail-heading">
            <div class="detail-copy">
              <span class="panel-kicker">Selected entry</span>
              <strong>{row().issue}</strong>
              <p>{row().date} · {row().duration} · {row().site}</p>
            </div>
            <StatusLozenge status={row().status} />
          </div>
          <dl class="detail-grid">
            <div><dt>Date</dt><dd>{row().date}</dd></div>
            <div><dt>Time</dt><dd>{row().time}</dd></div>
            <div><dt>Duration</dt><dd>{row().duration}</dd></div>
            <div><dt>Site</dt><dd>{row().site}</dd></div>
            <div><dt>Worklog</dt><dd>{row().worklog}</dd></div>
          </dl>
          <div class="detail-reason">
            <span>Reason</span>
            <p>{row().reason}</p>
            <Show when={guidance()}>
              {(help) => (
                <div class="detail-guidance">
                  <strong>{help().title}</strong>
                  <p>{help().body}</p>
                  <ul class="detail-steps">
                    <For each={help().steps}>{(step) => <li>{step}</li>}</For>
                  </ul>
                </div>
              )}
            </Show>
          </div>
          <div class="detail-links">
            <Show when={guidance()?.configCta}>
              <button class="secondary" type="button" onClick={() => props.setView("configuration")}>{props.readOnlyConfig ? "View tenant config" : "Open configuration"}</button>
            </Show>
            <button class="link-button" disabled={!row().issueUrl} onClick={() => props.openUrl(row().issueUrl)}>Open issue</button>
            <button class="link-button" disabled={!row().worklogUrl} onClick={() => props.openUrl(row().worklogUrl)}>Open worklog</button>
          </div>
        </>}
      </Show>
    </aside>
  );
}

function Configuration(props) {
  const disabled = () => props.readOnly;
  const newSite = () => ({ key: "", base_url: "", email_env: "", email_value: "", email_present: false, api_token_env: "", api_token_value: "", api_token_present: false, enabled: true, local_id: crypto.randomUUID() });
  const withLocalIds = (sites) => (sites.length ? sites : [newSite()]).map((site) => ({ ...site, local_id: site.local_id || crypto.randomUUID() }));
  const [sites, setSites] = createSignal(withLocalIds(props.config.jira_sites || []));
  const [showTogglToken, setShowTogglToken] = createSignal(false);
  const [showJiraToken, setShowJiraToken] = createSignal(false);
  createEffect(() => setSites(withLocalIds(props.config.jira_sites || [])));
  const addSite = () => setSites((current) => [...current, newSite()]);
  const removeSite = (localId) => setSites((current) => current.length === 1 ? current : current.filter((site) => site.local_id !== localId));
  return (
    <form class="panel config-grid" onSubmit={props.saveConfig}>
      <div class="panel-head full"><div><h2>{props.readOnly ? "Tenant configuration" : "Configuration"}</h2><p>{props.readOnly ? "Managed by the tenant administrator." : props.config.path}</p></div><Show when={!props.readOnly}><div class="config-actions"><button class="secondary" type="button" onClick={props.exportConfig}>Export configuration</button><button class="danger" type="button" onClick={props.deleteLocalData}>Delete local data</button><button class="primary" type="submit">Save configuration</button></div></Show></div>
      <label>Workspace ID <input name="toggl_workspace_id" type="number" value={props.config.toggl_workspace_id} required disabled={disabled()} /></label>
      <input type="hidden" name="toggl_api_token_env" value={props.config.toggl_api_token_env || "TOGGL_API_TOKEN"} />
      <SecretField label="Toggl API token" name="toggl_api_token_value" value={props.config.toggl_api_token_value || ""} visible={showTogglToken()} setVisible={setShowTogglToken} placeholder={props.config.toggl_api_token_present ? "Token saved; leave blank to keep it" : "Paste Toggl API token"} disabled={disabled()} />
      <label>SQLite path <input name="sqlite_path" value={props.config.sqlite_path} required disabled={disabled()} /></label>
      <label>Initial sync from month <input name="initial_backfill_from_month" type="month" value={configMonthToInput(props.config.initial_backfill_from_month)} disabled={disabled()} /></label>
      <label>Recovery scan from month <input name="recovery_from_month" type="month" value={configMonthToInput(props.config.recovery_from_month)} disabled={disabled()} /></label>
      <details class="advanced-config full"><summary>Advanced day-count fallback</summary><div class="advanced-grid"><label>Initial backfill days <input name="initial_backfill_days" type="number" min="1" value={props.config.initial_backfill_days} required disabled={disabled()} /></label><label>Recovery scan days <input name="recovery_scan_days" type="number" min="1" value={props.config.recovery_scan_days} required disabled={disabled()} /></label></div><p>Used only when the matching month field is empty. Month values use the first day of that month.</p></details>
      <label>Schedule interval minutes <input name="schedule_interval_minutes" type="number" min="1" value={props.config.schedule_interval_minutes} required disabled={disabled()} /></label>
      <label class="check"><input name="schedule_enabled" type="checkbox" checked={props.config.schedule_enabled} disabled={disabled()} /> Enable OS schedule</label>
      <Show when={!props.readOnly}><label class="check"><input name="gui_background_sync_enabled" type="checkbox" checked={props.guiBackgroundSyncEnabled} /> Sync hourly while this app is open</label></Show>
      <section class="sites-section full">
        <div class="site-card-head">
          <div>
            <h3>Jira sites</h3>
            <p class="sites-copy">Add every Jira site that might contain your issue keys. Sync checks each enabled site before it writes a worklog.</p>
            <span>{sites().length} configured</span>
          </div>
          <Show when={!props.readOnly}><button type="button" class="secondary" onClick={addSite}>Add another Jira site</button></Show>
        </div>
        <For each={sites()}>{(site, index) => (
          <section class="site-card" data-jira-site>
            <div class="site-card-head"><div><h3>Jira site {index() + 1}</h3><span>{site.key || "New site"}</span></div><Show when={!props.readOnly && sites().length > 1}><button type="button" class="danger" onClick={() => removeSite(site.local_id)}>Remove</button></Show></div>
            <label>Site key <input name="jira_key" value={site.key || ""} required disabled={disabled()} /></label><label>Base URL <input name="jira_base_url" value={site.base_url || ""} required disabled={disabled()} /></label>
            <input type="hidden" name="jira_email_env" value={site.email_env || ""} />
            <label>Jira email <input name="jira_email_value" type="email" value={site.email_value || ""} placeholder={site.email_present ? "Email saved; leave blank to keep it" : "name@example.com"} disabled={disabled()} /></label>
            <input type="hidden" name="jira_api_token_env" value={site.api_token_env || ""} />
            <SecretField label="Jira API token" name="jira_api_token_value" value={site.api_token_value || ""} visible={showJiraToken()} setVisible={setShowJiraToken} placeholder={site.api_token_present ? "Token saved; leave blank to keep it" : "Paste Jira API token"} disabled={disabled()} />
            <label class="check"><input name="jira_enabled" type="checkbox" checked={site.enabled ?? true} disabled={disabled()} /> Enabled</label>
          </section>
        )}</For>
      </section>
    </form>
  );
}

function SecretField(props) {
  const type = () => (props.visible ? props.inputType || "text" : "password");
  return (
    <label>{props.label}
      <span class="secret-control">
        <input name={props.name} type={type()} value={props.value} placeholder={props.placeholder} disabled={props.disabled} />
        <button type="button" class="secret-toggle" aria-label={props.visible ? `Hide ${props.label}` : `Show ${props.label}`} title={props.visible ? "Hide" : "Show"} disabled={props.disabled} onClick={() => props.setVisible(!props.visible)}><EyeIcon hidden={props.visible} /></button>
      </span>
    </label>
  );
}

function EyeIcon(props) {
  return (
    <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7Z" />
      <circle cx="12" cy="12" r="3" />
      <Show when={props.hidden}>
        <path d="M4 4l16 16" />
      </Show>
    </svg>
  );
}

function rowView(entry, config, now) {
  const [date, start] = splitDateTime(entry.started_at);
  const [, end] = splitDateTime(entry.stopped_at);
  const issue = entry.issue_key || "-";
  const site = entry.site || "-";
  const worklog = entry.worklog_id || "-";
  const fallbackSite = (config?.jira_sites || []).find((jiraSite) => jiraSite.enabled)?.key;
  const linkSite = site !== "-" ? site : fallbackSite;
  const baseUrl = linkSite ? `https://${linkSite}.atlassian.net` : null;
  const running = entry.stopped_at == null && entry.reason === "running entry";
  const reason = running ? "time entry is still running; worklog will be created after it stops" : entry.reason || "-";
  return { date, time: running ? `${start} – running` : `${start} – ${end}`, duration: running ? formatRunningDuration(entry.started_at, now) : formatDuration(entry.duration_seconds), issue, site, worklog, status: running ? "running" : entry.status, reason, reasonSummary: summarizeReason(reason), issueUrl: baseUrl && issue !== "-" ? `${baseUrl}/browse/${issue}` : null, worklogUrl: !running && baseUrl && issue !== "-" && worklog !== "-" ? `${baseUrl}/browse/${issue}?focusedWorklogId=${worklog}` : null };
}

const root = document.getElementById("root");
if (import.meta.hot?.data.disposeApp) import.meta.hot.data.disposeApp();
root.replaceChildren();
const disposeApp = render(() => <App />, root);
if (import.meta.hot) {
  import.meta.hot.data.disposeApp = disposeApp;
  import.meta.hot.dispose(() => disposeApp());
}
