use std::{
    collections::BTreeMap,
    io::{self, Stdout},
    sync::mpsc::Receiver,
    time::Duration,
};

use chrono::DateTime;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use url::form_urlencoded;

use crate::{
    model::{Endpoint, ExchangePart, LogEntry},
    server::CaptureServer,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FocusPane {
    Endpoints,
    Logs,
    Server,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum DetailTab {
    #[default]
    Request,
    Response,
    Headers,
}

impl DetailTab {
    const ALL: [Self; 3] = [Self::Request, Self::Response, Self::Headers];

    fn label(self) -> &'static str {
        match self {
            Self::Request => "Request",
            Self::Response => "Response",
            Self::Headers => "Headers",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Request => Self::Response,
            Self::Response => Self::Headers,
            Self::Headers => Self::Request,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Request => Self::Headers,
            Self::Response => Self::Request,
            Self::Headers => Self::Response,
        }
    }
}

enum Action {
    Continue,
    Restart,
    Quit,
}

#[derive(Clone, Copy, Debug, Default)]
struct UiAreas {
    endpoints_pane: Rect,
    endpoints_list: Rect,
    logs_pane: Rect,
    exchanges_list: Rect,
    detail: Rect,
    server_pane: Rect,
    tabs: [Rect; 3],
    close_detail: Rect,
}

struct App {
    endpoints: Vec<Endpoint>,
    filtered: Vec<usize>,
    selected_endpoint: usize,
    endpoint_list_offset: usize,
    focus: FocusPane,
    search_mode: bool,
    search_query: String,
    search_cursor: usize,
    logs: Vec<LogEntry>,
    selected_exchange: usize,
    exchange_list_offset: usize,
    detail_tab: DetailTab,
    detail_scroll: usize,
    pretty_bodies: bool,
    wrap_bodies: bool,
    detail_expanded: bool,
    output: Vec<String>,
    output_scroll: usize,
    show_help: bool,
    notice: Option<String>,
    areas: UiAreas,
}

impl App {
    fn new(endpoints: Vec<Endpoint>) -> Self {
        let filtered = (0..endpoints.len()).collect();
        Self {
            endpoints,
            filtered,
            selected_endpoint: 0,
            endpoint_list_offset: 0,
            focus: FocusPane::Endpoints,
            search_mode: false,
            search_query: String::new(),
            search_cursor: 0,
            logs: Vec::new(),
            selected_exchange: 0,
            exchange_list_offset: 0,
            detail_tab: DetailTab::Request,
            detail_scroll: 0,
            pretty_bodies: true,
            wrap_bodies: false,
            detail_expanded: false,
            output: Vec::new(),
            output_scroll: 0,
            show_help: false,
            notice: None,
            areas: UiAreas::default(),
        }
    }

    fn selected_endpoint(&self) -> Option<&Endpoint> {
        self.filtered
            .get(self.selected_endpoint)
            .and_then(|index| self.endpoints.get(*index))
    }

    fn matching_log_indices(&self) -> Vec<usize> {
        let Some(endpoint) = self.selected_endpoint() else {
            return Vec::new();
        };
        self.logs
            .iter()
            .enumerate()
            .filter(|(_, entry)| endpoint.matches(&entry.method, &entry.path))
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_log(&self) -> Option<&LogEntry> {
        let indices = self.matching_log_indices();
        let index = indices.get(self.selected_exchange.min(indices.len().saturating_sub(1)))?;
        self.logs.get(*index)
    }

    fn select_latest_exchange(&mut self) {
        self.selected_exchange = self.matching_log_indices().len().saturating_sub(1);
        self.detail_scroll = 0;
    }

    fn endpoint_changed(&mut self) {
        self.select_latest_exchange();
        self.detail_tab = DetailTab::Request;
        self.detail_expanded = false;
    }

    fn push_output(&mut self, line: String) {
        self.output.push(line);
        if self.output.len() > 1_000 {
            self.output.drain(..self.output.len() - 1_000);
        }
    }

    fn push_log(&mut self, entry: LogEntry) {
        let matches_selected = self
            .selected_endpoint()
            .is_some_and(|endpoint| endpoint.matches(&entry.method, &entry.path));
        self.logs.push(entry);
        if self.logs.len() > 100 {
            self.logs.drain(..self.logs.len() - 100);
        }
        if matches_selected {
            self.select_latest_exchange();
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if self.show_help {
            self.show_help = false;
            return Action::Continue;
        }
        if self.search_mode && self.focus == FocusPane::Endpoints {
            return self.handle_search_key(key);
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc if self.detail_expanded => {
                self.detail_expanded = false;
                Action::Continue
            }
            KeyCode::Char('?') | KeyCode::Char('h') => {
                self.show_help = true;
                Action::Continue
            }
            KeyCode::Char('/') if self.focus == FocusPane::Endpoints => {
                self.search_mode = true;
                self.search_query.clear();
                self.search_cursor = 0;
                self.filter_endpoints();
                Action::Continue
            }
            KeyCode::Left | KeyCode::Char('[') if self.focus == FocusPane::Logs => {
                self.detail_tab = self.detail_tab.previous();
                self.detail_scroll = 0;
                Action::Continue
            }
            KeyCode::Right | KeyCode::Char(']') if self.focus == FocusPane::Logs => {
                self.detail_tab = self.detail_tab.next();
                self.detail_scroll = 0;
                Action::Continue
            }
            KeyCode::PageUp if self.focus == FocusPane::Logs => {
                self.detail_scroll = self.detail_scroll.saturating_sub(5);
                Action::Continue
            }
            KeyCode::PageDown if self.focus == FocusPane::Logs => {
                self.detail_scroll = self.detail_scroll.saturating_add(5);
                Action::Continue
            }
            KeyCode::Char('p') if self.focus == FocusPane::Logs => {
                self.pretty_bodies = !self.pretty_bodies;
                self.detail_scroll = 0;
                Action::Continue
            }
            KeyCode::Char('w') if self.focus == FocusPane::Logs => {
                self.wrap_bodies = !self.wrap_bodies;
                self.detail_scroll = 0;
                Action::Continue
            }
            KeyCode::Char('e') if self.focus == FocusPane::Logs => {
                self.detail_expanded = !self.detail_expanded;
                Action::Continue
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    FocusPane::Endpoints => FocusPane::Logs,
                    FocusPane::Logs => FocusPane::Server,
                    FocusPane::Server => FocusPane::Endpoints,
                };
                Action::Continue
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    FocusPane::Endpoints => FocusPane::Server,
                    FocusPane::Logs => FocusPane::Endpoints,
                    FocusPane::Server => FocusPane::Logs,
                };
                Action::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.navigate_up();
                Action::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.navigate_down();
                Action::Continue
            }
            KeyCode::Enter | KeyCode::Char(' ') if self.focus == FocusPane::Endpoints => {
                self.focus = FocusPane::Logs;
                self.select_latest_exchange();
                Action::Continue
            }
            KeyCode::Char('r') => Action::Restart,
            _ => Action::Continue,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.show_help {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.show_help = false;
            }
            return;
        }

        let column = mouse.column;
        let row = mouse.row;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.handle_mouse_click(column, row),
            MouseEventKind::ScrollUp => self.handle_mouse_scroll(column, row, true),
            MouseEventKind::ScrollDown => self.handle_mouse_scroll(column, row, false),
            MouseEventKind::ScrollLeft
                if self.detail_expanded || rect_contains(self.areas.logs_pane, column, row) =>
            {
                self.focus = FocusPane::Logs;
                self.detail_tab = self.detail_tab.previous();
                self.detail_scroll = 0;
            }
            MouseEventKind::ScrollRight
                if self.detail_expanded || rect_contains(self.areas.logs_pane, column, row) =>
            {
                self.focus = FocusPane::Logs;
                self.detail_tab = self.detail_tab.next();
                self.detail_scroll = 0;
            }
            _ => {}
        }
    }

    fn handle_mouse_click(&mut self, column: u16, row: u16) {
        if self.detail_expanded {
            if rect_contains(self.areas.close_detail, column, row) {
                self.detail_expanded = false;
                return;
            }
            if self.select_tab_at(column, row) {
                return;
            }
            if rect_contains(self.areas.detail, column, row) {
                self.focus = FocusPane::Logs;
            }
            return;
        }

        if self.select_tab_at(column, row) {
            self.focus = FocusPane::Logs;
            return;
        }
        if rect_contains(self.areas.endpoints_list, column, row) {
            let visible_row = usize::from(row - self.areas.endpoints_list.y);
            let index = self.endpoint_list_offset + visible_row;
            if index < self.filtered.len() {
                self.selected_endpoint = index;
                self.focus = FocusPane::Endpoints;
                self.endpoint_changed();
            }
            return;
        }
        if rect_contains(self.areas.exchanges_list, column, row) {
            let visible_row = usize::from(row - self.areas.exchanges_list.y);
            let index = self.exchange_list_offset + visible_row;
            if index < self.matching_log_indices().len() {
                self.selected_exchange = index;
                self.focus = FocusPane::Logs;
                self.detail_scroll = 0;
            }
            return;
        }
        if rect_contains(self.areas.detail, column, row)
            || rect_contains(self.areas.logs_pane, column, row)
        {
            self.focus = FocusPane::Logs;
        } else if rect_contains(self.areas.endpoints_pane, column, row) {
            self.focus = FocusPane::Endpoints;
        } else if rect_contains(self.areas.server_pane, column, row) {
            self.focus = FocusPane::Server;
        }
    }

    fn handle_mouse_scroll(&mut self, column: u16, row: u16, upwards: bool) {
        if self.detail_expanded || rect_contains(self.areas.detail, column, row) {
            self.focus = FocusPane::Logs;
            if upwards {
                self.detail_scroll = self.detail_scroll.saturating_sub(3);
            } else {
                self.detail_scroll = self.detail_scroll.saturating_add(3);
            }
        } else if rect_contains(self.areas.exchanges_list, column, row) {
            self.focus = FocusPane::Logs;
            if upwards {
                self.navigate_up();
            } else {
                self.navigate_down();
            }
        } else if rect_contains(self.areas.endpoints_pane, column, row) {
            self.focus = FocusPane::Endpoints;
            if upwards {
                self.navigate_up();
            } else {
                self.navigate_down();
            }
        } else if rect_contains(self.areas.server_pane, column, row) {
            self.focus = FocusPane::Server;
            if upwards {
                self.output_scroll = self.output_scroll.saturating_add(3);
            } else {
                self.output_scroll = self.output_scroll.saturating_sub(3);
            }
        }
    }

    fn select_tab_at(&mut self, column: u16, row: u16) -> bool {
        let Some((index, _)) = self
            .areas
            .tabs
            .iter()
            .enumerate()
            .find(|(_, area)| rect_contains(**area, column, row))
        else {
            return false;
        };
        self.detail_tab = DetailTab::ALL[index];
        self.detail_scroll = 0;
        true
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.search_mode = false,
            KeyCode::Backspace => {
                if self.search_cursor > 0 {
                    let start = byte_index(&self.search_query, self.search_cursor - 1);
                    let end = byte_index(&self.search_query, self.search_cursor);
                    self.search_query.replace_range(start..end, "");
                    self.search_cursor -= 1;
                    self.filter_endpoints();
                }
            }
            KeyCode::Delete => {
                if self.search_cursor < self.search_query.chars().count() {
                    let start = byte_index(&self.search_query, self.search_cursor);
                    let end = byte_index(&self.search_query, self.search_cursor + 1);
                    self.search_query.replace_range(start..end, "");
                    self.filter_endpoints();
                }
            }
            KeyCode::Left => self.search_cursor = self.search_cursor.saturating_sub(1),
            KeyCode::Right => {
                self.search_cursor =
                    (self.search_cursor + 1).min(self.search_query.chars().count());
            }
            KeyCode::Home => self.search_cursor = 0,
            KeyCode::End => self.search_cursor = self.search_query.chars().count(),
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_cursor = 0;
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_cursor = self.search_query.chars().count();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_query.clear();
                self.search_cursor = 0;
                self.filter_endpoints();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let index = byte_index(&self.search_query, self.search_cursor);
                self.search_query.insert(index, character);
                self.search_cursor += 1;
                self.filter_endpoints();
            }
            _ => {}
        }
        Action::Continue
    }

    fn filter_endpoints(&mut self) {
        let query = self.search_query.to_lowercase();
        self.filtered = self
            .endpoints
            .iter()
            .enumerate()
            .filter(|(_, endpoint)| {
                query.is_empty()
                    || format!("{} {}", endpoint.method, endpoint.path)
                        .to_lowercase()
                        .contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected_endpoint = 0;
        self.endpoint_changed();
    }

    fn navigate_up(&mut self) {
        match self.focus {
            FocusPane::Endpoints => {
                self.selected_endpoint = self.selected_endpoint.saturating_sub(1);
                self.endpoint_changed();
            }
            FocusPane::Logs => {
                self.selected_exchange = self.selected_exchange.saturating_sub(1);
                self.detail_scroll = 0;
            }
            FocusPane::Server => self.output_scroll = self.output_scroll.saturating_add(1),
        }
    }

    fn navigate_down(&mut self) {
        match self.focus {
            FocusPane::Endpoints => {
                if self.selected_endpoint + 1 < self.filtered.len() {
                    self.selected_endpoint += 1;
                    self.endpoint_changed();
                }
            }
            FocusPane::Logs => {
                let last = self.matching_log_indices().len().saturating_sub(1);
                self.selected_exchange = (self.selected_exchange + 1).min(last);
                self.detail_scroll = 0;
            }
            FocusPane::Server => self.output_scroll = self.output_scroll.saturating_sub(1),
        }
    }
}

pub fn run(
    endpoints: Vec<Endpoint>,
    server: &mut CaptureServer,
    output_rx: Receiver<String>,
    logs_rx: Receiver<LogEntry>,
) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        return Err(error);
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
            return Err(error);
        }
    };

    let result = run_loop(
        &mut terminal,
        App::new(endpoints),
        server,
        output_rx,
        logs_rx,
    );

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut app: App,
    server: &mut CaptureServer,
    output_rx: Receiver<String>,
    logs_rx: Receiver<LogEntry>,
) -> io::Result<()> {
    loop {
        drain_messages(&mut app, &output_rx, &logs_rx);
        terminal.draw(|frame| draw(frame, &mut app))?;

        if !event::poll(Duration::from_millis(75))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match app.handle_key(key) {
                Action::Continue => {}
                Action::Quit => return Ok(()),
                Action::Restart => match server.restart() {
                    Ok(address) => {
                        app.notice = Some(format!("Capture server restarted on {address}"));
                    }
                    Err(error) => app.notice = Some(format!("Restart failed: {error}")),
                },
            },
            Event::Mouse(mouse) => app.handle_mouse(mouse),
            _ => {}
        }
    }
}

