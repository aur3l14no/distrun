use crate::executor::SystemExecutor;
use crate::model::{Project, RuntimeState, ServiceStatus, SpecState};
use crate::ops;
use crate::tmux::TmuxBackend;
use anyhow::{Result, bail};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub fn run(project: Project, tail: usize) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("distrun tui requires an interactive terminal");
    }

    let (request_tx, request_rx) = mpsc::channel();
    let (response_tx, response_rx) = mpsc::channel();
    spawn_worker(project.clone(), tail, request_rx, response_tx);

    let mut app = App::new(project);
    app.request_refresh(&request_tx);

    ratatui::run(|terminal| run_loop(terminal, &mut app, &request_tx, &response_rx))
}

fn run_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    request_tx: &Sender<WorkerRequest>,
    response_rx: &Receiver<WorkerResponse>,
) -> Result<()> {
    while !app.should_quit {
        drain_responses(app, request_tx, response_rx);
        app.request_refresh_if_due(request_tx);
        terminal.draw(|frame| draw(frame, app))?;

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            handle_key(app, request_tx, key);
        }
    }

    Ok(())
}

fn drain_responses(
    app: &mut App,
    request_tx: &Sender<WorkerRequest>,
    response_rx: &Receiver<WorkerResponse>,
) {
    while let Ok(response) = response_rx.try_recv() {
        match response {
            WorkerResponse::Status(result) => {
                app.status_in_flight = false;
                match result {
                    Ok(statuses) => {
                        app.set_statuses(statuses);
                        app.status_error = None;
                        app.request_selected_logs(request_tx);
                    }
                    Err(error) => {
                        app.status_error = Some(error.clone());
                        app.message = error;
                    }
                }
            }
            WorkerResponse::Logs { key, result } => {
                app.log_in_flight.remove(&key);
                let entry = app.logs.entry(key).or_default();
                entry.loading = false;
                match result {
                    Ok(text) => {
                        entry.text = text;
                        entry.error = None;
                    }
                    Err(error) => {
                        entry.error = Some(error.clone());
                        app.message = error;
                    }
                }
            }
            WorkerResponse::Action(result) => {
                app.action_in_flight = false;
                app.message = match result {
                    Ok(message) => message,
                    Err(error) => error,
                };
                app.request_refresh(request_tx);
                app.request_selected_logs(request_tx);
            }
        }
    }
}

fn handle_key(app: &mut App, request_tx: &Sender<WorkerRequest>, key: KeyEvent) {
    if app.filtering {
        handle_filter_key(app, request_tx, key);
        return;
    }

    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('/') => app.filtering = true,
        KeyCode::Esc => {
            app.filter.clear();
            app.clamp_selection();
            app.request_selected_logs(request_tx);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_selection(1);
            app.request_selected_logs(request_tx);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_selection(-1);
            app.request_selected_logs(request_tx);
        }
        KeyCode::F(5) | KeyCode::Char('r') if key.modifiers.is_empty() => {
            app.request_refresh(request_tx);
            app.request_selected_logs(request_tx);
        }
        KeyCode::F(7) | KeyCode::Char('s') => {
            app.request_action(request_tx, ActionKind::Start);
        }
        KeyCode::F(9) | KeyCode::Char('x') => {
            app.request_action(request_tx, ActionKind::Stop);
        }
        KeyCode::F(8) => {
            app.request_action(request_tx, ActionKind::Restart);
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.request_action(request_tx, ActionKind::Restart);
        }
        _ => {}
    }
}

fn handle_filter_key(app: &mut App, request_tx: &Sender<WorkerRequest>, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => {
            app.filtering = false;
            app.clamp_selection();
            app.request_selected_logs(request_tx);
        }
        KeyCode::Backspace => {
            app.filter.pop();
            app.clamp_selection();
            app.request_selected_logs(request_tx);
        }
        KeyCode::Char(ch) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            app.filter.push(ch);
            app.clamp_selection();
            app.request_selected_logs(request_tx);
        }
        _ => {}
    }
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(55),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);

    draw_status_table(frame, app, chunks[0]);
    draw_logs(frame, app, chunks[1]);
    draw_footer(frame, app, chunks[2]);
}

