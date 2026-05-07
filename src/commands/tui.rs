use std::{
    collections::HashMap,
    io,
    io::IsTerminal,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    DefaultTerminal, Frame,
};

use crate::{
    cli::{SharedPaths, SyncArgs, TuiArgs},
    commands::{
        config::{resolve_config_path, resolve_db_path},
        schedule,
    },
    config::AppConfig,
    db::{Database, StoredStatusEntry},
    report::{StatusEntry, StatusReport},
    time::{format_duration, split_status_datetime},
};

pub async fn run(args: TuiArgs) -> anyhow::Result<()> {
    if !io::stdout().is_terminal() {
        anyhow::bail!(
            "tui requires an interactive terminal; use `tjs status` for non-interactive output"
        );
    }

    let config_path = resolve_config_path(args.paths.config.clone())?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let db_path = resolve_db_path(
        args.paths.db.clone(),
        &config_path,
        config.runtime.sqlite_path.as_deref(),
        "tui",
    )?;
    let database = Database::open(&db_path)
        .with_context(|| format!("failed to open SQLite DB {}", db_path.display()))?;
    database
        .run_migrations()
        .context("failed to run DB migrations")?;
    let rows = database
        .list_status_entries(args.limit)
        .context("failed to load status rows")?;
    let base_urls = config
        .enabled_jira_sites()
        .into_iter()
        .map(|site| {
            (
                site.key.clone(),
                site.base_url.trim_end_matches('/').to_owned(),
            )
        })
        .collect();

    let sync_paths = args.paths.clone();
    let mut terminal = ratatui::init();
    let result = run_app(
        &mut terminal,
        TuiApp::new(
            status_entries(rows),
            base_urls,
            config.schedule.enabled,
            config.schedule.interval_minutes,
        ),
        sync_paths,
        config_path,
        db_path,
        args.limit,
    )
    .await;
    ratatui::restore();
    result
}