fn drain_messages(app: &mut App, output_rx: &Receiver<String>, logs_rx: &Receiver<LogEntry>) {
    while let Ok(line) = output_rx.try_recv() {
        app.push_output(line);
    }
    while let Ok(entry) = logs_rx.try_recv() {
        app.push_log(entry);
    }
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    app.areas = UiAreas::default();
    if area.width < 60 || area.height < 12 {
        frame.render_widget(
            Paragraph::new("LazyAPI needs a terminal of at least 60x12")
                .style(Style::default().fg(Color::Yellow)),
            area,
        );
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(45),
            Constraint::Percentage(25),
        ])
        .split(rows[0]);

    app.areas.endpoints_pane = panes[0];
    app.areas.logs_pane = panes[1];
    app.areas.server_pane = panes[2];

    render_endpoints(frame, app, panes[0]);
    render_logs(frame, app, panes[1]);
    render_server(frame, app, panes[2]);
    render_status(frame, app, rows[1]);
    render_hint(frame, app, rows[2]);

    if app.detail_expanded {
        app.areas.detail = area;
        app.areas.tabs = tab_hitboxes(area);
        app.areas.close_detail = close_hitbox(area);
        render_expanded_detail(frame, app, area);
    }
    if app.show_help {
        render_help(frame, area);
    }
}