fn draw_status_table(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let indexes = app.filtered_indexes();
    let rows = indexes
        .iter()
        .map(|index| {
            let status = &app.statuses[*index];
            Row::new(vec![
                Cell::from(status.host.clone()),
                Cell::from(status.service.clone()),
                Cell::from(runtime_label(status.runtime)),
                Cell::from(status.spec.as_str()),
            ])
            .style(status_style(status))
        })
        .collect::<Vec<_>>();

    let mut title = format!(
        " {} services ({}/{}) ",
        app.project.name,
        indexes.len(),
        app.statuses.len()
    );
    if app.status_error.is_some() {
        title.push_str(" stale ");
    }
    if !app.filter.is_empty() {
        title.push_str(&format!(" filter:{} ", app.filter));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Min(18),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec!["HOST", "SERVICE", "RUNTIME", "SPEC"]).style(
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(Block::default().title(title).borders(Borders::ALL))
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    frame.render_stateful_widget(table, area, &mut app.table_state);
}

fn draw_logs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let (title, text) = app.selected_log_text();
    let inner_height = area.height.saturating_sub(2) as usize;
    let scroll = text.lines().count().saturating_sub(inner_height) as u16;
    let paragraph = Paragraph::new(text)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    frame.render_widget(paragraph, area);
}

fn draw_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut spans = vec![
        Span::raw("q quit "),
        Span::raw("| up/down select "),
        Span::raw("| / filter "),
        Span::raw("| s start "),
        Span::raw("| x stop "),
        Span::raw("| Ctrl-R restart "),
    ];
    if app.filtering {
        spans.push(Span::styled(
            "| filtering",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if !app.message.is_empty() {
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(&app.message, Style::default().fg(Color::Cyan)));
    }
    let footer = Paragraph::new(Line::from(spans));
    frame.render_widget(footer, area);
}

fn status_style(status: &ServiceStatus) -> Style {
    match (status.runtime, status.spec) {
        (_, SpecState::Missing) => Style::default().fg(Color::Yellow),
        (_, SpecState::Orphan) => Style::default().fg(Color::Magenta),
        (Some(RuntimeState::Running), SpecState::InSync) => Style::default().fg(Color::Green),
        (Some(RuntimeState::Exited), _) => Style::default().fg(Color::Red),
        (Some(RuntimeState::Unknown), _) => Style::default().fg(Color::Gray),
        (None, _) => Style::default().fg(Color::Yellow),
    }
}

fn runtime_label(runtime: Option<RuntimeState>) -> &'static str {
    runtime.map(RuntimeState::as_str).unwrap_or("-")
}

fn spawn_worker(
    project: Project,
    tail: usize,
    request_rx: Receiver<WorkerRequest>,
    response_tx: Sender<WorkerResponse>,
) {
    thread::spawn(move || {
        let backend = TmuxBackend::new(SystemExecutor);
        while let Ok(request) = request_rx.recv() {
            match request {
                WorkerRequest::Refresh => {
                    let result = ops::status(&backend, &project).map_err(error_message);
                    let _ = response_tx.send(WorkerResponse::Status(result));
                }
                WorkerRequest::Logs(key) => {
                    let result =
                        ops::logs_for_host(&backend, &project, &key.host, &key.service, tail)
                            .map_err(error_message);
                    let _ = response_tx.send(WorkerResponse::Logs { key, result });
                }
                WorkerRequest::Action { kind, key } => {
                    let result = run_action(&backend, &project, kind, &key).map_err(error_message);
                    let _ = response_tx.send(WorkerResponse::Action(result));
                }
            }
        }
    });
}

fn run_action(
    backend: &TmuxBackend<SystemExecutor>,
    project: &Project,
    kind: ActionKind,
    key: &ServiceKey,
) -> Result<String> {
    let action = match kind {
        ActionKind::Start => ops::start_service(backend, project, &key.host, &key.service)?,
        ActionKind::Stop => ops::stop_service(backend, project, &key.host, &key.service)?,
        ActionKind::Restart => ops::restart_service(backend, project, &key.host, &key.service)?,
    };
    Ok(action.message())
}