async fn run_app(
    terminal: &mut DefaultTerminal,
    mut app: TuiApp,
    sync_paths: SharedPaths,
    config_path: std::path::PathBuf,
    db_path: std::path::PathBuf,
    limit: usize,
) -> anyhow::Result<()> {
    let sync_interval = Duration::from_secs(60 * 60);
    let mut next_hourly_sync = schedule_next_sync(Instant::now(), sync_interval);
    let mut pending_sync: Option<PendingSync> = None;

    while !app.quit {
        if pending_sync
            .as_ref()
            .is_some_and(|pending| pending.handle.is_finished())
        {
            let pending = pending_sync.take().expect("pending sync should exist");
            app.message = match pending.handle.join() {
                Ok(Ok(message)) => message,
                Ok(Err(error)) => format!("{} failed: {error}", pending.label),
                Err(_) => format!("{} task panicked", pending.label),
            };
            if app.message.starts_with("sync finished")
                || app.message.starts_with("dry-run finished")
            {
                refresh_rows(&mut app, &db_path, limit)?;
            }
            if !pending.dry_run {
                next_hourly_sync = schedule_next_sync(Instant::now(), sync_interval);
            }
        }

        terminal.draw(|frame| render(frame, &mut app))?;
        if pending_sync.is_none() && hourly_sync_due(Instant::now(), next_hourly_sync) {
            app.message = "running hourly sync...".to_owned();
            pending_sync = Some(spawn_sync_action(sync_paths.clone(), false, "hourly sync"));
        }
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match app.handle_key(key.code) {
                        TuiAction::None => {}
                        TuiAction::OpenIssue(url) | TuiAction::OpenWorklog(url) => {
                            app.message = open_url(&url)
                                .map(|()| format!("opened {url}"))
                                .unwrap_or_else(|error| format!("open failed: {error}"));
                        }
                        TuiAction::DryRun => {
                            if pending_sync.is_none() {
                                app.message = "running dry-run...".to_owned();
                                pending_sync =
                                    Some(spawn_sync_action(sync_paths.clone(), true, "dry-run"));
                            } else {
                                app.message = "sync already running".to_owned();
                            }
                        }
                        TuiAction::Sync => {
                            if pending_sync.is_none() {
                                app.message = "running sync...".to_owned();
                                pending_sync =
                                    Some(spawn_sync_action(sync_paths.clone(), false, "sync"));
                            } else {
                                app.message = "sync already running".to_owned();
                            }
                        }
                        TuiAction::ToggleSchedule => {
                            let enabled = !app.schedule_enabled;
                            if enabled {
                                match schedule::install_default_job(
                                    &config_path,
                                    app.schedule_interval_minutes,
                                ) {
                                    Ok(()) => {
                                        schedule::update_schedule_config(
                                            &config_path,
                                            None,
                                            Some(true),
                                        )?;
                                        app.schedule_enabled = true;
                                        app.message = format!(
                                            "OS schedule enabled: every {} minutes",
                                            app.schedule_interval_minutes
                                        );
                                    }
                                    Err(error) => {
                                        app.message = format!("OS schedule enable failed: {error}");
                                    }
                                }
                            } else {
                                match schedule::uninstall_job() {
                                    Ok(()) => {
                                        schedule::update_schedule_config(
                                            &config_path,
                                            None,
                                            Some(false),
                                        )?;
                                        app.schedule_enabled = false;
                                        app.message = "OS schedule disabled".to_owned();
                                    }
                                    Err(error) => {
                                        app.message =
                                            format!("OS schedule disable failed: {error}");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

struct PendingSync {
    label: &'static str,
    dry_run: bool,
    handle: thread::JoinHandle<anyhow::Result<String>>,
}

fn spawn_sync_action(paths: SharedPaths, dry_run: bool, label: &'static str) -> PendingSync {
    PendingSync {
        label,
        dry_run,
        handle: thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to start sync runtime")?
                .block_on(run_sync_action(paths, dry_run))
        }),
    }
}

fn schedule_next_sync(now: Instant, interval: Duration) -> Instant {
    now + interval
}

fn hourly_sync_due(now: Instant, next_sync: Instant) -> bool {
    now >= next_sync
}

fn refresh_rows(app: &mut TuiApp, db_path: &std::path::Path, limit: usize) -> anyhow::Result<()> {
    let database = Database::open(db_path)
        .with_context(|| format!("failed to open SQLite DB {}", db_path.display()))?;
    let rows = database
        .list_status_entries(limit)
        .context("failed to reload status rows")?;
    app.set_rows(status_entries(rows));
    Ok(())
}

fn status_entries(rows: Vec<StoredStatusEntry>) -> Vec<StatusEntry> {
    StatusReport::from_rows(rows).entries
}

async fn run_sync_action(paths: SharedPaths, dry_run: bool) -> anyhow::Result<String> {
    crate::commands::sync::run(SyncArgs {
        paths,
        dry_run,
        json: false,
        quiet: true,
    })
    .await?;
    Ok(if dry_run {
        "dry-run finished; rows refreshed".to_owned()
    } else {
        "sync finished; rows refreshed".to_owned()
    })
}

fn open_url(url: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = Command::new("cmd");

    #[cfg(target_os = "windows")]
    command.args(["/C", "start", url]);
    #[cfg(not(target_os = "windows"))]
    command.arg(url);

    command.spawn()?.wait()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiRow {
    pub date: String,
    pub start: String,
    pub end: String,
    pub duration: String,
    pub issue: String,
    pub site: String,
    pub worklog: String,
    pub status: String,
    pub reason: String,
    pub issue_url: Option<String>,
    pub worklog_url: Option<String>,
}

#[derive(Debug)]
pub struct TuiApp {
    rows: Vec<TuiRow>,
    base_urls: HashMap<String, String>,
    visible: Vec<usize>,
    selected: usize,
    mode: InputMode,
    issue_filter: String,
    date_filter: String,
    message: String,
    schedule_enabled: bool,
    schedule_interval_minutes: u32,
    quit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Issue,
    Date,
}

enum TuiAction {
    None,
    OpenIssue(String),
    OpenWorklog(String),
    DryRun,
    Sync,
    ToggleSchedule,
}

impl TuiApp {
    pub fn new(
        entries: Vec<StatusEntry>,
        base_urls: HashMap<String, String>,
        schedule_enabled: bool,
        schedule_interval_minutes: u32,
    ) -> Self {
        let rows = entries
            .into_iter()
            .map(|entry| TuiRow::from_status(entry, &base_urls))
            .collect::<Vec<_>>();
        let mut app = Self {
            rows,
            base_urls,
            visible: Vec::new(),
            selected: 0,
            mode: InputMode::Normal,
            issue_filter: String::new(),
            date_filter: String::new(),
            message:
                "↑/↓ move · / issue · f date · d dry-run · s sync · a OS schedule · o issue · w worklog · r reset · q quit"
                    .to_owned(),
            schedule_enabled,
            schedule_interval_minutes,
            quit: false,
        };
        app.apply_filters();
        app
    }

    fn handle_key(&mut self, code: KeyCode) -> TuiAction {
        match self.mode {
            InputMode::Issue => return self.handle_input(code, InputMode::Issue),
            InputMode::Date => return self.handle_input(code, InputMode::Date),
            InputMode::Normal => {}
        }

        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.move_down(),
            KeyCode::Up | KeyCode::Char('k') => self.move_up(),
            KeyCode::Char('/') => {
                self.mode = InputMode::Issue;
                self.message =
                    "issue search: type issue fragment, Enter to apply, Esc to cancel".to_owned();
            }
            KeyCode::Char('f') => {
                self.mode = InputMode::Date;
                self.message =
                    "date/time filter: type e.g. 2026-05-04 or 15:59, Enter to apply".to_owned();
            }
            KeyCode::Char('r') => {
                self.issue_filter.clear();
                self.date_filter.clear();
                self.apply_filters();
                self.message = "filters cleared".to_owned();
            }
            KeyCode::Char('d') => return TuiAction::DryRun,
            KeyCode::Char('s') => return TuiAction::Sync,
            KeyCode::Char('a') => return TuiAction::ToggleSchedule,
            KeyCode::Char('o') => {
                if let Some(url) = self.selected_row().and_then(|row| row.issue_url.clone()) {
                    return TuiAction::OpenIssue(url);
                }
                self.message = "selected row has no issue link".to_owned();
            }
            KeyCode::Char('w') => {
                if let Some(url) = self.selected_row().and_then(|row| row.worklog_url.clone()) {
                    return TuiAction::OpenWorklog(url);
                }
                self.message = "selected row has no worklog link".to_owned();
            }
            _ => {}
        }
        TuiAction::None
    }

    fn handle_input(&mut self, code: KeyCode, mode: InputMode) -> TuiAction {
        let target = match mode {
            InputMode::Issue => &mut self.issue_filter,
            InputMode::Date => &mut self.date_filter,
            InputMode::Normal => unreachable!(),
        };
        match code {
            KeyCode::Enter => {
                self.mode = InputMode::Normal;
                self.apply_filters();
                self.message = format!("{} rows", self.visible.len());
            }
            KeyCode::Esc => self.mode = InputMode::Normal,
            KeyCode::Backspace => {
                target.pop();
            }
            KeyCode::Char(char) => target.push(char),
            _ => {}
        }
        TuiAction::None
    }

    fn apply_filters(&mut self) {
        let issue = self.issue_filter.to_ascii_uppercase();
        let date = self.date_filter.to_ascii_lowercase();
        self.visible = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                (issue.is_empty() || row.issue.to_ascii_uppercase().contains(&issue))
                    && (date.is_empty()
                        || row.date.to_ascii_lowercase().contains(&date)
                        || row.start.to_ascii_lowercase().contains(&date)
                        || row.end.to_ascii_lowercase().contains(&date))
            })
            .map(|(index, _)| index)
            .collect();
        if self.selected >= self.visible.len() {
            self.selected = self.visible.len().saturating_sub(1);
        }
    }

    fn set_rows(&mut self, rows: Vec<StatusEntry>) {
        self.rows = rows
            .into_iter()
            .map(|entry| TuiRow::from_status(entry, &self.base_urls))
            .collect();
        self.apply_filters();
    }

    fn selected_row(&self) -> Option<&TuiRow> {
        self.visible
            .get(self.selected)
            .and_then(|index| self.rows.get(*index))
    }

    fn move_down(&mut self) {
        if !self.visible.is_empty() {
            self.selected = (self.selected + 1).min(self.visible.len() - 1);
        }
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

impl TuiRow {
    fn from_status(entry: StatusEntry, base_urls: &HashMap<String, String>) -> Self {
        let (date, start) = split_status_datetime(&entry.started_at);
        let (_, end) = split_status_datetime(&entry.stopped_at);
        let issue = entry.issue_key.unwrap_or_else(|| "-".to_owned());
        let site = entry.site.unwrap_or_else(|| "-".to_owned());
        let worklog = entry.worklog_id.unwrap_or_else(|| "-".to_owned());
        let issue_url = base_urls
            .get(&site)
            .filter(|_| issue != "-")
            .map(|base| format!("{base}/browse/{issue}"));
        let worklog_url = issue_url
            .as_ref()
            .filter(|_| worklog != "-")
            .map(|url| format!("{url}?focusedWorklogId={worklog}"));

        Self {
            date,
            start,
            end,
            duration: format_duration(entry.duration_seconds),
            issue,
            site,
            worklog,
            status: entry.status,
            reason: entry.reason.unwrap_or_else(|| "-".to_owned()),
            issue_url,
            worklog_url,
        }
    }
}

fn render(frame: &mut Frame<'_>, app: &mut TuiApp) {
    let [header, table_area, detail, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(4),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    let mode = match app.mode {
        InputMode::Normal => "normal",
        InputMode::Issue => "issue search",
        InputMode::Date => "date/time filter",
    };
    let active_filter = match app.mode {
        InputMode::Issue => format!("Issue search: {}█", app.issue_filter),
        InputMode::Date => format!("Date/time filter: {}█", app.date_filter),
        InputMode::Normal => format!(
            "Issue search: {} | Date/time filter: {} | rows={}/{}",
            empty_or_dash(&app.issue_filter),
            empty_or_dash(&app.date_filter),
            app.visible.len(),
            app.rows.len()
        ),
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                "Toggl → Jira Sync TUI".bold(),
                "  ".into(),
                mode.yellow(),
            ]),
            Line::from(format!(
                "{} | OS schedule: {} every {}m",
                active_filter,
                if app.schedule_enabled { "on" } else { "off" },
                app.schedule_interval_minutes
            )),
        ])
        .block(Block::default().borders(Borders::ALL)),
        header,
    );

    let rows = app
        .visible
        .iter()
        .filter_map(|index| app.rows.get(*index))
        .map(|row| {
            Row::new(vec![
                Cell::from(row.date.clone()),
                Cell::from(row.start.clone()),
                Cell::from(row.end.clone()),
                Cell::from(row.duration.clone()),
                Cell::from(row.issue.clone()),
                Cell::from(row.site.clone()),
                Cell::from(row.worklog.clone()),
                Cell::from(status_span(&row.status)),
                Cell::from(reason_span(&row.reason)),
            ])
        });
    let mut state = TableState::default().with_selected(Some(app.selected));
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Min(8),
        ],
    )
    .header(
        Row::new([
            "date", "start", "end", "duration", "issue", "site", "worklog", "status", "reason",
        ])
        .bold(),
    )
    .column_spacing(1)
    .block(Block::default().borders(Borders::ALL).title("Status"))
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(table, table_area, &mut state);

    let details = app.selected_row().map_or_else(
        || vec![Line::from("no row selected")],
        |row| {
            vec![
                Line::from(format!(
                    "Issue: {} | Worklog: {} | Reason: {}",
                    row.issue, row.worklog, row.reason
                )),
                Line::from(format!(
                    "Issue URL: {}",
                    row.issue_url.as_deref().unwrap_or("-")
                )),
                Line::from(format!(
                    "Worklog URL: {}",
                    row.worklog_url.as_deref().unwrap_or("-")
                )),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(details).block(Block::default().borders(Borders::ALL).title("Details")),
        detail,
    );
    frame.render_widget(Paragraph::new(app.message.clone()), footer);
}

fn status_span(status: &str) -> Span<'static> {
    match status {
        "synced" => Span::styled(status.to_owned(), Style::new().fg(Color::Green)),
        "error" => Span::styled(status.to_owned(), Style::new().fg(Color::Red).bold()),
        "skipped" => Span::styled(status.to_owned(), Style::new().fg(Color::Yellow)),
        _ => Span::styled(status.to_owned(), Style::new().fg(Color::Blue)),
    }
}

fn reason_span(reason: &str) -> Span<'static> {
    if reason == "running entry" {
        Span::styled("● running", Style::new().fg(Color::Yellow).bold())
    } else if reason == "-" {
        Span::raw("-")
    } else {
        Span::styled(format!("⚠ {reason}"), Style::new().fg(Color::Yellow))
    }
}