fn panel_block<'a>(title: String, focused: bool) -> Block<'a> {
    let border = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title)
}

fn render_endpoints(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let title = if app.search_mode || !app.search_query.is_empty() {
        format!(
            " Endpoints ({}/{}) /{} ",
            app.filtered.len(),
            app.endpoints.len(),
            app.search_query
        )
    } else {
        format!(" Endpoints ({}) ", app.filtered.len())
    };
    let items: Vec<_> = app
        .filtered
        .iter()
        .enumerate()
        .map(|(number, index)| {
            let endpoint = &app.endpoints[*index];
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>3}. ", number + 1),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:<7}", endpoint.method),
                    Style::default()
                        .fg(method_color(&endpoint.method))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(endpoint.path.clone()),
            ]))
        })
        .collect();

    let block = panel_block(title, app.focus == FocusPane::Endpoints);
    app.areas.endpoints_list = block.inner(area);
    let list = List::new(items)
        .block(block)
        .highlight_symbol(" > ")
        .highlight_style(selected_style());
    let mut state = ListState::default();
    state.select((!app.filtered.is_empty()).then_some(app.selected_endpoint));
    frame.render_stateful_widget(list, area, &mut state);
    app.endpoint_list_offset = state.offset();
}

fn render_logs(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let endpoint_path = app
        .selected_endpoint()
        .map(|endpoint| endpoint.path.as_str())
        .unwrap_or("none");
    let indices = app.matching_log_indices();
    let block = panel_block(
        format!(" Requests: {endpoint_path} ({}) ", indices.len()),
        app.focus == FocusPane::Logs,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if indices.is_empty() {
        app.areas.detail = inner;
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("No matching requests yet", dim_style()),
                Line::raw(""),
                Line::styled("Send traffic to the capture address.", dim_style()),
            ]),
            inner,
        );
        return;
    }

    let list_height = if inner.height >= 12 {
        (inner.height / 3).clamp(4, 7)
    } else {
        inner.height.min(3)
    };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(list_height), Constraint::Min(1)])
        .split(inner);
    app.areas.exchanges_list = sections[0];
    app.areas.detail = sections[1];
    app.areas.tabs = tab_hitboxes(sections[1]);

    let items: Vec<_> = indices
        .iter()
        .filter_map(|index| app.logs.get(*index))
        .map(|entry| ListItem::new(exchange_summary(entry, sections[0].width as usize)))
        .collect();
    let list = List::new(items)
        .highlight_symbol(" > ")
        .highlight_style(selected_style());
    let mut state = ListState::default();
    state.select(Some(app.selected_exchange.min(indices.len() - 1)));
    frame.render_stateful_widget(list, sections[0], &mut state);
    app.exchange_list_offset = state.offset();

    if let Some(entry) = app.selected_log() {
        render_exchange_detail(frame, app, entry, sections[1], false);
    }
}