fn error_message(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[derive(Debug)]
struct App {
    project: Project,
    statuses: Vec<ServiceStatus>,
    table_state: TableState,
    logs: BTreeMap<ServiceKey, LogCache>,
    log_in_flight: BTreeSet<ServiceKey>,
    status_in_flight: bool,
    action_in_flight: bool,
    next_refresh: Instant,
    status_error: Option<String>,
    message: String,
    filter: String,
    filtering: bool,
    should_quit: bool,
}

impl App {
    fn new(project: Project) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        Self {
            project,
            statuses: Vec::new(),
            table_state,
            logs: BTreeMap::new(),
            log_in_flight: BTreeSet::new(),
            status_in_flight: false,
            action_in_flight: false,
            next_refresh: Instant::now(),
            status_error: None,
            message: String::new(),
            filter: String::new(),
            filtering: false,
            should_quit: false,
        }
    }

    fn request_refresh(&mut self, request_tx: &Sender<WorkerRequest>) {
        if self.status_in_flight {
            return;
        }
        if request_tx.send(WorkerRequest::Refresh).is_ok() {
            self.status_in_flight = true;
            self.next_refresh = Instant::now() + REFRESH_INTERVAL;
        }
    }

    fn request_refresh_if_due(&mut self, request_tx: &Sender<WorkerRequest>) {
        if Instant::now() >= self.next_refresh {
            self.request_refresh(request_tx);
        }
    }

    fn request_selected_logs(&mut self, request_tx: &Sender<WorkerRequest>) {
        let Some((key, runtime)) = self.selected_key_with_runtime() else {
            return;
        };

        if runtime.is_none() {
            let entry = self.logs.entry(key).or_default();
            entry.text = "No logs yet. The service is missing.".to_owned();
            entry.loading = false;
            entry.error = None;
            return;
        }

        if self.log_in_flight.insert(key.clone()) {
            let entry = self.logs.entry(key.clone()).or_default();
            entry.loading = true;
            if request_tx.send(WorkerRequest::Logs(key.clone())).is_err() {
                self.log_in_flight.remove(&key);
            }
        }
    }

    fn request_action(&mut self, request_tx: &Sender<WorkerRequest>, kind: ActionKind) {
        if self.action_in_flight {
            return;
        }
        let Some((key, _)) = self.selected_key_with_runtime() else {
            self.message = "no service selected".to_owned();
            return;
        };

        if request_tx.send(WorkerRequest::Action { kind, key }).is_ok() {
            self.action_in_flight = true;
            self.message = "running action...".to_owned();
        }
    }

    fn set_statuses(&mut self, statuses: Vec<ServiceStatus>) {
        let previous = self.selected_status().map(ServiceKey::from_status);
        self.statuses = statuses;
        self.select_key_or_first(previous);
    }

    fn select_key_or_first(&mut self, key: Option<ServiceKey>) {
        let indexes = self.filtered_indexes();
        if indexes.is_empty() {
            self.table_state.select(None);
            return;
        }

        let selected = key
            .and_then(|key| {
                indexes
                    .iter()
                    .position(|index| ServiceKey::from_status(&self.statuses[*index]) == key)
            })
            .unwrap_or(0);
        self.table_state.select(Some(selected));
    }

    fn move_selection(&mut self, amount: isize) {
        let indexes = self.filtered_indexes();
        if indexes.is_empty() {
            self.table_state.select(None);
            return;
        }

        let current = self.table_state.selected().unwrap_or(0);
        let last = indexes.len() - 1;
        let next = if amount.is_negative() {
            current.saturating_sub(amount.unsigned_abs())
        } else {
            current.saturating_add(amount as usize).min(last)
        };
        self.table_state.select(Some(next));
    }

    fn clamp_selection(&mut self) {
        let indexes = self.filtered_indexes();
        if indexes.is_empty() {
            self.table_state.select(None);
            return;
        }

        let selected = self
            .table_state
            .selected()
            .unwrap_or(0)
            .min(indexes.len() - 1);
        self.table_state.select(Some(selected));
    }

    fn filtered_indexes(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.statuses.len()).collect();
        }

        let filter = self.filter.to_ascii_lowercase();
        self.statuses
            .iter()
            .enumerate()
            .filter_map(|(index, status)| status_matches_filter(status, &filter).then_some(index))
            .collect()
    }

    fn selected_key_with_runtime(&self) -> Option<(ServiceKey, Option<RuntimeState>)> {
        self.selected_status()
            .map(|status| (ServiceKey::from_status(status), status.runtime))
    }

    fn selected_status(&self) -> Option<&ServiceStatus> {
        let indexes = self.filtered_indexes();
        let selected = self.table_state.selected()?;
        indexes.get(selected).map(|index| &self.statuses[*index])
    }

    fn selected_log_text(&self) -> (String, String) {
        let Some(status) = self.selected_status() else {
            return (" logs ".to_owned(), "No service selected.".to_owned());
        };
        let key = ServiceKey::from_status(status);
        let entry = self.logs.get(&key);
        let title_suffix = if matches!(entry, Some(entry) if entry.error.is_some()) {
            " error"
        } else {
            ""
        };
        let title = format!(" logs {}:{}{} ", key.host, key.service, title_suffix);
        let text = match entry {
            Some(entry) if let Some(error) = &entry.error => error.clone(),
            Some(entry) if !entry.text.is_empty() => entry.text.clone(),
            Some(entry) if entry.loading => "Loading logs...".to_owned(),
            Some(_) => "No logs captured.".to_owned(),
            None if status.runtime.is_none() => "No logs yet. The service is missing.".to_owned(),
            None => "Loading logs...".to_owned(),
        };

        (title, text)
    }
}

