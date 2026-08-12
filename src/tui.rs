use crate::backend::Backend;
use crate::executor::SystemExecutor;
use crate::model::{HostTarget, ObservedService, RuntimeState};
use crate::ops;
use crate::tmux::TmuxBackend;
use anyhow::{Result, bail};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const LOG_FETCH_CONCURRENCY: usize = 2;

pub fn run(
    hosts: Vec<HostTarget>,
    project_filter: Option<String>,
    tail: usize,
    timeout: Duration,
) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("distrun tui requires an interactive terminal");
    }
    let (request_tx, request_rx) = mpsc::channel();
    let (response_tx, response_rx) = mpsc::channel();
    spawn_worker(hosts, tail, timeout, request_rx, response_tx);

    let mut app = App::new(project_filter);
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
            WorkerResponse::Services(report) => {
                app.refresh_in_flight = false;
                app.set_service_report(report.observed, &report.unavailable_hosts);
                app.message = unavailable_message(&report.unavailable_hosts).unwrap_or_default();
                app.request_selected_logs(request_tx);
            }
            WorkerResponse::Logs { request_id, result } => {
                let Some(key) = app.finish_log_request(request_id) else {
                    continue;
                };
                let entry = app.logs.entry(key).or_default();
                entry.loading = false;
                match result {
                    Ok(text) => {
                        entry.text = plain_terminal_text(&text);
                        entry.error = None;
                    }
                    Err(error) => {
                        let error = plain_terminal_text(&error);
                        entry.error = Some(error.clone());
                        app.message = error;
                    }
                }
            }
        }
    }
}

fn handle_key(app: &mut App, request_tx: &Sender<WorkerRequest>, key: KeyEvent) {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }
    if app.filtering {
        handle_filter_key(app, request_tx, key);
        return;
    }

    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('/') => app.filtering = true,
        KeyCode::Esc => {
            app.update_filter(|filter| filter.clear());
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
        KeyCode::PageUp => app.scroll_logs_up(),
        KeyCode::PageDown => app.scroll_logs_down(),
        KeyCode::Home => app.scroll_logs_to_top(),
        KeyCode::End => app.follow_latest_logs(),
        KeyCode::F(5) | KeyCode::Char('r') => {
            app.request_refresh(request_tx);
            app.request_selected_logs(request_tx);
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
            app.update_filter(|filter| {
                filter.pop();
            });
            app.request_selected_logs(request_tx);
        }
        KeyCode::Char(ch) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            app.update_filter(|filter| filter.push(ch));
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

    draw_runtime_table(frame, app, chunks[0]);
    draw_logs(frame, app, chunks[1]);
    draw_footer(frame, app, chunks[2]);
}