fn render_expanded_detail(frame: &mut Frame<'_>, app: &App, area: Rect) {
    frame.render_widget(Clear, area);
    if let Some(entry) = app.selected_log() {
        render_exchange_detail(frame, app, entry, area, true);
    } else {
        frame.render_widget(
            Paragraph::new("No exchange selected").block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            ),
            area,
        );
    }
}

fn render_exchange_detail(
    frame: &mut Frame<'_>,
    app: &App,
    entry: &LogEntry,
    area: Rect,
    expanded: bool,
) {
    let border_style = if expanded {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let borders = if expanded { Borders::ALL } else { Borders::TOP };
    let block = Block::default()
        .borders(borders)
        .border_style(border_style)
        .title(detail_tabs(app.detail_tab, expanded));
    let inner_height = block.inner(area).height as usize;
    let lines = detail_lines(entry, app.detail_tab, app.pretty_bodies);
    let max_scroll = lines.len().saturating_sub(inner_height);
    let scroll = app.detail_scroll.min(max_scroll) as u16;
    let mut paragraph = Paragraph::new(lines).block(block).scroll((scroll, 0));
    if app.wrap_bodies {
        paragraph = paragraph.wrap(Wrap { trim: false });
    }
    frame.render_widget(paragraph, area);
}

fn detail_tabs(active: DetailTab, expanded: bool) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for tab in DetailTab::ALL {
        let style = if tab == active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            dim_style()
        };
        spans.push(Span::styled(format!(" {} ", tab.label()), style));
        spans.push(Span::raw(" "));
    }
    if expanded {
        spans.push(Span::styled("[e/Esc close]", dim_style()));
    }
    Line::from(spans)
}