fn empty_or_dash(value: &str) -> &str {
    if value.is_empty() {
        "-"
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        issue: Option<&str>,
        started_at: &str,
        status: &str,
        reason: Option<&str>,
    ) -> StatusEntry {
        StatusEntry {
            workspace: "4528427".to_owned(),
            entry: "1".to_owned(),
            started_at: Some(started_at.to_owned()),
            stopped_at: Some("2026-05-04T17:43:00Z".to_owned()),
            duration_seconds: 6_240,
            issue_key: issue.map(str::to_owned),
            site: Some("sabservis".to_owned()),
            worklog_id: Some("26410".to_owned()),
            status: status.to_owned(),
            reason: reason.map(str::to_owned),
        }
    }

    #[test]
    fn tui_filters_by_issue_and_date_time() {
        let mut base_urls = HashMap::new();
        base_urls.insert(
            "sabservis".to_owned(),
            "https://sabservis.atlassian.net".to_owned(),
        );
        let mut app = TuiApp::new(
            vec![
                row(Some("CORE-223"), "2026-05-04T15:59:00Z", "synced", None),
                row(Some("CORE-202"), "2026-05-03T10:05:00Z", "synced", None),
            ],
            base_urls,
            true,
            60,
        );

        app.issue_filter = "223".to_owned();
        app.date_filter = "17:59".to_owned();
        app.apply_filters();

        assert_eq!(app.visible.len(), 1);
        assert_eq!(app.selected_row().unwrap().issue, "CORE-223");
    }

    #[test]
    fn tui_builds_issue_and_worklog_urls() {
        let mut base_urls = HashMap::new();
        base_urls.insert(
            "sabservis".to_owned(),
            "https://sabservis.atlassian.net".to_owned(),
        );
        let app = TuiApp::new(
            vec![row(
                Some("CORE-223"),
                "2026-05-04T15:59:00Z",
                "synced",
                None,
            )],
            base_urls,
            true,
            60,
        );

        let selected = app.selected_row().unwrap();
        assert_eq!(
            selected.issue_url.as_deref(),
            Some("https://sabservis.atlassian.net/browse/CORE-223")
        );
        assert_eq!(
            selected.worklog_url.as_deref(),
            Some("https://sabservis.atlassian.net/browse/CORE-223?focusedWorklogId=26410")
        );
    }

    #[test]
    fn tui_visualizes_running_reason() {
        assert_eq!(reason_span("running entry").content, "● running");
    }

    #[test]
    fn tui_has_dry_run_and_sync_key_actions() {
        let mut app = TuiApp::new(Vec::new(), HashMap::new(), true, 60);

        assert!(matches!(
            app.handle_key(KeyCode::Char('d')),
            TuiAction::DryRun
        ));
        assert!(matches!(
            app.handle_key(KeyCode::Char('s')),
            TuiAction::Sync
        ));
        assert!(matches!(
            app.handle_key(KeyCode::Char('a')),
            TuiAction::ToggleSchedule
        ));
    }

    #[test]
    fn hourly_sync_is_due_only_at_or_after_next_scheduled_time() {
        let now = Instant::now();
        let next_sync = now + Duration::from_secs(60 * 60);

        assert!(!hourly_sync_due(now, next_sync));
        assert!(!hourly_sync_due(
            next_sync - Duration::from_secs(1),
            next_sync
        ));
        assert!(hourly_sync_due(next_sync, next_sync));
        assert!(hourly_sync_due(
            next_sync + Duration::from_secs(1),
            next_sync
        ));
    }

    #[test]
    fn hourly_sync_schedules_next_run_from_completion_time() {
        let now = Instant::now();
        let interval = Duration::from_secs(60 * 60);

        assert_eq!(schedule_next_sync(now, interval), now + interval);
    }

    #[test]
    fn tui_refreshes_rows_and_preserves_active_filters() {
        let mut base_urls = HashMap::new();
        base_urls.insert(
            "sabservis".to_owned(),
            "https://sabservis.atlassian.net".to_owned(),
        );
        let mut app = TuiApp::new(
            vec![row(
                Some("CORE-223"),
                "2026-05-04T15:59:00Z",
                "synced",
                None,
            )],
            base_urls,
            true,
            60,
        );
        app.issue_filter = "202".to_owned();
        app.apply_filters();
        assert_eq!(app.visible.len(), 0);

        app.set_rows(vec![row(
            Some("CORE-202"),
            "2026-05-04T03:34:00Z",
            "synced",
            None,
        )]);

        assert_eq!(app.visible.len(), 1);
        assert_eq!(app.selected_row().unwrap().issue, "CORE-202");
    }
}