fn draw_runtime_table(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let services = app.filtered_services().collect::<Vec<_>>();
    let mut name_counts = BTreeMap::new();
    for service in &app.services {
        *name_counts.entry(service.logical_key()).or_insert(0_usize) += 1;
    }
    let rows = services
        .iter()
        .map(|service| {
            let unavailable = app.unavailable_hosts.contains(&service.host);
            let name = if name_counts[&service.logical_key()] > 1 {
                format!("{} [{}]", service.name, service.instance_label())
            } else {
                service.name.clone()
            };
            Row::new(vec![
                Cell::from(service.host.clone()),
                Cell::from(service.project.clone()),
                Cell::from(name),
                Cell::from(if unavailable {
                    format!("{}*", service.runtime.as_str())
                } else {
                    service.runtime.as_str().to_owned()
                }),
            ])
            .style(if unavailable {
                Style::default().fg(Color::Gray)
            } else {
                runtime_style(service.runtime)
            })
        })
        .collect::<Vec<_>>();

    let scope = app.project_filter.as_deref().unwrap_or("all projects");
    let mut title = format!(
        " runtime services: {} ({}/{}) ",
        scope,
        services.len(),
        app.services.len()
    );
    if !app.filter.is_empty() {
        title.push_str(&format!(" filter:{} ", app.filter));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(24),
            Constraint::Min(18),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec!["HOST", "PROJECT", "SERVICE", "RUNTIME"]).style(
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

fn draw_logs(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.sync_log_view();
    let (mut title, text) = app.selected_log_text();
    let inner_height = area.height.saturating_sub(2);
    let lines = text.lines().count().min(u16::MAX as usize) as u16;
    app.set_log_viewport(lines.saturating_sub(inner_height), inner_height);
    title.push_str(if app.follow_logs {
        "[follow] "
    } else {
        "[paused] "
    });
    let paragraph = Paragraph::new(text)
        .block(Block::default().title(title).borders(Borders::ALL))
        .scroll((app.log_scroll, 0));

    frame.render_widget(paragraph, area);
}

fn draw_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut spans = Vec::new();
    if app.filtering {
        spans.push(Span::styled(
            "filtering | ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if !app.message.is_empty() {
        spans.push(Span::styled(&app.message, Style::default().fg(Color::Cyan)));
        spans.push(Span::raw(" | "));
    }
    spans.extend([
        Span::raw("q quit "),
        Span::raw("| j/k service "),
        Span::raw("| PgUp/PgDn "),
        Span::raw("| Home/End "),
        Span::raw("| / filter "),
        Span::raw("| r refresh"),
    ]);
    let footer = Paragraph::new(Line::from(spans));
    frame.render_widget(footer, area);
}

fn runtime_style(runtime: RuntimeState) -> Style {
    match runtime {
        RuntimeState::Running => Style::default().fg(Color::Green),
        RuntimeState::Exited => Style::default().fg(Color::Red),
        RuntimeState::Unknown => Style::default().fg(Color::Gray),
    }
}

fn unavailable_message(hosts: &[ops::UnavailableHost]) -> Option<String> {
    match hosts {
        [] => None,
        [host] => Some(format!("{} unavailable", host.host)),
        [first, rest @ ..] => Some(format!("{} unavailable (+{} more)", first.host, rest.len())),
    }
}

fn spawn_worker(
    hosts: Vec<HostTarget>,
    tail: usize,
    timeout: Duration,
    request_rx: Receiver<WorkerRequest>,
    response_tx: Sender<WorkerResponse>,
) {
    let hosts = Arc::new(hosts);
    let refresh_hosts = Arc::clone(&hosts);
    thread::spawn(move || {
        run_worker(
            request_rx,
            response_tx,
            move || {
                let backend = TmuxBackend::new(SystemExecutor);
                ops::status_all_with_timeout(&backend, refresh_hosts.iter(), timeout)
            },
            move |service| {
                let backend = TmuxBackend::new(SystemExecutor);
                hosts
                    .iter()
                    .find(|host| host.name == service.host)
                    .ok_or_else(|| {
                        anyhow::anyhow!("host `{}` is not in the TUI scope", service.host)
                    })
                    .and_then(|host| backend.logs_with_timeout(host, &service, tail, timeout))
                    .map_err(error_message)
            },
        );
    });
}

fn run_worker<Refresh, Logs>(
    request_rx: Receiver<WorkerRequest>,
    response_tx: Sender<WorkerResponse>,
    mut refresh: Refresh,
    logs: Logs,
) where
    Refresh: FnMut() -> ops::StatusAllReport,
    Logs: Fn(ObservedService) -> Result<String, String> + Send + Sync + 'static,
{
    let logs = Arc::new(logs);
    let log_requests = Arc::new(LatestLogQueue::default());
    let log_workers = (0..LOG_FETCH_CONCURRENCY)
        .map(|_| {
            let log_worker_requests = Arc::clone(&log_requests);
            let log_response_tx = response_tx.clone();
            let logs = Arc::clone(&logs);
            thread::spawn(move || {
                while let Some(request) = log_worker_requests.take() {
                    let result = logs(request.service);
                    let _ = log_response_tx.send(WorkerResponse::Logs {
                        request_id: request.id,
                        result,
                    });
                }
            })
        })
        .collect::<Vec<_>>();

    while let Ok(request) = request_rx.recv() {
        match request {
            WorkerRequest::Refresh => {
                let _ = response_tx.send(WorkerResponse::Services(refresh()));
            }
            WorkerRequest::Logs {
                request_id,
                service,
            } => {
                log_requests.replace(LogRequest {
                    id: request_id,
                    service: *service,
                });
            }
        }
    }
    log_requests.close();
    for log_worker in log_workers {
        let _ = log_worker.join();
    }
}

struct LogRequest {
    id: u64,
    service: ObservedService,
}

#[derive(Default)]
struct LatestLogQueue {
    state: Mutex<LatestLogState>,
    ready: Condvar,
}

#[derive(Default)]
struct LatestLogState {
    request: Option<LogRequest>,
    closed: bool,
}

impl LatestLogQueue {
    fn replace(&self, request: LogRequest) {
        let mut state = self.lock_state();
        if state.closed {
            return;
        }
        state.request = Some(request);
        self.ready.notify_one();
    }

    fn take(&self) -> Option<LogRequest> {
        let state = self.lock_state();
        let mut state = self
            .ready
            .wait_while(state, |state| state.request.is_none() && !state.closed)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.request.take()
    }

    fn close(&self) {
        let mut state = self.lock_state();
        state.closed = true;
        state.request = None;
        self.ready.notify_all();
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, LatestLogState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn error_message(error: anyhow::Error) -> String {
    format!("{error:#}")
}

fn plain_terminal_text(input: &str) -> String {
    #[derive(Clone, Copy)]
    enum EscapeState {
        Ground,
        Escape,
        Csi,
        ControlString,
        ControlStringEscape,
    }

    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut state = EscapeState::Ground;
    while let Some(ch) = chars.next() {
        state = match state {
            EscapeState::Ground => match ch {
                '\u{1b}' => EscapeState::Escape,
                '\r' => {
                    if chars.peek() != Some(&'\n') {
                        output.push('\n');
                    }
                    EscapeState::Ground
                }
                '\n' | '\t' => {
                    output.push(ch);
                    EscapeState::Ground
                }
                ch if ch.is_control() => EscapeState::Ground,
                _ => {
                    output.push(ch);
                    EscapeState::Ground
                }
            },
            EscapeState::Escape => match ch {
                '[' => EscapeState::Csi,
                ']' | 'P' | 'X' | '^' | '_' => EscapeState::ControlString,
                '@'..='~' => EscapeState::Ground,
                _ => EscapeState::Escape,
            },
            EscapeState::Csi => {
                if ('@'..='~').contains(&ch) {
                    EscapeState::Ground
                } else {
                    EscapeState::Csi
                }
            }
            EscapeState::ControlString => match ch {
                '\u{7}' => EscapeState::Ground,
                '\u{1b}' => EscapeState::ControlStringEscape,
                _ => EscapeState::ControlString,
            },
            EscapeState::ControlStringEscape => {
                if ch == '\\' {
                    EscapeState::Ground
                } else {
                    EscapeState::ControlString
                }
            }
        };
    }
    output
}

#[derive(Debug)]
struct App {
    project_filter: Option<String>,
    services: Vec<ObservedService>,
    table_state: TableState,
    logs: BTreeMap<ServiceKey, LogCache>,
    log_view_key: Option<ServiceKey>,
    log_scroll: u16,
    log_max_scroll: u16,
    log_view_height: u16,
    follow_logs: bool,
    latest_log_request: Option<(u64, ServiceKey)>,
    next_log_request_id: u64,
    unavailable_hosts: BTreeSet<String>,
    refresh_in_flight: bool,
    next_refresh: Instant,
    message: String,
    filter: String,
    filtering: bool,
    should_quit: bool,
}

impl App {
    fn new(project_filter: Option<String>) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        Self {
            project_filter,
            services: Vec::new(),
            table_state,
            logs: BTreeMap::new(),
            log_view_key: None,
            log_scroll: 0,
            log_max_scroll: 0,
            log_view_height: 1,
            follow_logs: true,
            latest_log_request: None,
            next_log_request_id: 0,
            unavailable_hosts: BTreeSet::new(),
            refresh_in_flight: false,
            next_refresh: Instant::now(),
            message: String::new(),
            filter: String::new(),
            filtering: false,
            should_quit: false,
        }
    }

    fn request_refresh(&mut self, request_tx: &Sender<WorkerRequest>) {
        if self.refresh_in_flight {
            return;
        }
        if request_tx.send(WorkerRequest::Refresh).is_ok() {
            self.refresh_in_flight = true;
            self.next_refresh = Instant::now() + REFRESH_INTERVAL;
        }
    }

    fn request_refresh_if_due(&mut self, request_tx: &Sender<WorkerRequest>) {
        if Instant::now() >= self.next_refresh {
            self.request_refresh(request_tx);
        }
    }

    fn request_selected_logs(&mut self, request_tx: &Sender<WorkerRequest>) {
        let Some(service) = self.selected_service().cloned() else {
            self.cancel_log_request();
            return;
        };
        if self.unavailable_hosts.contains(&service.host) {
            self.cancel_log_request();
            return;
        }
        let key = ServiceKey::from_service(&service);
        if matches!(
            &self.latest_log_request,
            Some((_, requested_key)) if requested_key == &key
        ) {
            return;
        }

        self.cancel_log_request();
        let request_id = self.next_log_request_id;
        self.next_log_request_id = self.next_log_request_id.wrapping_add(1);
        self.latest_log_request = Some((request_id, key.clone()));
        self.logs.entry(key.clone()).or_default().loading = true;

        if request_tx
            .send(WorkerRequest::Logs {
                request_id,
                service: Box::new(service),
            })
            .is_err()
        {
            self.cancel_log_request();
        }
    }

    fn sync_log_view(&mut self) {
        let selected = self.selected_service().map(ServiceKey::from_service);
        if self.log_view_key != selected {
            self.log_view_key = selected;
            self.log_scroll = 0;
            self.follow_logs = true;
        }
    }

    fn set_log_viewport(&mut self, max_scroll: u16, height: u16) {
        self.log_max_scroll = max_scroll;
        self.log_view_height = height.max(1);
        self.log_scroll = if self.follow_logs {
            max_scroll
        } else {
            self.log_scroll.min(max_scroll)
        };
    }

    fn scroll_logs_up(&mut self) {
        if self.log_max_scroll == 0 {
            return;
        }
        self.follow_logs = false;
        self.log_scroll = self.log_scroll.saturating_sub(self.log_view_height);
    }

    fn scroll_logs_down(&mut self) {
        self.log_scroll = self
            .log_scroll
            .saturating_add(self.log_view_height)
            .min(self.log_max_scroll);
        self.follow_logs = self.log_scroll == self.log_max_scroll;
    }

    fn scroll_logs_to_top(&mut self) {
        self.log_scroll = 0;
        self.follow_logs = false;
    }

    fn follow_latest_logs(&mut self) {
        self.log_scroll = self.log_max_scroll;
        self.follow_logs = true;
    }

    fn cancel_log_request(&mut self) {
        let Some((_, key)) = self.latest_log_request.take() else {
            return;
        };
        if let Some(entry) = self.logs.get_mut(&key) {
            entry.loading = false;
        }
    }

    fn finish_log_request(&mut self, request_id: u64) -> Option<ServiceKey> {
        if !matches!(self.latest_log_request, Some((current_id, _)) if current_id == request_id) {
            return None;
        }
        self.latest_log_request.take().map(|(_, key)| key)
    }

    #[cfg(test)]
    fn set_services(&mut self, services: Vec<ObservedService>) {
        self.unavailable_hosts.clear();
        self.replace_services(services);
    }

    fn set_service_report(
        &mut self,
        mut services: Vec<ObservedService>,
        unavailable_hosts: &[ops::UnavailableHost],
    ) {
        let unavailable = unavailable_hosts
            .iter()
            .map(|host| host.host.clone())
            .collect::<BTreeSet<_>>();
        let observed_keys = services
            .iter()
            .map(ServiceKey::from_service)
            .collect::<BTreeSet<_>>();
        services.extend(
            self.services
                .iter()
                .filter(|service| {
                    unavailable.contains(&service.host)
                        && !observed_keys.contains(&ServiceKey::from_service(service))
                })
                .cloned(),
        );
        self.unavailable_hosts = unavailable;
        self.replace_services(services);
    }

    fn replace_services(&mut self, mut services: Vec<ObservedService>) {
        let previous = self.selected_service().map(ServiceKey::from_service);
        if let Some(project) = &self.project_filter {
            services.retain(|service| service.project == *project);
        }
        services.sort_by(|left, right| {
            left.host
                .cmp(&right.host)
                .then_with(|| left.project.cmp(&right.project))
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.window_id.cmp(&right.window_id))
                .then_with(|| left.pane_id.cmp(&right.pane_id))
        });
        self.services = services;
        let current_keys = self
            .services
            .iter()
            .map(ServiceKey::from_service)
            .collect::<BTreeSet<_>>();
        if matches!(
            &self.latest_log_request,
            Some((_, key)) if !current_keys.contains(key)
        ) {
            self.cancel_log_request();
        }
        self.logs.retain(|key, _| current_keys.contains(key));
        self.select_key_or_first(previous);
    }

    fn update_filter(&mut self, update: impl FnOnce(&mut String)) {
        let selected = self.selected_service().map(ServiceKey::from_service);
        update(&mut self.filter);
        self.select_key_or_first(selected);
    }

    fn select_key_or_first(&mut self, key: Option<ServiceKey>) {
        let selected = key
            .and_then(|key| {
                self.filtered_services()
                    .position(|service| ServiceKey::from_service(service) == key)
            })
            .or_else(|| self.filtered_services().next().map(|_| 0));
        self.table_state.select(selected);
    }

    fn move_selection(&mut self, amount: isize) {
        let Some(last) = self.filtered_services().count().checked_sub(1) else {
            self.table_state.select(None);
            return;
        };

        let current = self.table_state.selected().unwrap_or(0);
        let next = if amount.is_negative() {
            current.saturating_sub(amount.unsigned_abs())
        } else {
            current.saturating_add(amount as usize).min(last)
        };
        self.table_state.select(Some(next));
    }

    fn clamp_selection(&mut self) {
        let Some(last) = self.filtered_services().count().checked_sub(1) else {
            self.table_state.select(None);
            return;
        };

        let selected = self.table_state.selected().unwrap_or(0).min(last);
        self.table_state.select(Some(selected));
    }

    fn filtered_services(&self) -> impl Iterator<Item = &ObservedService> {
        let filter = self.filter.to_ascii_lowercase();
        self.services
            .iter()
            .filter(move |service| service_matches_filter(service, &filter))
    }

    fn selected_service(&self) -> Option<&ObservedService> {
        let selected = self.table_state.selected()?;
        self.filtered_services().nth(selected)
    }

    fn selected_log_text(&self) -> (String, String) {
        let Some(service) = self.selected_service() else {
            return (" logs ".to_owned(), "No service selected.".to_owned());
        };
        let key = ServiceKey::from_service(service);
        let entry = self.logs.get(&key);
        let title_suffix = if matches!(entry, Some(entry) if entry.error.is_some()) {
            " error"
        } else if self.unavailable_hosts.contains(&service.host) {
            " stale"
        } else {
            ""
        };
        let instance = if self
            .services
            .iter()
            .filter(|candidate| candidate.logical_key() == service.logical_key())
            .take(2)
            .count()
            > 1
        {
            format!(" [{}]", service.instance_label())
        } else {
            String::new()
        };
        let title = format!(
            " logs {}/{}/{}{}{} ",
            key.host, key.project, key.service, instance, title_suffix
        );
        let text = match entry {
            Some(entry) if let Some(error) = &entry.error => error.clone(),
            Some(entry) if !entry.text.is_empty() => entry.text.clone(),
            Some(entry) if entry.loading => "Loading logs...".to_owned(),
            Some(_) => "No logs captured.".to_owned(),
            None if self.unavailable_hosts.contains(&service.host) => {
                "Host unavailable.".to_owned()
            }
            None => "Loading logs...".to_owned(),
        };

        (title, text)
    }
}

fn service_matches_filter(service: &ObservedService, filter: &str) -> bool {
    service.host.to_ascii_lowercase().contains(filter)
        || service.project.to_ascii_lowercase().contains(filter)
        || service.name.to_ascii_lowercase().contains(filter)
        || service.runtime.as_str().contains(filter)
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
    project: String,
    service: String,
    window_id: String,
    pane_id: String,
    runtime_id: Option<String>,
}

impl ServiceKey {
    fn from_service(service: &ObservedService) -> Self {
        Self {
            host: service.host.clone(),
            project: service.project.clone(),
            service: service.name.clone(),
            window_id: service.window_id.clone(),
            pane_id: service.pane_id.clone(),
            runtime_id: service.runtime_id.clone(),
        }
    }
}

enum WorkerRequest {
    Refresh,
    Logs {
        request_id: u64,
        service: Box<ObservedService>,
    },
}

enum WorkerResponse {
    Services(ops::StatusAllReport),
    Logs {
        request_id: u64,
        result: Result<String, String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::thread::JoinHandle;

    type WorkerHarness = (
        Sender<WorkerRequest>,
        Receiver<WorkerResponse>,
        JoinHandle<()>,
    );

    #[test]
    fn renders_project_runtime_rows_and_selected_logs() {
        let mut app = App::new(Some("readme".to_owned()));
        app.set_services(vec![
            observed("local", "readme", "api", RuntimeState::Running),
            observed("local", "other", "hidden", RuntimeState::Running),
            observed("web", "readme", "worker", RuntimeState::Exited),
        ]);
        app.logs.insert(
            ServiceKey::from_service(&observed("local", "readme", "api", RuntimeState::Running)),
            LogCache {
                text: "api listening on :8080\nGET /health 200\nworker queue connected".to_owned(),
                loading: true,
                ..LogCache::default()
            },
        );

        let rendered = render(&mut app, 110, 26, draw);
        for expected in [
            "runtime services: readme (2/2)",
            "HOST",
            "PROJECT",
            "SERVICE",
            "RUNTIME",
            "worker",
            "exited",
            "logs local/readme/api",
            "GET /health 200",
            "j/k service",
            "PgUp/PgDn",
            "r refresh",
        ] {
            assert!(
                rendered.contains(expected),
                "missing `{expected}`: {rendered}"
            );
        }
        assert!(!rendered.contains("hidden"));
    }

    #[test]
    fn duplicate_runtime_rows_and_log_titles_include_instance_identity() {
        let mut first = observed("local", "demo", "api", RuntimeState::Running);
        first.window_id = "@1".to_owned();
        first.pane_id = "%1".to_owned();
        let mut second = observed("local", "demo", "api", RuntimeState::Exited);
        second.window_id = "@2".to_owned();
        second.pane_id = "%2".to_owned();
        let mut app = App::new(Some("demo".to_owned()));
        app.set_services(vec![first, second]);

        let rendered = render(&mut app, 100, 18, draw);
        for expected in ["api [@1]", "api [@2]", "logs local/demo/api [@1]"] {
            assert!(
                rendered.contains(expected),
                "missing `{expected}`: {rendered}"
            );
        }
    }

    #[test]
    fn all_projects_are_visible_and_filterable_by_project() {
        let mut app = App::new(None);
        app.set_services(vec![
            observed("local", "alpha", "api", RuntimeState::Running),
            observed("local", "beta", "worker", RuntimeState::Unknown),
        ]);

        assert_eq!(app.filtered_services().count(), 2);
        app.filter = "beta".to_owned();
        assert_eq!(app.filtered_services().count(), 1);
        assert_eq!(
            app.selected_service()
                .map(|service| service.project.as_str()),
            Some("beta")
        );
    }

    #[test]
    fn unavailable_hosts_keep_their_last_observed_rows_as_stale() {
        let mut app = App::new(None);
        app.set_services(vec![observed("edge", "demo", "api", RuntimeState::Running)]);

        app.set_service_report(
            Vec::new(),
            &[ops::UnavailableHost {
                host: "edge".to_owned(),
                message: "timed out".to_owned(),
            }],
        );

        assert_eq!(app.services.len(), 1);
        assert!(app.unavailable_hosts.contains("edge"));
        assert!(app.selected_log_text().0.contains("stale"));
        let (request_tx, request_rx) = mpsc::channel();
        app.request_selected_logs(&request_tx);
        assert!(request_rx.try_recv().is_err());
    }

    #[test]
    fn filter_keeps_the_selected_service_when_it_still_matches() {
        let mut app = App::new(None);
        app.set_services(["alpha", "bravo", "charlie"].map(service).to_vec());
        app.table_state.select(Some(1));
        let (request_tx, request_rx) = mpsc::channel();

        handle_filter_key(
            &mut app,
            &request_tx,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        );

        assert_eq!(app.filter, "r");
        assert_eq!(
            app.selected_service().map(|service| service.name.as_str()),
            Some("bravo")
        );
        assert!(matches!(
            request_rx.try_recv(),
            Ok(WorkerRequest::Logs { service, .. }) if service.name == "bravo"
        ));
    }

    #[test]
    fn refresh_key_requests_a_runtime_refresh() {
        let mut app = App::new(None);
        let (request_tx, request_rx) = mpsc::channel();

        press(&mut app, &request_tx, KeyCode::Char('r'));

        assert!(app.refresh_in_flight);
        assert!(matches!(request_rx.try_recv(), Ok(WorkerRequest::Refresh)));
    }

    #[test]
    fn repeated_log_requests_for_the_same_service_stay_deduplicated() {
        let mut app = App::new(None);
        app.set_services(vec![service("api")]);
        let (request_tx, request_rx) = mpsc::channel();

        app.request_selected_logs(&request_tx);
        app.request_selected_logs(&request_tx);

        assert!(matches!(
            request_rx.try_recv(),
            Ok(WorkerRequest::Logs { service, .. }) if service.name == "api"
        ));
        assert!(request_rx.try_recv().is_err());
    }

    #[test]
    fn logs_scroll_to_the_latest_line() {
        let mut app = App::new(None);
        let api = service("api");
        app.set_services(vec![api.clone()]);
        app.logs.insert(
            ServiceKey::from_service(&api),
            LogCache {
                text: format!(
                    "{}\nLATEST",
                    (0..20)
                        .map(|line| format!("line {line}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
                ..LogCache::default()
            },
        );

        let rendered = render(&mut app, 24, 6, |frame, app| {
            draw_logs(frame, app, frame.area());
        });

        assert!(rendered.contains("LATEST"));
        assert!(app.follow_logs);
        assert_eq!(app.log_scroll, app.log_max_scroll);
    }

    #[test]
    fn log_navigation_keys_set_scroll_and_follow_mode() {
        let (request_tx, _request_rx) = mpsc::channel();

        for (key, start, following, expected_scroll, expected_following) in [
            (KeyCode::PageUp, 20, true, 15, false),
            (KeyCode::PageDown, 15, false, 20, true),
            (KeyCode::Home, 20, true, 0, false),
            (KeyCode::End, 0, false, 20, true),
        ] {
            let mut app = App::new(None);
            app.log_max_scroll = 20;
            app.log_scroll = start;
            app.log_view_height = 5;
            app.follow_logs = following;

            press(&mut app, &request_tx, key);

            assert_eq!(app.log_scroll, expected_scroll, "key: {key:?}");
            assert_eq!(app.follow_logs, expected_following, "key: {key:?}");
        }
    }

    #[test]
    fn changing_selection_restores_log_following() {
        let mut app = App::new(None);
        app.set_services(["alpha", "bravo"].map(service).to_vec());
        app.sync_log_view();
        app.log_scroll = 4;
        app.follow_logs = false;

        app.move_selection(1);
        app.sync_log_view();

        assert_eq!(app.log_scroll, 0);
        assert!(app.follow_logs);
        assert_eq!(
            app.log_view_key.as_ref().map(|key| key.service.as_str()),
            Some("bravo")
        );
    }

    #[test]
    fn narrow_log_panes_do_not_mutate_wide_graphemes() {
        let mut app = App::new(None);
        let api = service("api");
        app.set_services(vec![api.clone()]);
        app.logs.insert(
            ServiceKey::from_service(&api),
            LogCache {
                text: "甲乙".to_owned(),
                ..LogCache::default()
            },
        );

        let rendered = render(&mut app, 4, 4, |frame, app| {
            draw_logs(frame, app, frame.area());
        });

        assert!(rendered.contains('甲'));
    }

    #[test]
    fn ctrl_c_quits_while_filtering() {
        let mut app = App::new(None);
        app.filtering = true;
        let (request_tx, _request_rx) = mpsc::channel();

        handle_key(
            &mut app,
            &request_tx,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert!(app.should_quit);
    }

    #[test]
    fn stale_log_response_does_not_replace_the_latest_selection() {
        let mut app = App::new(None);
        let alpha = service("alpha");
        let bravo = service("bravo");
        let alpha_key = ServiceKey::from_service(&alpha);
        app.set_services(vec![alpha, bravo]);
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();

        app.request_selected_logs(&request_tx);
        let alpha_id = next_log_request(&request_rx);
        app.move_selection(1);
        app.request_selected_logs(&request_tx);
        let bravo_id = next_log_request(&request_rx);

        response_tx
            .send(WorkerResponse::Logs {
                request_id: alpha_id,
                result: Ok("stale alpha".to_owned()),
            })
            .expect("stale response");
        drain_responses(&mut app, &request_tx, &response_rx);
        assert!(
            app.logs
                .get(&alpha_key)
                .is_some_and(|entry| entry.text.is_empty())
        );

        response_tx
            .send(WorkerResponse::Logs {
                request_id: bravo_id,
                result: Ok("fresh bravo".to_owned()),
            })
            .expect("fresh response");
        drain_responses(&mut app, &request_tx, &response_rx);
        assert_eq!(app.selected_log_text().1, "fresh bravo");
    }

    #[test]
    fn terminal_control_sequences_are_removed_from_tui_logs() {
        assert_eq!(
            plain_terminal_text("\u{1b}[31mred\u{1b}[0m\r\nnext\u{1b}]0;title\u{7}\tvalue\u{8}\n"),
            "red\nnext\tvalue\n"
        );
    }

    #[test]
    fn terminal_control_sequences_are_removed_from_log_errors() {
        let mut app = App::new(Some("demo".to_owned()));
        app.set_services(vec![service("api")]);
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        app.request_selected_logs(&request_tx);
        let request_id = next_log_request(&request_rx);

        response_tx
            .send(WorkerResponse::Logs {
                request_id,
                result: Err("\u{1b}[31mpermission denied\u{1b}[0m\u{7}\nretry".to_owned()),
            })
            .expect("log error response");
        drain_responses(&mut app, &request_tx, &response_rx);

        assert_eq!(app.message, "permission denied\nretry");
        assert_eq!(app.selected_log_text().1, "permission denied\nretry");
    }

    #[test]
    fn empty_host_scope_refreshes_to_an_empty_report() {
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        spawn_worker(
            Vec::new(),
            80,
            Duration::from_millis(100),
            request_rx,
            response_tx,
        );

        request_tx
            .send(WorkerRequest::Refresh)
            .expect("request empty refresh");
        let WorkerResponse::Services(report) = response_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("empty refresh report")
        else {
            panic!("unexpected log response");
        };
        assert!(report.observed.is_empty());
        assert!(report.unavailable_hosts.is_empty());
    }

    #[test]
    fn a_hung_log_fetch_does_not_block_refresh_or_the_latest_selection() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let (request_tx, response_rx, worker) = test_worker(move |service| {
            started_tx
                .send(service.name.clone())
                .expect("announce log request");
            if service.name == "alpha" {
                release_rx
                    .lock()
                    .expect("lock release gate")
                    .recv()
                    .expect("release first request");
            }
            Ok(service.name)
        });

        queue_logs(&request_tx, 1, "alpha");
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(1)),
            Ok("alpha".to_owned())
        );

        request_tx
            .send(WorkerRequest::Refresh)
            .expect("request refresh");
        assert!(matches!(
            response_rx.recv_timeout(Duration::from_secs(1)),
            Ok(WorkerResponse::Services(_))
        ));

        queue_logs(&request_tx, 2, "bravo");

        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(1)),
            Ok("bravo".to_owned())
        );
        assert!(matches!(
            response_rx.recv_timeout(Duration::from_secs(1)),
            Ok(WorkerResponse::Logs {
                request_id: 2,
                result: Ok(name),
                ..
            }) if name == "bravo"
        ));
        assert!(started_rx.try_recv().is_err());

        release_tx.send(()).expect("release alpha");
        assert!(matches!(
            response_rx.recv_timeout(Duration::from_secs(1)),
            Ok(WorkerResponse::Logs { request_id: 1, .. })
        ));

        drop(request_tx);
        worker.join().expect("worker exits");
    }

    fn render(
        app: &mut App,
        width: u16,
        height: u16,
        draw_app: impl FnOnce(&mut Frame<'_>, &mut App),
    ) -> String {
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).expect("create test terminal");
        terminal
            .draw(|frame| draw_app(frame, app))
            .expect("draw test terminal");
        terminal.backend().to_string()
    }

    fn press(app: &mut App, request_tx: &Sender<WorkerRequest>, code: KeyCode) {
        handle_key(app, request_tx, KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn next_log_request(request_rx: &Receiver<WorkerRequest>) -> u64 {
        match request_rx.recv().expect("log request") {
            WorkerRequest::Logs { request_id, .. } => request_id,
            WorkerRequest::Refresh => panic!("unexpected refresh request"),
        }
    }

    fn queue_logs(request_tx: &Sender<WorkerRequest>, request_id: u64, name: &str) {
        let service = service(name);
        request_tx
            .send(WorkerRequest::Logs {
                request_id,
                service: Box::new(service),
            })
            .expect("queue log request");
    }

    fn test_worker(
        logs: impl Fn(ObservedService) -> Result<String, String> + Send + Sync + 'static,
    ) -> WorkerHarness {
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            run_worker(request_rx, response_tx, empty_report, logs);
        });
        (request_tx, response_rx, worker)
    }

    fn empty_report() -> ops::StatusAllReport {
        ops::StatusAllReport {
            observed: Vec::new(),
            unavailable_hosts: Vec::new(),
        }
    }

    fn service(name: &str) -> ObservedService {
        observed("local", "demo", name, RuntimeState::Running)
    }

    fn observed(host: &str, project: &str, name: &str, runtime: RuntimeState) -> ObservedService {
        ObservedService {
            project: project.to_owned(),
            host: host.to_owned(),
            name: name.to_owned(),
            window_id: format!("@{name}"),
            pane_id: format!("%{name}"),
            runtime_id: None,
            runtime,
        }
    }
}