fn detail_lines(entry: &LogEntry, tab: DetailTab, pretty: bool) -> Vec<Line<'static>> {
    match tab {
        DetailTab::Request => request_lines(entry, pretty),
        DetailTab::Response => response_lines(entry, pretty),
        DetailTab::Headers => header_detail_lines(entry),
    }
}

fn request_lines(entry: &LogEntry, pretty: bool) -> Vec<Line<'static>> {
    let target = entry.query.as_ref().map_or_else(
        || entry.path.clone(),
        |query| format!("{}?{query}", entry.path),
    );
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!(" {} ", entry.method),
            Style::default()
                .fg(Color::Black)
                .bg(method_color(&entry.method))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(target, Style::default().add_modifier(Modifier::BOLD)),
    ])];
    lines.push(metadata_line(&entry.request));

    if let Some(query) = &entry.query {
        lines.push(Line::raw(""));
        lines.push(section_heading("Query parameters"));
        let parameters: Vec<_> = form_urlencoded::parse(query.as_bytes()).collect();
        if parameters.is_empty() {
            lines.push(Line::styled("  none", dim_style()));
        } else {
            for (name, value) in parameters {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {name}"), Style::default().fg(Color::Cyan)),
                    Span::styled(" = ", dim_style()),
                    Span::raw(value.into_owned()),
                ]));
            }
        }
    }

    lines.push(Line::raw(""));
    lines.push(section_heading("Body"));
    lines.extend(body_lines(&entry.request, pretty));
    lines
}

fn response_lines(entry: &LogEntry, pretty: bool) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!(" {} ", entry.status),
            Style::default()
                .fg(Color::Black)
                .bg(status_color(entry.status))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{}ms", entry.latency_ms),
            Style::default().fg(latency_color(entry.latency_ms)),
        ),
    ])];
    lines.push(metadata_line(&entry.response));
    lines.push(Line::raw(""));
    lines.push(section_heading("Body"));
    lines.extend(body_lines(&entry.response, pretty));
    lines
}

fn header_detail_lines(entry: &LogEntry) -> Vec<Line<'static>> {
    let mut lines = vec![section_heading(&format!(
        "Request headers ({})",
        entry.request.headers.len()
    ))];
    lines.extend(header_lines(&entry.request.headers));
    lines.push(Line::raw(""));
    lines.push(section_heading(&format!(
        "Response headers ({})",
        entry.response.headers.len()
    )));
    lines.extend(header_lines(&entry.response.headers));
    lines
}

fn header_lines(headers: &BTreeMap<String, String>) -> Vec<Line<'static>> {
    if headers.is_empty() {
        return vec![Line::styled("  none", dim_style())];
    }
    headers
        .iter()
        .map(|(name, value)| {
            Line::from(vec![
                Span::styled(format!("{name}: "), Style::default().fg(Color::Cyan)),
                Span::raw(value.clone()),
            ])
        })
        .collect()
}

fn body_lines(part: &ExchangePart, pretty: bool) -> Vec<Line<'static>> {
    let content_type = header_value(&part.headers, "content-type").unwrap_or_default();
    let mut lines = Vec::new();
    if part.truncated {
        lines.push(Line::styled(
            format!(
                "! Captured preview truncated; original body is at least {}",
                human_size(part.size)
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if part.body.trim().is_empty() && part.size == 0 {
        lines.push(Line::styled("No body", dim_style()));
        return lines;
    }
    if is_binary_content_type(&content_type) {
        lines.push(Line::styled(
            format!(
                "Binary body: {} ({})",
                content_type_or_unknown(&content_type),
                human_size(part.size)
            ),
            Style::default().fg(Color::Magenta),
        ));
        return lines;
    }

    if pretty
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&part.body)
        && let Ok(formatted) = serde_json::to_string_pretty(&value)
    {
        lines.extend(formatted.lines().map(highlight_json_line));
    } else {
        lines.extend(part.body.lines().map(|line| Line::raw(line.to_string())));
    }
    if lines.is_empty() {
        lines.push(Line::styled("No body", dim_style()));
    }
    lines
}

fn metadata_line(part: &ExchangePart) -> Line<'static> {
    let content_type = header_value(&part.headers, "content-type").unwrap_or_default();
    Line::from(vec![
        Span::styled("Content-Type: ", dim_style()),
        Span::raw(content_type_or_unknown(&content_type)),
        Span::styled("  Size: ", dim_style()),
        Span::raw(human_size(part.size)),
    ])
}

fn section_heading(title: &str) -> Line<'static> {
    Line::styled(
        title.to_string(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )
}

fn exchange_summary(entry: &LogEntry, width: usize) -> Line<'static> {
    let time = DateTime::parse_from_rfc3339(&entry.timestamp)
        .map(|timestamp| timestamp.format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| truncate(&entry.timestamp, 8));
    let target = entry.query.as_ref().map_or_else(
        || entry.path.clone(),
        |query| format!("{}?{query}", entry.path),
    );
    let fixed_width = 8 + 1 + 6 + 1 + 3 + 1 + 7 + 1;
    let path = truncate(&target, width.saturating_sub(fixed_width).max(1));
    Line::from(vec![
        Span::styled(time, dim_style()),
        Span::raw(" "),
        Span::styled(
            format!("{:<6}", entry.method),
            Style::default()
                .fg(method_color(&entry.method))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{:>3}", entry.status),
            Style::default().fg(status_color(entry.status)),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{:>6} ", format!("{}ms", entry.latency_ms)),
            Style::default().fg(latency_color(entry.latency_ms)),
        ),
        Span::raw(path),
    ])
}