fn status_matches_filter(status: &ServiceStatus, filter: &str) -> bool {
    status.host.to_ascii_lowercase().contains(filter)
        || status.service.to_ascii_lowercase().contains(filter)
        || runtime_label(status.runtime).contains(filter)
        || status.spec.as_str().contains(filter)
}

#[derive(Clone, Debug, Default)]
struct LogCache {
    text: String,
    error: Option<String>,
    loading: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ServiceKey {
    host: String,
    service: String,
}

impl ServiceKey {
    fn from_status(status: &ServiceStatus) -> Self {
        Self {
            host: status.host.clone(),
            service: status.service.clone(),
        }
    }
}

enum WorkerRequest {
    Refresh,
    Logs(ServiceKey),
    Action { kind: ActionKind, key: ServiceKey },
}

enum WorkerResponse {
    Status(Result<Vec<ServiceStatus>, String>),
    Logs {
        key: ServiceKey,
        result: Result<String, String>,
    },
    Action(Result<String, String>),
}

#[derive(Clone, Copy, Debug)]
enum ActionKind {
    Start,
    Stop,
    Restart,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DesiredService, HostTarget, HostTransport, OnExisting};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

    #[test]
    fn renders_status_table_and_selected_logs() {
        let backend = TestBackend::new(100, 26);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        let mut app = App::new(project());
        app.set_statuses(vec![
            status(
                "local",
                "api",
                Some(RuntimeState::Running),
                SpecState::InSync,
            ),
            status("local", "db", None, SpecState::Missing),
            status(
                "local",
                "old-cron",
                Some(RuntimeState::Running),
                SpecState::Orphan,
            ),
            status(
                "web",
                "worker",
                Some(RuntimeState::Exited),
                SpecState::InSync,
            ),
        ]);
        app.logs.insert(
            ServiceKey {
                host: "local".to_owned(),
                service: "api".to_owned(),
            },
            LogCache {
                text: "api listening on :8080\nGET /health 200\nworker queue connected".to_owned(),
                loading: true,
                ..LogCache::default()
            },
        );
        app.status_in_flight = true;

        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("readme services (4/4)"));
        assert!(rendered.contains("HOST"));
        assert!(rendered.contains("old-cron"));
        assert!(rendered.contains("orphan"));
        assert!(rendered.contains("web"));
        assert!(rendered.contains("GET /health 200"));
        assert!(rendered.contains("Ctrl-R restart"));
        assert!(!rendered.contains("refresh"));
        assert!(!rendered.contains("updating"));
        assert!(!rendered.contains("loading"));
    }

    fn project() -> Project {
        Project {
            name: "readme".to_owned(),
            on_existing: OnExisting::Skip,
            hosts: BTreeMap::from([
                (
                    "local".to_owned(),
                    HostTarget {
                        name: "local".to_owned(),
                        transport: HostTransport::Local,
                    },
                ),
                (
                    "web".to_owned(),
                    HostTarget {
                        name: "web".to_owned(),
                        transport: HostTransport::Ssh("web-prod".to_owned()),
                    },
                ),
            ]),
            services: BTreeMap::from([
                ("api".to_owned(), desired("readme", "local", "api")),
                ("db".to_owned(), desired("readme", "local", "db")),
                ("worker".to_owned(), desired("readme", "web", "worker")),
            ]),
        }
    }

    fn desired(project: &str, host: &str, name: &str) -> DesiredService {
        DesiredService {
            project: project.to_owned(),
            name: name.to_owned(),
            host: host.to_owned(),
            cmd: "sleep 60".to_owned(),
            cwd: None,
            env: BTreeMap::new(),
            stop_timeout: Duration::from_secs(1),
        }
    }

    fn status(
        host: &str,
        service: &str,
        runtime: Option<RuntimeState>,
        spec: SpecState,
    ) -> ServiceStatus {
        ServiceStatus {
            host: host.to_owned(),
            service: service.to_owned(),
            runtime,
            spec,
        }
    }
}