fn render_server(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut lines: Vec<_> = app
        .output
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let color = if line.contains(" -> ") {
                Color::Cyan
            } else {
                Color::Gray
            };
            Line::styled(line.clone(), Style::default().fg(color))
        })
        .collect();
    if lines.is_empty() {
        lines.push(Line::styled("Waiting for server output...", dim_style()));
    }

    let content_height = area.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(content_height);
    let offset_from_bottom = app.output_scroll.min(max_scroll);
    let scroll = max_scroll.saturating_sub(offset_from_bottom) as u16;
    let paragraph = Paragraph::new(lines)
        .block(panel_block(
            format!(" Server ({}) ", app.output.len()),
            app.focus == FocusPane::Server,
        ))
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn render_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let selected = app.selected_endpoint().map_or_else(
        || "No endpoint selected".to_string(),
        |endpoint| format!("Selected: {} {}", endpoint.method, endpoint.path),
    );
    let detail = if app.focus == FocusPane::Logs {
        format!(
            " | {} | {} | {}",
            app.detail_tab.label(),
            if app.pretty_bodies { "pretty" } else { "raw" },
            if app.wrap_bodies { "wrap" } else { "clip" }
        )
    } else {
        String::new()
    };
    let status = app
        .notice
        .clone()
        .unwrap_or_else(|| format!("{selected}{detail}"));
    frame.render_widget(
        Paragraph::new(truncate(&status, area.width as usize))
            .style(Style::default().fg(Color::Black).bg(Color::Cyan)),
        area,
    );
}

fn render_hint(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let hint = if app.search_mode {
        "Search: type to filter | Enter/Esc: finish | Ctrl+U: clear"
    } else if app.focus == FocusPane::Logs {
        "Click/Up/Down: exchanges | Click/Left/Right: tabs | Wheel/PgUp/PgDn: body | e: expand"
    } else {
        "Mouse: click/scroll | ?: help | Tab: panes | arrows/jk: navigate | /: search | q: quit"
    };
    frame.render_widget(
        Paragraph::new(truncate(hint, area.width as usize)).style(dim_style()),
        area,
    );
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(78, 94, area);
    frame.render_widget(Clear, popup);
    let help = Paragraph::new(
        "LazyAPI\n\n\
         Navigation\n\
         Up/k, Down/j    Navigate endpoints, exchanges, or server output\n\
         Tab, Shift+Tab  Switch panes\n\
         Enter/Space     Focus the selected endpoint's requests\n\n\
         Exchange inspector\n\
         Left/Right      Request, Response, and Headers tabs\n\
         PageUp/PageDown Scroll the selected body\n\
         p               Toggle pretty and raw bodies\n\
         w               Toggle line wrapping\n\
         e               Expand or close the detail view\n\n\
         Mouse\n\
         Click           Focus panes, select rows, and switch detail tabs\n\
         Wheel           Navigate lists or scroll the pane under the pointer\n\
         Horizontal wheel Switch request/response/header tabs\n\n\
         Search and actions\n\
         /               Filter endpoints by method or path\n\
         r               Restart capture server\n\
         q / Ctrl+C      Quit\n\n\
         Press any key to return",
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(" Help "),
    )
    .wrap(Wrap { trim: false });
    frame.render_widget(help, popup);
}

fn highlight_json_line(line: &str) -> Line<'static> {
    let characters: Vec<char> = line.chars().collect();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let start = index;
        match characters[index] {
            '"' => {
                index += 1;
                let mut escaped = false;
                while index < characters.len() {
                    let character = characters[index];
                    index += 1;
                    if character == '"' && !escaped {
                        break;
                    }
                    escaped = character == '\\' && !escaped;
                    if character != '\\' {
                        escaped = false;
                    }
                }
                let mut lookahead = index;
                while lookahead < characters.len() && characters[lookahead].is_whitespace() {
                    lookahead += 1;
                }
                let color = if characters.get(lookahead) == Some(&':') {
                    Color::Cyan
                } else {
                    Color::Green
                };
                spans.push(Span::styled(
                    characters[start..index].iter().collect::<String>(),
                    Style::default().fg(color),
                ));
            }
            character if character.is_ascii_digit() || character == '-' => {
                index += 1;
                while index < characters.len()
                    && (characters[index].is_ascii_digit()
                        || matches!(characters[index], '.' | 'e' | 'E' | '+' | '-'))
                {
                    index += 1;
                }
                spans.push(Span::styled(
                    characters[start..index].iter().collect::<String>(),
                    Style::default().fg(Color::Magenta),
                ));
            }
            character if character.is_ascii_alphabetic() => {
                index += 1;
                while index < characters.len() && characters[index].is_ascii_alphabetic() {
                    index += 1;
                }
                let token: String = characters[start..index].iter().collect();
                let color = if token == "null" {
                    Color::DarkGray
                } else {
                    Color::Yellow
                };
                spans.push(Span::styled(token, Style::default().fg(color)));
            }
            character if character.is_whitespace() => {
                index += 1;
                while index < characters.len() && characters[index].is_whitespace() {
                    index += 1;
                }
                spans.push(Span::raw(
                    characters[start..index].iter().collect::<String>(),
                ));
            }
            _ => {
                index += 1;
                spans.push(Span::styled(
                    characters[start..index].iter().collect::<String>(),
                    dim_style(),
                ));
            }
        }
    }
    Line::from(spans)
}

fn header_value(headers: &BTreeMap<String, String>, wanted: &str) -> Option<String> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
        .map(|(_, value)| value.clone())
}

fn is_binary_content_type(content_type: &str) -> bool {
    let content_type = content_type.to_ascii_lowercase();
    if content_type.is_empty()
        || content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.contains("yaml")
        || content_type.contains("javascript")
        || content_type.contains("x-www-form-urlencoded")
    {
        return false;
    }
    content_type.starts_with("image/")
        || content_type.starts_with("audio/")
        || content_type.starts_with("video/")
        || content_type.contains("octet-stream")
        || content_type.contains("pdf")
        || content_type.contains("zip")
}

fn content_type_or_unknown(content_type: &str) -> String {
    if content_type.is_empty() {
        "unknown".into()
    } else {
        content_type.into()
    }
}

fn human_size(size: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let size = size as f64;
    if size >= MIB {
        format!("{:.1} MiB", size / MIB)
    } else if size >= KIB {
        format!("{:.1} KiB", size / KIB)
    } else {
        format!("{} B", size as usize)
    }
}

fn method_color(method: &str) -> Color {
    match method {
        "GET" => Color::Green,
        "POST" => Color::Blue,
        "PUT" => Color::Yellow,
        "PATCH" => Color::Magenta,
        "DELETE" => Color::Red,
        _ => Color::Cyan,
    }
}

fn status_color(status: u16) -> Color {
    match status {
        200..=299 => Color::Green,
        300..=399 => Color::Cyan,
        400..=499 => Color::Yellow,
        _ => Color::Red,
    }
}

fn latency_color(latency_ms: u128) -> Color {
    match latency_ms {
        0..=99 => Color::Green,
        100..=499 => Color::Yellow,
        _ => Color::Red,
    }
}

fn selected_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn tab_hitboxes(area: Rect) -> [Rect; 3] {
    let mut tabs = [Rect::default(); 3];
    let mut x = area.x.saturating_add(2);
    for (index, tab) in DetailTab::ALL.iter().enumerate() {
        let width = tab.label().chars().count() as u16 + 2;
        tabs[index] = title_hitbox(area, x, width);
        x = x.saturating_add(width + 1);
    }
    tabs
}

fn close_hitbox(area: Rect) -> Rect {
    let tabs_width: u16 = DetailTab::ALL
        .iter()
        .map(|tab| tab.label().chars().count() as u16 + 3)
        .sum();
    title_hitbox(area, area.x.saturating_add(2 + tabs_width), 13)
}

fn title_hitbox(area: Rect, x: u16, requested_width: u16) -> Rect {
    let right = area.x.saturating_add(area.width);
    let width = requested_width.min(right.saturating_sub(x));
    Rect::new(x, area.y, width, u16::from(width > 0))
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    area.width > 0
        && area.height > 0
        && column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn truncate(value: &str, width: usize) -> String {
    let count = value.chars().count();
    if count <= width {
        return value.to_string();
    }
    if width <= 3 {
        return value.chars().take(width).collect();
    }
    let mut result: String = value.chars().take(width - 3).collect();
    result.push_str("...");
    result
}

fn byte_index(value: &str, character_index: usize) -> usize {
    value
        .char_indices()
        .nth(character_index)
        .map_or(value.len(), |(index, _)| index)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;

    use super::{
        App, DetailTab, FocusPane, body_lines, human_size, is_binary_content_type, truncate,
    };
    use crate::model::{Endpoint, ExchangePart, LogEntry};

    fn endpoint(method: &str, path: &str) -> Endpoint {
        Endpoint {
            method: method.into(),
            path: path.into(),
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn line_text(lines: &[ratatui::text::Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn filters_endpoints_and_edits_unicode_search() {
        let mut app = App::new(vec![
            endpoint("GET", "/cafe"),
            endpoint("POST", "/cafeteria"),
        ]);
        app.focus = FocusPane::Endpoints;
        app.search_mode = true;
        app.handle_key(KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE));
        assert_eq!(app.search_query, "é");
        assert!(app.filtered.is_empty());
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.search_query, "");
        assert_eq!(app.filtered.len(), 2);
    }

    #[test]
    fn selected_logs_match_path_templates_and_follow_latest() {
        let mut app = App::new(vec![endpoint("GET", "/users/{id}")]);
        app.push_log(LogEntry {
            method: "GET".into(),
            path: "/users/41".into(),
            ..LogEntry::default()
        });
        app.push_log(LogEntry {
            method: "POST".into(),
            path: "/users/42".into(),
            ..LogEntry::default()
        });
        app.push_log(LogEntry {
            method: "GET".into(),
            path: "/users/42".into(),
            ..LogEntry::default()
        });
        assert_eq!(app.matching_log_indices().len(), 2);
        assert_eq!(app.selected_exchange, 1);
        assert_eq!(app.selected_log().unwrap().path, "/users/42");
    }

    #[test]
    fn log_controls_switch_tabs_and_body_modes() {
        let mut app = App::new(vec![endpoint("GET", "/users")]);
        app.focus = FocusPane::Logs;
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.detail_tab, DetailTab::Response);
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(!app.pretty_bodies);
        assert!(app.wrap_bodies);
        assert!(app.detail_expanded);
    }

    #[test]
    fn mouse_clicks_select_endpoints_exchanges_and_tabs() {
        let mut app = App::new(vec![endpoint("GET", "/users"), endpoint("POST", "/users")]);
        app.areas.endpoints_list = Rect::new(1, 1, 20, 5);
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 2));
        assert_eq!(app.selected_endpoint, 1);
        assert_eq!(app.focus, FocusPane::Endpoints);

        app.logs = vec![
            LogEntry {
                method: "POST".into(),
                path: "/users".into(),
                ..LogEntry::default()
            },
            LogEntry {
                method: "POST".into(),
                path: "/users".into(),
                ..LogEntry::default()
            },
        ];
        app.areas.exchanges_list = Rect::new(25, 2, 30, 5);
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 27, 3));
        assert_eq!(app.selected_exchange, 1);
        assert_eq!(app.focus, FocusPane::Logs);

        app.areas.tabs = [
            Rect::new(25, 8, 9, 1),
            Rect::new(35, 8, 10, 1),
            Rect::new(46, 8, 9, 1),
        ];
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 48, 8));
        assert_eq!(app.detail_tab, DetailTab::Headers);
    }

    #[test]
    fn mouse_wheel_targets_the_pane_under_the_pointer() {
        let mut app = App::new(vec![endpoint("GET", "/users")]);
        app.areas.detail = Rect::new(20, 8, 40, 12);
        app.detail_scroll = 6;
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 30, 10));
        assert_eq!(app.detail_scroll, 3);
        assert_eq!(app.focus, FocusPane::Logs);
        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 30, 10));
        assert_eq!(app.detail_scroll, 6);

        app.output_scroll = 2;
        app.areas.server_pane = Rect::new(60, 0, 20, 20);
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 70, 5));
        assert_eq!(app.output_scroll, 5);
        assert_eq!(app.focus, FocusPane::Server);
    }

    #[test]
    fn expanded_detail_can_be_closed_with_mouse() {
        let mut app = App::new(vec![endpoint("GET", "/users")]);
        app.detail_expanded = true;
        app.areas.close_detail = Rect::new(34, 0, 13, 1);
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 36, 0));
        assert!(!app.detail_expanded);
    }

    #[test]
    fn json_bodies_are_pretty_printed_and_binary_bodies_are_described() {
        let json = ExchangePart {
            body: r#"{"name":"Ada","active":true}"#.into(),
            size: 28,
            ..ExchangePart::default()
        };
        let text = line_text(&body_lines(&json, true));
        assert!(text.contains("  \"name\": \"Ada\""));
        assert!(text.contains("  \"active\": true"));

        let binary = ExchangePart {
            headers: BTreeMap::from([("Content-Type".into(), "image/png".into())]),
            body: "not displayed".into(),
            size: 4_096,
            ..ExchangePart::default()
        };
        let text = line_text(&body_lines(&binary, true));
        assert!(text.contains("Binary body: image/png (4.0 KiB)"));
        assert!(is_binary_content_type("application/pdf"));
    }

    #[test]
    fn truncation_and_size_helpers_are_clear() {
        let part = ExchangePart {
            body: "partial".into(),
            size: 2_097_153,
            truncated: true,
            ..ExchangePart::default()
        };
        assert!(line_text(&body_lines(&part, false)).contains("truncated"));
        assert_eq!(human_size(2_097_152), "2.0 MiB");
        assert_eq!(truncate("abcdefghij", 7), "abcd...");
    }
}
