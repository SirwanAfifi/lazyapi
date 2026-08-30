use std::{
    collections::VecDeque,
    io::{self, Stdout},
    path::Path,
    sync::mpsc::Receiver,
    thread,
    time::{Duration, Instant},
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
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use url::form_urlencoded;

use crate::{
    model::{ContractViolation, Endpoint, ExchangePart, LogEntry},
    server::CaptureServer,
    session::{self, SessionRecorder},
};

const SLOW_REQUEST_MS: u128 = 500;
const DISPLAY_LOG_LIMIT: usize = 500;
const DISPLAY_LOG_BYTE_LIMIT: usize = 64 * 1024 * 1024;
const LOG_DRAIN_BATCH: usize = 64;
const NOTICE_TTL: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LayoutMode {
    #[default]
    TooSmall,
    Narrow,
    Compact,
    Wide,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FocusPane {
    Endpoints,
    Logs,
    Server,
}

impl FocusPane {
    const ALL: [Self; 3] = [Self::Endpoints, Self::Logs, Self::Server];

    fn label(self) -> &'static str {
        match self {
            Self::Endpoints => "Endpoints",
            Self::Logs => "Requests",
            Self::Server => "Server",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum DetailTab {
    #[default]
    Request,
    Response,
    Headers,
    Contract,
    Curl,
}

impl DetailTab {
    const ALL: [Self; 5] = [
        Self::Request,
        Self::Response,
        Self::Headers,
        Self::Contract,
        Self::Curl,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Request => "Request",
            Self::Response => "Response",
            Self::Headers => "Headers",
            Self::Contract => "Contract",
            Self::Curl => "cURL",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Request => Self::Response,
            Self::Response => Self::Headers,
            Self::Headers => Self::Contract,
            Self::Contract => Self::Curl,
            Self::Curl => Self::Request,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Request => Self::Curl,
            Self::Response => Self::Request,
            Self::Headers => Self::Response,
            Self::Contract => Self::Headers,
            Self::Curl => Self::Contract,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum TrafficView {
    #[default]
    Selected,
    All,
    Unmatched,
    Errors,
    Slow,
}

impl TrafficView {
    const ALL: [Self; 5] = [
        Self::Selected,
        Self::All,
        Self::Errors,
        Self::Slow,
        Self::Unmatched,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Selected => "Selected",
            Self::All => "All",
            Self::Unmatched => "Unmatched",
            Self::Errors => "Errors",
            Self::Slow => "Slow",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Selected => Self::All,
            Self::All => Self::Errors,
            Self::Errors => Self::Slow,
            Self::Slow => Self::Unmatched,
            Self::Unmatched => Self::Selected,
        }
    }

    fn shortcut(self) -> char {
        match self {
            Self::Selected => '1',
            Self::All => '2',
            Self::Errors => '3',
            Self::Slow => '4',
            Self::Unmatched => '5',
        }
    }

    fn short_label(self) -> &'static str {
        match self {
            Self::Selected => "Sel",
            Self::All => "All",
            Self::Errors => "Err",
            Self::Slow => "Slow",
            Self::Unmatched => "Miss",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchTarget {
    Endpoints,
    Traffic,
}

#[derive(Clone, Debug)]
struct SearchSnapshot {
    query: String,
    selected_endpoint: usize,
    selected_exchange: usize,
    follow_live: bool,
    history_len: usize,
    detail_tab: DetailTab,
    detail_scroll: usize,
    detail_expanded: bool,
}

enum Action {
    Continue,
    Restart,
    Replay(Box<LogEntry>),
    Quit,
}

#[derive(Clone, Copy, Debug, Default)]
struct UiAreas {
    layout_mode: LayoutMode,
    workspace: Rect,
    pane_tabs: [Rect; 3],
    endpoints_pane: Rect,
    endpoints_search: Rect,
    endpoints_list: Rect,
    logs_pane: Rect,
    traffic_filters: [Rect; 5],
    traffic_search: Rect,
    exchanges_list: Rect,
    detail: Rect,
    server_pane: Rect,
    tabs: [Rect; 5],
    close_detail: Rect,
    replay_confirm: Rect,
    replay_cancel: Rect,
}

struct App {
    endpoints: Vec<Endpoint>,
    filtered: Vec<usize>,
    selected_endpoint: usize,
    endpoint_list_offset: usize,
    focus: FocusPane,
    last_workspace_focus: FocusPane,
    show_server: bool,
    search_target: Option<SearchTarget>,
    search_snapshot: Option<SearchSnapshot>,
    endpoint_search_query: String,
    endpoint_search_cursor: usize,
    recorder: Option<SessionRecorder>,
    history_len: usize,
    logs: VecDeque<LogEntry>,
    display_bytes: usize,
    display_byte_limit: usize,
    selected_exchange: usize,
    exchange_list_offset: usize,
    traffic_view: TrafficView,
    traffic_search_query: String,
    traffic_search_cursor: usize,
    follow_live: bool,
    detail_tab: DetailTab,
    detail_scroll: usize,
    detail_max_scroll: usize,
    pretty_bodies: bool,
    wrap_bodies: bool,
    detail_expanded: bool,
    output: Vec<String>,
    output_scroll: usize,
    output_max_scroll: usize,
    show_help: bool,
    help_scroll: usize,
    help_max_scroll: usize,
    replay_confirmation: Option<Box<LogEntry>>,
    notice: Option<String>,
    notice_expires_at: Option<Instant>,
    areas: UiAreas,
}

fn retained_entry_bytes(entry: &LogEntry) -> usize {
    fn part_bytes(part: &ExchangePart) -> usize {
        std::mem::size_of::<ExchangePart>()
            + part.body.len()
            + part
                .headers
                .iter()
                .map(|(name, value)| name.len() + value.len())
                .sum::<usize>()
            + part
                .header_values
                .iter()
                .map(|header| header.name.len() + header.value.len())
                .sum::<usize>()
    }

    std::mem::size_of::<LogEntry>()
        + entry.method.len()
        + entry.path.len()
        + entry.query.as_deref().map_or(0, str::len)
        + entry.timestamp.len()
        + part_bytes(&entry.request)
        + part_bytes(&entry.response)
        + entry
            .contract
            .matched_endpoint
            .as_deref()
            .map_or(0, str::len)
        + entry
            .contract
            .violations
            .iter()
            .map(|violation| {
                violation.code.len() + violation.location.len() + violation.message.len()
            })
            .sum::<usize>()
}

fn mark_redacted_contract_inconclusive(entry: &mut LogEntry) {
    entry.contract.checked = true;
    entry.contract.inconclusive = true;
    if !entry
        .contract
        .violations
        .iter()
        .any(|violation| violation.code == "validation_inconclusive")
    {
        entry.contract.violations.push(ContractViolation::new(
            "validation_inconclusive",
            "contract.validation",
            "current-spec revalidation was skipped because the saved capture contains masked data",
        ));
    }
}

impl App {
    #[cfg(test)]
    fn new(endpoints: Vec<Endpoint>) -> Self {
        Self::with_logs(endpoints, Vec::new()).expect("could not create test app")
    }

    #[cfg(test)]
    fn with_logs(endpoints: Vec<Endpoint>, logs: Vec<LogEntry>) -> io::Result<Self> {
        Self::with_recorder(endpoints, logs, None)
    }

    fn with_recorder(
        endpoints: Vec<Endpoint>,
        logs: Vec<LogEntry>,
        recorder: Option<SessionRecorder>,
    ) -> io::Result<Self> {
        let filtered = (0..endpoints.len()).collect();
        let mut app = Self {
            endpoints,
            filtered,
            selected_endpoint: 0,
            endpoint_list_offset: 0,
            focus: FocusPane::Endpoints,
            last_workspace_focus: FocusPane::Endpoints,
            show_server: false,
            search_target: None,
            search_snapshot: None,
            endpoint_search_query: String::new(),
            endpoint_search_cursor: 0,
            recorder,
            history_len: 0,
            logs: VecDeque::new(),
            display_bytes: 0,
            display_byte_limit: DISPLAY_LOG_BYTE_LIMIT,
            selected_exchange: 0,
            exchange_list_offset: 0,
            traffic_view: TrafficView::Selected,
            traffic_search_query: String::new(),
            traffic_search_cursor: 0,
            follow_live: true,
            detail_tab: DetailTab::Request,
            detail_scroll: 0,
            detail_max_scroll: 0,
            pretty_bodies: true,
            wrap_bodies: false,
            detail_expanded: false,
            output: Vec::new(),
            output_scroll: 0,
            output_max_scroll: 0,
            show_help: false,
            help_scroll: 0,
            help_max_scroll: 0,
            replay_confirmation: None,
            notice: None,
            notice_expires_at: None,
            areas: UiAreas::default(),
        };
        for entry in logs {
            app.push_loaded_log(entry)?;
        }
        app.select_latest_exchange();
        Ok(app)
    }

    fn selected_endpoint(&self) -> Option<&Endpoint> {
        self.filtered
            .get(self.selected_endpoint)
            .and_then(|index| self.endpoints.get(*index))
    }

    fn log_matches_selected_endpoint(&self, entry: &LogEntry) -> bool {
        self.selected_endpoint()
            .is_some_and(|endpoint| endpoint.matches(&entry.method, &entry.path))
    }

    fn log_matches_any_endpoint(&self, entry: &LogEntry) -> bool {
        self.endpoints
            .iter()
            .any(|endpoint| endpoint.matches(&entry.method, &entry.path))
    }

    fn log_matches_traffic_view(&self, entry: &LogEntry) -> bool {
        match self.traffic_view {
            TrafficView::Selected => self.log_matches_selected_endpoint(entry),
            TrafficView::All => true,
            TrafficView::Unmatched => !self.log_matches_any_endpoint(entry),
            TrafficView::Errors => {
                entry.status >= 400 || (entry.contract.checked && !entry.contract.is_valid())
            }
            TrafficView::Slow => entry.latency_ms >= SLOW_REQUEST_MS,
        }
    }

    fn view_log_indices(&self) -> Vec<usize> {
        self.logs
            .iter()
            .enumerate()
            .filter(|(_, entry)| self.log_matches_traffic_view(entry))
            .map(|(index, _)| index)
            .collect()
    }

    fn visible_log_indices(&self) -> Vec<usize> {
        let query = self.traffic_search_query.trim().to_lowercase();
        self.logs
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                self.log_matches_traffic_view(entry)
                    && (query.is_empty() || log_matches_query(entry, &query))
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_log(&self) -> Option<&LogEntry> {
        let indices = self.visible_log_indices();
        let index = indices.get(self.selected_exchange.min(indices.len().saturating_sub(1)))?;
        self.logs.get(*index)
    }

    fn select_latest_exchange(&mut self) {
        self.selected_exchange = self.visible_log_indices().len().saturating_sub(1);
        self.detail_scroll = 0;
    }

    fn endpoint_changed(&mut self) {
        if self.traffic_view == TrafficView::Selected {
            self.select_latest_exchange();
        }
        self.detail_tab = DetailTab::Request;
        self.detail_expanded = false;
    }

    fn set_focus(&mut self, focus: FocusPane) {
        self.focus = focus;
        if focus != FocusPane::Server {
            if self.areas.layout_mode == LayoutMode::Narrow {
                self.show_server = false;
            }
            self.last_workspace_focus = focus;
        } else if self.areas.layout_mode == LayoutMode::Narrow {
            self.show_server = true;
        }
    }

    fn cycle_focus(&mut self, backwards: bool) {
        let next = match (self.show_server, self.focus, backwards) {
            (false, FocusPane::Endpoints, false) => FocusPane::Logs,
            (false, FocusPane::Logs | FocusPane::Server, false) => FocusPane::Endpoints,
            (false, FocusPane::Endpoints | FocusPane::Server, true) => FocusPane::Logs,
            (false, FocusPane::Logs, true) => FocusPane::Endpoints,
            (true, FocusPane::Endpoints, false) => FocusPane::Logs,
            (true, FocusPane::Logs, false) => FocusPane::Server,
            (true, FocusPane::Server, false) => FocusPane::Endpoints,
            (true, FocusPane::Endpoints, true) => FocusPane::Server,
            (true, FocusPane::Logs, true) => FocusPane::Endpoints,
            (true, FocusPane::Server, true) => FocusPane::Logs,
        };
        self.set_focus(next);
    }

    fn toggle_server(&mut self) {
        if self.show_server {
            self.show_server = false;
            if self.focus == FocusPane::Server {
                self.set_focus(self.last_workspace_focus);
            }
            self.set_notice("Server output hidden");
        } else {
            self.show_server = true;
            self.set_focus(FocusPane::Server);
            self.set_notice("Server output opened");
        }
    }

    fn set_traffic_view(&mut self, view: TrafficView) {
        self.traffic_view = view;
        self.select_latest_exchange();
        self.detail_tab = DetailTab::Request;
        self.set_notice(format!("Traffic view: {}", view.label()));
    }

    fn traffic_view_count(&self, view: TrafficView) -> usize {
        self.logs
            .iter()
            .filter(|entry| match view {
                TrafficView::Selected => self.log_matches_selected_endpoint(entry),
                TrafficView::All => true,
                TrafficView::Unmatched => !self.log_matches_any_endpoint(entry),
                TrafficView::Errors => {
                    entry.status >= 400 || (entry.contract.checked && !entry.contract.is_valid())
                }
                TrafficView::Slow => entry.latency_ms >= SLOW_REQUEST_MS,
            })
            .count()
    }

    fn begin_search(&mut self, target: SearchTarget) {
        if self.search_target.is_some() {
            return;
        }
        let query = match target {
            SearchTarget::Endpoints => &self.endpoint_search_query,
            SearchTarget::Traffic => &self.traffic_search_query,
        };
        self.search_snapshot = Some(SearchSnapshot {
            query: query.clone(),
            selected_endpoint: self.selected_endpoint,
            selected_exchange: self.selected_exchange,
            follow_live: self.follow_live,
            history_len: self.history_len,
            detail_tab: self.detail_tab,
            detail_scroll: self.detail_scroll,
            detail_expanded: self.detail_expanded,
        });
        self.search_target = Some(target);
        match target {
            SearchTarget::Endpoints => {
                self.endpoint_search_cursor = self.endpoint_search_query.chars().count();
            }
            SearchTarget::Traffic => {
                self.traffic_search_cursor = self.traffic_search_query.chars().count();
            }
        }
    }

    fn apply_search(&mut self) {
        self.search_target = None;
        self.search_snapshot = None;
    }

    fn cancel_search(&mut self) {
        let Some(target) = self.search_target.take() else {
            return;
        };
        let Some(snapshot) = self.search_snapshot.take() else {
            return;
        };
        match target {
            SearchTarget::Endpoints => {
                self.endpoint_search_query = snapshot.query;
                self.endpoint_search_cursor = self.endpoint_search_query.chars().count();
                self.filter_endpoints();
            }
            SearchTarget::Traffic => {
                self.traffic_search_query = snapshot.query;
                self.traffic_search_cursor = self.traffic_search_query.chars().count();
                self.select_latest_exchange();
            }
        }
        self.selected_endpoint = snapshot
            .selected_endpoint
            .min(self.filtered.len().saturating_sub(1));
        self.follow_live = snapshot.follow_live;
        if snapshot.follow_live {
            self.select_latest_exchange();
        } else {
            self.selected_exchange = snapshot
                .selected_exchange
                .min(self.visible_log_indices().len().saturating_sub(1));
        }
        self.detail_tab = snapshot.detail_tab;
        self.detail_scroll = if snapshot.follow_live && self.history_len != snapshot.history_len {
            0
        } else {
            snapshot.detail_scroll
        };
        self.detail_expanded = snapshot.detail_expanded;
    }

    fn set_notice(&mut self, message: impl Into<String>) {
        self.notice = Some(message.into());
        self.notice_expires_at = Some(Instant::now() + NOTICE_TTL);
    }

    fn set_sticky_notice(&mut self, message: impl Into<String>) {
        self.notice = Some(message.into());
        self.notice_expires_at = None;
    }

    fn expire_notice_at(&mut self, now: Instant) {
        if self
            .notice_expires_at
            .is_some_and(|deadline| now >= deadline)
        {
            self.notice = None;
            self.notice_expires_at = None;
        }
    }

    fn scroll_detail_up(&mut self, amount: usize) {
        self.detail_scroll = self.detail_scroll.saturating_sub(amount);
    }

    fn scroll_detail_down(&mut self, amount: usize) {
        self.detail_scroll = self
            .detail_scroll
            .saturating_add(amount)
            .min(self.detail_max_scroll);
    }

    fn scroll_output_up(&mut self, amount: usize) {
        self.output_scroll = self
            .output_scroll
            .saturating_add(amount)
            .min(self.output_max_scroll);
    }

    fn scroll_output_down(&mut self, amount: usize) {
        self.output_scroll = self.output_scroll.saturating_sub(amount);
    }

    fn push_output(&mut self, line: String) {
        self.output.push(line);
        if self.output.len() > 1_000 {
            self.output.drain(..self.output.len() - 1_000);
        }
    }

    fn push_log(&mut self, entry: LogEntry) -> io::Result<()> {
        let mut entry = entry;
        if !entry.contract.checked {
            entry.validate_against(&self.endpoints);
        }
        self.retain_log(entry)
    }

    fn push_loaded_log(&mut self, mut entry: LogEntry) -> io::Result<()> {
        if capture_contains_redaction(&entry) {
            mark_redacted_contract_inconclusive(&mut entry);
        } else {
            entry.validate_against(&self.endpoints);
        }
        self.retain_log(entry)
    }

    fn retain_log(&mut self, entry: LogEntry) -> io::Result<()> {
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.push(&entry)?;
        }
        self.history_len += 1;
        let query = self.traffic_search_query.trim().to_lowercase();
        self.display_bytes = self
            .display_bytes
            .saturating_add(retained_entry_bytes(&entry));
        self.logs.push_back(entry);
        while self.logs.len() > DISPLAY_LOG_LIMIT
            || (self.display_bytes > self.display_byte_limit && self.logs.len() > 1)
        {
            let dropped_was_visible = self.logs.front().is_some_and(|entry| {
                self.log_matches_traffic_view(entry)
                    && (query.is_empty() || log_matches_query(entry, &query))
            });
            let dropped = self.logs.pop_front().expect("checked non-empty log window");
            self.display_bytes = self
                .display_bytes
                .saturating_sub(retained_entry_bytes(&dropped));
            if dropped_was_visible && !self.follow_live {
                self.selected_exchange = self.selected_exchange.saturating_sub(1);
            }
        }
        let new_entry_is_visible = self
            .visible_log_indices()
            .last()
            .is_some_and(|index| *index + 1 == self.logs.len());
        if self.follow_live && new_entry_is_visible {
            self.select_latest_exchange();
        }
        self.selected_exchange = self
            .selected_exchange
            .min(self.visible_log_indices().len().saturating_sub(1));
        Ok(())
    }
    fn finish(mut self) -> io::Result<()> {
        if let Some(recorder) = self.recorder.take() {
            recorder.finish()?;
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if self.replay_confirmation.is_some() {
            return match key.code {
                KeyCode::Enter | KeyCode::Char('y' | 'Y') => Action::Replay(
                    self.replay_confirmation
                        .take()
                        .expect("checked pending replay"),
                ),
                KeyCode::Esc | KeyCode::Char('n' | 'N' | 'q') => {
                    self.replay_confirmation = None;
                    self.set_notice("Replay cancelled");
                    Action::Continue
                }
                _ => Action::Continue,
            };
        }
        if self.show_help {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?' | 'h' | 'q') => self.show_help = false,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1).min(self.help_max_scroll);
                }
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(5),
                KeyCode::PageDown => {
                    self.help_scroll = self.help_scroll.saturating_add(5).min(self.help_max_scroll);
                }
                KeyCode::Home => self.help_scroll = 0,
                KeyCode::End => self.help_scroll = self.help_max_scroll,
                _ => {}
            }
            return Action::Continue;
        }
        if self.search_target.is_some() {
            return self.handle_search_key(key);
        }
        if self.detail_expanded
            && matches!(
                key.code,
                KeyCode::Char('/') | KeyCode::Char('o') | KeyCode::Tab | KeyCode::BackTab
            )
        {
            return Action::Continue;
        }

        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc if self.detail_expanded => {
                self.detail_expanded = false;
                Action::Continue
            }
            KeyCode::Char('?') | KeyCode::Char('h') => {
                self.show_help = true;
                self.help_scroll = 0;
                Action::Continue
            }
            KeyCode::Char('/') if self.focus == FocusPane::Endpoints => {
                self.begin_search(SearchTarget::Endpoints);
                Action::Continue
            }
            KeyCode::Char('/') if self.focus == FocusPane::Logs => {
                self.begin_search(SearchTarget::Traffic);
                Action::Continue
            }
            KeyCode::Char('v') => {
                self.set_traffic_view(self.traffic_view.next());
                Action::Continue
            }
            KeyCode::Char(shortcut @ '1'..='5') if self.focus == FocusPane::Logs => {
                if let Some(view) = TrafficView::ALL
                    .into_iter()
                    .find(|view| view.shortcut() == shortcut)
                {
                    self.set_traffic_view(view);
                }
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
                self.scroll_detail_up(5);
                Action::Continue
            }
            KeyCode::PageDown if self.focus == FocusPane::Logs => {
                self.scroll_detail_down(5);
                Action::Continue
            }
            KeyCode::Home if self.focus == FocusPane::Logs => {
                self.detail_scroll = 0;
                Action::Continue
            }
            KeyCode::End if self.focus == FocusPane::Logs => {
                self.detail_scroll = self.detail_max_scroll;
                Action::Continue
            }
            KeyCode::PageUp if self.focus == FocusPane::Server => {
                self.scroll_output_up(5);
                Action::Continue
            }
            KeyCode::PageDown if self.focus == FocusPane::Server => {
                self.scroll_output_down(5);
                Action::Continue
            }
            KeyCode::Home if self.focus == FocusPane::Server => {
                self.output_scroll = self.output_max_scroll;
                Action::Continue
            }
            KeyCode::End if self.focus == FocusPane::Server => {
                self.output_scroll = 0;
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
            KeyCode::Char('c') if self.focus == FocusPane::Logs => {
                self.detail_tab = DetailTab::Curl;
                self.detail_scroll = 0;
                self.detail_expanded = true;
                Action::Continue
            }
            KeyCode::Char('f') if self.focus == FocusPane::Logs => {
                self.follow_live = !self.follow_live;
                if self.follow_live {
                    self.select_latest_exchange();
                }
                self.set_notice(if self.follow_live {
                    "Live-follow resumed"
                } else {
                    "Live-follow paused"
                });
                Action::Continue
            }
            KeyCode::Char('R') if self.focus == FocusPane::Logs => {
                if let Some(entry) = self.selected_log().cloned() {
                    if let Some(reason) = non_replayable_reason(&entry) {
                        self.set_notice(format!("Replay unavailable: {reason}"));
                        Action::Continue
                    } else {
                        self.replay_confirmation = Some(Box::new(entry));
                        Action::Continue
                    }
                } else {
                    self.set_notice("No exchange selected to replay");
                    Action::Continue
                }
            }
            KeyCode::Tab => {
                self.cycle_focus(false);
                Action::Continue
            }
            KeyCode::BackTab => {
                self.cycle_focus(true);
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
                self.traffic_view = TrafficView::Selected;
                self.set_focus(FocusPane::Logs);
                self.select_latest_exchange();
                Action::Continue
            }
            KeyCode::Char('o') => {
                self.toggle_server();
                Action::Continue
            }
            KeyCode::Char('r') => Action::Restart,
            _ => Action::Continue,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Action {
        if self.replay_confirmation.is_some() {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                if rect_contains(self.areas.replay_confirm, mouse.column, mouse.row) {
                    return Action::Replay(
                        self.replay_confirmation
                            .take()
                            .expect("checked pending replay"),
                    );
                }
                if rect_contains(self.areas.replay_cancel, mouse.column, mouse.row) {
                    self.replay_confirmation = None;
                    self.set_notice("Replay cancelled");
                }
            }
            return Action::Continue;
        }
        if self.show_help {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => self.show_help = false,
                MouseEventKind::ScrollUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(3);
                }
                MouseEventKind::ScrollDown => {
                    self.help_scroll = self.help_scroll.saturating_add(3).min(self.help_max_scroll);
                }
                _ => {}
            }
            return Action::Continue;
        }
        if self.search_target.is_some() {
            return Action::Continue;
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
                self.set_focus(FocusPane::Logs);
                self.detail_tab = self.detail_tab.previous();
                self.detail_scroll = 0;
            }
            MouseEventKind::ScrollRight
                if self.detail_expanded || rect_contains(self.areas.logs_pane, column, row) =>
            {
                self.set_focus(FocusPane::Logs);
                self.detail_tab = self.detail_tab.next();
                self.detail_scroll = 0;
            }
            _ => {}
        }
        Action::Continue
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
                self.set_focus(FocusPane::Logs);
            }
            return;
        }

        if let Some((index, _)) = self
            .areas
            .pane_tabs
            .iter()
            .enumerate()
            .find(|(_, area)| rect_contains(**area, column, row))
        {
            let focus = FocusPane::ALL[index];
            if focus == FocusPane::Server {
                self.show_server = true;
            }
            self.set_focus(focus);
            return;
        }
        if self.select_traffic_filter_at(column, row) {
            self.set_focus(FocusPane::Logs);
            return;
        }
        if rect_contains(self.areas.endpoints_search, column, row) {
            self.set_focus(FocusPane::Endpoints);
            self.begin_search(SearchTarget::Endpoints);
            return;
        }
        if rect_contains(self.areas.traffic_search, column, row) {
            self.set_focus(FocusPane::Logs);
            self.begin_search(SearchTarget::Traffic);
            return;
        }
        if self.select_tab_at(column, row) {
            self.set_focus(FocusPane::Logs);
            return;
        }
        if rect_contains(self.areas.endpoints_list, column, row) {
            let visible_row = usize::from(row - self.areas.endpoints_list.y);
            let index = self.endpoint_list_offset + visible_row;
            if index < self.filtered.len() {
                self.selected_endpoint = index;
                self.set_focus(FocusPane::Endpoints);
                self.endpoint_changed();
            }
            return;
        }
        if rect_contains(self.areas.exchanges_list, column, row) {
            let visible_row = usize::from(row - self.areas.exchanges_list.y);
            let index = self.exchange_list_offset + visible_row;
            if index < self.visible_log_indices().len() {
                self.selected_exchange = index;
                self.set_focus(FocusPane::Logs);
                self.follow_live = false;
                self.detail_scroll = 0;
            }
            return;
        }
        if rect_contains(self.areas.detail, column, row)
            || rect_contains(self.areas.logs_pane, column, row)
        {
            self.set_focus(FocusPane::Logs);
        } else if rect_contains(self.areas.endpoints_pane, column, row) {
            self.set_focus(FocusPane::Endpoints);
        } else if rect_contains(self.areas.server_pane, column, row) {
            self.set_focus(FocusPane::Server);
        }
    }

    fn handle_mouse_scroll(&mut self, column: u16, row: u16, upwards: bool) {
        if self.detail_expanded || rect_contains(self.areas.detail, column, row) {
            self.set_focus(FocusPane::Logs);
            if upwards {
                self.scroll_detail_up(3);
            } else {
                self.scroll_detail_down(3);
            }
        } else if rect_contains(self.areas.exchanges_list, column, row) {
            self.set_focus(FocusPane::Logs);
            if upwards {
                self.navigate_up();
            } else {
                self.navigate_down();
            }
        } else if rect_contains(self.areas.endpoints_pane, column, row) {
            self.set_focus(FocusPane::Endpoints);
            if upwards {
                self.navigate_up();
            } else {
                self.navigate_down();
            }
        } else if rect_contains(self.areas.server_pane, column, row) {
            self.set_focus(FocusPane::Server);
            if upwards {
                self.scroll_output_up(3);
            } else {
                self.scroll_output_down(3);
            }
        }
    }

    fn select_traffic_filter_at(&mut self, column: u16, row: u16) -> bool {
        let Some((index, _)) = self
            .areas
            .traffic_filters
            .iter()
            .enumerate()
            .find(|(_, area)| rect_contains(**area, column, row))
        else {
            return false;
        };
        self.set_traffic_view(TrafficView::ALL[index]);
        true
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
        let Some(target) = self.search_target else {
            return Action::Continue;
        };
        let edit = match target {
            SearchTarget::Endpoints => edit_search(
                &mut self.endpoint_search_query,
                &mut self.endpoint_search_cursor,
                key,
            ),
            SearchTarget::Traffic => edit_search(
                &mut self.traffic_search_query,
                &mut self.traffic_search_cursor,
                key,
            ),
        };
        match edit {
            SearchEdit::Applied => self.apply_search(),
            SearchEdit::Cancelled => self.cancel_search(),
            SearchEdit::Changed => match target {
                SearchTarget::Endpoints => self.filter_endpoints(),
                SearchTarget::Traffic => self.select_latest_exchange(),
            },
            SearchEdit::Unchanged => {}
        }
        Action::Continue
    }

    fn filter_endpoints(&mut self) {
        let query = self.endpoint_search_query.to_lowercase();
        self.filtered = self
            .endpoints
            .iter()
            .enumerate()
            .filter(|(_, endpoint)| query.is_empty() || endpoint_matches_query(endpoint, &query))
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
                self.follow_live = false;
                self.detail_scroll = 0;
            }
            FocusPane::Server => self.scroll_output_up(1),
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
                let last = self.visible_log_indices().len().saturating_sub(1);
                self.selected_exchange = (self.selected_exchange + 1).min(last);
                self.detail_scroll = 0;
            }
            FocusPane::Server => self.scroll_output_down(1),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchEdit {
    Changed,
    Applied,
    Cancelled,
    Unchanged,
}

fn edit_search(query: &mut String, cursor: &mut usize, key: KeyEvent) -> SearchEdit {
    match key.code {
        KeyCode::Enter => SearchEdit::Applied,
        KeyCode::Esc => SearchEdit::Cancelled,
        KeyCode::Backspace if *cursor > 0 => {
            let start = byte_index(query, *cursor - 1);
            let end = byte_index(query, *cursor);
            query.replace_range(start..end, "");
            *cursor -= 1;
            SearchEdit::Changed
        }
        KeyCode::Delete if *cursor < query.chars().count() => {
            let start = byte_index(query, *cursor);
            let end = byte_index(query, *cursor + 1);
            query.replace_range(start..end, "");
            SearchEdit::Changed
        }
        KeyCode::Left => {
            *cursor = (*cursor).saturating_sub(1);
            SearchEdit::Unchanged
        }
        KeyCode::Right => {
            *cursor = (*cursor + 1).min(query.chars().count());
            SearchEdit::Unchanged
        }
        KeyCode::Home | KeyCode::Char('a')
            if key.code == KeyCode::Home || key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            *cursor = 0;
            SearchEdit::Unchanged
        }
        KeyCode::End | KeyCode::Char('e')
            if key.code == KeyCode::End || key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            *cursor = query.chars().count();
            SearchEdit::Unchanged
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if query.is_empty() {
                SearchEdit::Unchanged
            } else {
                query.clear();
                *cursor = 0;
                SearchEdit::Changed
            }
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            let index = byte_index(query, *cursor);
            query.insert(index, character);
            *cursor += 1;
            SearchEdit::Changed
        }
        _ => SearchEdit::Unchanged,
    }
}

fn endpoint_matches_query(endpoint: &Endpoint, query: &str) -> bool {
    endpoint.method.to_lowercase().contains(query)
        || endpoint.path.to_lowercase().contains(query)
        || endpoint
            .summary
            .as_deref()
            .is_some_and(|summary| summary.to_lowercase().contains(query))
        || endpoint
            .operation_id
            .as_deref()
            .is_some_and(|operation_id| operation_id.to_lowercase().contains(query))
        || endpoint
            .tags
            .iter()
            .any(|tag| tag.to_lowercase().contains(query))
}

fn log_matches_query(entry: &LogEntry, query: &str) -> bool {
    entry.method.to_lowercase().contains(query)
        || entry.path.to_lowercase().contains(query)
        || entry.status.to_string().contains(query)
        || entry.latency_ms.to_string().contains(query)
        || entry
            .query
            .as_deref()
            .is_some_and(|value| value.to_lowercase().contains(query))
        || headers_match_query(&entry.request, query)
        || headers_match_query(&entry.response, query)
        || entry.request.body.to_lowercase().contains(query)
        || entry.response.body.to_lowercase().contains(query)
        || (entry.contract.inconclusive
            && ("inconclusive".contains(query) || "partial".contains(query)))
        || entry.contract.violations.iter().any(|violation| {
            violation.code.to_lowercase().contains(query)
                || violation.location.to_lowercase().contains(query)
                || violation.message.to_lowercase().contains(query)
        })
}

fn headers_match_query(part: &ExchangePart, query: &str) -> bool {
    part.iter_headers().any(|(name, value)| {
        name.to_lowercase().contains(query) || value.to_lowercase().contains(query)
    })
}

pub fn run(
    endpoints: Vec<Endpoint>,
    load_path: Option<&Path>,
    recorder: Option<SessionRecorder>,
    server: &mut CaptureServer,
    output_rx: Receiver<String>,
    logs_rx: Receiver<LogEntry>,
) -> io::Result<()> {
    let mut app = App::with_recorder(endpoints, Vec::new(), recorder)?;
    if let Some(path) = load_path {
        session::visit(path, |entry| app.push_loaded_log(entry))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    }
    server.start().map_err(io::Error::other)?;
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

    let result = run_loop(&mut terminal, app, server, output_rx, logs_rx);

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
        drain_messages(&mut app, &output_rx, &logs_rx)?;
        app.expire_notice_at(Instant::now());
        terminal.draw(|frame| draw(frame, &mut app))?;

        if !event::poll(Duration::from_millis(75))? {
            continue;
        }
        let action = match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
            Event::Mouse(mouse) => app.handle_mouse(mouse),
            _ => Action::Continue,
        };
        match action {
            Action::Continue => {}
            Action::Quit => {
                stop_server_and_drain(&mut app, server, &output_rx, &logs_rx)?;
                return app.finish();
            }
            Action::Restart => {
                stop_server_and_drain(&mut app, server, &output_rx, &logs_rx)?;
                match server.restart() {
                    Ok(address) => {
                        app.set_notice(format!("Capture server restarted on {address}"));
                    }
                    Err(error) => app.set_sticky_notice(format!("Restart failed: {error}")),
                }
            }
            Action::Replay(entry) => match server.replay(&entry) {
                Ok(()) => {
                    app.set_notice(format!(
                        "Replay queued: {} {}",
                        entry.method,
                        request_target(&entry)
                    ));
                }
                Err(error) => app.set_sticky_notice(format!("Replay failed: {error}")),
            },
        }
    }
}

fn stop_server_and_drain(
    app: &mut App,
    server: &mut CaptureServer,
    output_rx: &Receiver<String>,
    logs_rx: &Receiver<LogEntry>,
) -> io::Result<()> {
    thread::scope(|scope| {
        let stopping = scope.spawn(|| server.stop());
        while !stopping.is_finished() {
            drain_messages(app, output_rx, logs_rx)?;
            thread::sleep(Duration::from_millis(1));
        }
        stopping
            .join()
            .map_err(|_| io::Error::other("capture server stop worker panicked"))?;
        drain_all_messages(app, output_rx, logs_rx)
    })
}

fn drain_messages(
    app: &mut App,
    output_rx: &Receiver<String>,
    logs_rx: &Receiver<LogEntry>,
) -> io::Result<()> {
    while let Ok(line) = output_rx.try_recv() {
        app.push_output(line);
    }
    drain_log_batch(app, logs_rx).map(|_| ())
}

fn drain_all_messages(
    app: &mut App,
    output_rx: &Receiver<String>,
    logs_rx: &Receiver<LogEntry>,
) -> io::Result<()> {
    while let Ok(line) = output_rx.try_recv() {
        app.push_output(line);
    }
    while drain_log_batch(app, logs_rx)? > 0 {}
    Ok(())
}

fn drain_log_batch(app: &mut App, logs_rx: &Receiver<LogEntry>) -> io::Result<usize> {
    let mut entries = Vec::with_capacity(LOG_DRAIN_BATCH);
    for _ in 0..LOG_DRAIN_BATCH {
        let Ok(entry) = logs_rx.try_recv() else {
            break;
        };
        entries.push(entry);
    }
    let drained = entries.len();
    for entry in entries {
        app.push_log(entry)?;
    }
    Ok(drained)
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    app.expire_notice_at(Instant::now());
    let area = frame.area();
    app.areas = UiAreas::default();
    let mode = layout_mode(area);
    app.areas.layout_mode = mode;
    if mode == LayoutMode::TooSmall {
        frame.render_widget(
            Paragraph::new("LazyAPI needs a terminal of at least 60x12")
                .style(Style::default().fg(Color::Yellow)),
            area,
        );
        if app.replay_confirmation.is_some() {
            render_replay_confirmation(frame, app, area);
        }
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
    app.areas.workspace = rows[0];
    render_workspace(frame, app, rows[0], mode);
    render_status(frame, app, rows[1]);
    render_hint(frame, app, rows[2]);

    if app.detail_expanded {
        app.areas.detail = area;
        app.areas.tabs = tab_hitboxes(area);
        app.areas.close_detail = close_hitbox(area);
        render_expanded_detail(frame, app, area);
    }
    if app.show_help {
        render_help(frame, app, area);
    }
    if app.replay_confirmation.is_some() {
        render_replay_confirmation(frame, app, area);
    }
}

fn layout_mode(area: Rect) -> LayoutMode {
    if area.width < 60 || area.height < 12 {
        LayoutMode::TooSmall
    } else if area.width >= 120 && area.height >= 18 {
        LayoutMode::Wide
    } else if area.width >= 88 && area.height >= 16 {
        LayoutMode::Compact
    } else {
        LayoutMode::Narrow
    }
}

fn render_workspace(frame: &mut Frame<'_>, app: &mut App, area: Rect, mode: LayoutMode) {
    if mode == LayoutMode::Narrow {
        app.show_server = app.focus == FocusPane::Server;
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(area);
        render_pane_switcher(frame, app, rows[0]);
        match app.focus {
            FocusPane::Endpoints => {
                app.areas.endpoints_pane = rows[1];
                render_endpoints(frame, app, rows[1]);
            }
            FocusPane::Logs => {
                app.areas.logs_pane = rows[1];
                render_logs(frame, app, rows[1]);
            }
            FocusPane::Server => {
                app.areas.server_pane = rows[1];
                render_server(frame, app, rows[1]);
            }
        }
        return;
    }

    let (primary, server) = if app.show_server {
        let drawer_height = (area.height / 3)
            .clamp(4, 8)
            .min(area.height.saturating_sub(6));
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(drawer_height)])
            .split(area);
        (rows[0], Some(rows[1]))
    } else {
        (area, None)
    };
    let constraints = if mode == LayoutMode::Wide {
        [Constraint::Percentage(30), Constraint::Percentage(70)]
    } else {
        [Constraint::Length(32), Constraint::Min(40)]
    };
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(primary);
    app.areas.endpoints_pane = panes[0];
    app.areas.logs_pane = panes[1];
    render_endpoints(frame, app, panes[0]);
    render_logs(frame, app, panes[1]);
    if let Some(server) = server {
        app.areas.server_pane = server;
        render_server(frame, app, server);
    }
}

fn render_pane_switcher(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let mut spans = Vec::new();
    let mut x = area.x;
    for (index, pane) in FocusPane::ALL.iter().enumerate() {
        let label = format!(" {} ", pane.label());
        let width = label.chars().count() as u16;
        app.areas.pane_tabs[index] =
            Rect::new(x, area.y, width.min(area.right().saturating_sub(x)), 1);
        let style = if *pane == app.focus {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            dim_style()
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
        x = x.saturating_add(width + 1);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn panel_block<'a>(title: String, focused: bool) -> Block<'a> {
    let border = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title = if focused {
        format!("▶{title}")
    } else {
        title
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title)
}

fn render_endpoints(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let title = if app.endpoint_search_query.is_empty() {
        format!(" Endpoints ({}) ", app.endpoints.len())
    } else {
        format!(
            " Endpoints ({}/{}) ",
            app.filtered.len(),
            app.endpoints.len()
        )
    };
    let items: Vec<_> = app
        .filtered
        .iter()
        .enumerate()
        .map(|(number, index)| {
            let endpoint = &app.endpoints[*index];
            let mut spans = vec![
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
            ];
            if let Some(summary) = endpoint
                .summary
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                spans.push(Span::styled(format!(" · {summary}"), dim_style()));
            }
            if !endpoint.tags.is_empty() {
                spans.push(Span::styled(
                    format!(" [{}]", endpoint.tags.join(",")),
                    Style::default().fg(Color::Cyan),
                ));
            }
            if let Some(operation_id) = endpoint
                .operation_id
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                spans.push(Span::styled(
                    format!(" #{operation_id}"),
                    Style::default().fg(Color::Magenta),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let focused = app.focus == FocusPane::Endpoints;
    let block = panel_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    app.areas.endpoints_search = sections[0];
    app.areas.endpoints_list = sections[1];
    frame.render_widget(
        Paragraph::new(search_input_line(
            &app.endpoint_search_query,
            app.endpoint_search_cursor,
            app.search_target == Some(SearchTarget::Endpoints),
            "search endpoints",
            app.filtered.len(),
            app.endpoints.len(),
            sections[0].width as usize,
        )),
        sections[0],
    );
    if app.filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("No endpoints match this search", dim_style()),
                Line::styled("Press / to edit; Ctrl+U clears while editing.", dim_style()),
            ])
            .wrap(Wrap { trim: false }),
            sections[1],
        );
        return;
    }
    let list = List::new(items)
        .highlight_symbol(" > ")
        .highlight_style(selected_style(focused));
    let mut state = ListState::default();
    state.select((!app.filtered.is_empty()).then_some(app.selected_endpoint));
    frame.render_stateful_widget(list, sections[1], &mut state);
    app.endpoint_list_offset = state.offset();
}

fn search_input_line(
    query: &str,
    cursor: usize,
    active: bool,
    placeholder: &str,
    matches: usize,
    total: usize,
    width: usize,
) -> Line<'static> {
    let count = if query.is_empty() {
        format!(" {total}")
    } else {
        format!(" {matches}/{total}")
    };
    if query.is_empty() && !active {
        return Line::from(vec![
            Span::styled(" / ", Style::default().fg(Color::Cyan)),
            Span::styled(
                truncate(placeholder, width.saturating_sub(count.len() + 3)),
                dim_style(),
            ),
            Span::styled(count, dim_style()),
        ]);
    }

    let characters: Vec<char> = query.chars().collect();
    let cursor = cursor.min(characters.len());
    let caret_width = usize::from(active);
    let available = width.saturating_sub(count.len() + 2 + caret_width).max(1);
    let mut start = cursor.saturating_sub(available / 2);
    let mut end = (start + available).min(characters.len());
    if end - start < available {
        start = end.saturating_sub(available);
    }
    if cursor < start {
        start = cursor;
    }
    if cursor > end {
        end = cursor;
    }
    let before: String = characters[start..cursor].iter().collect();
    let after: String = characters[cursor..end].iter().collect();
    let mut spans = vec![
        Span::styled(" /", Style::default().fg(Color::Cyan)),
        Span::raw(before),
    ];
    if active {
        spans.push(Span::styled(
            "▏",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::raw(after));
    if end < characters.len() {
        spans.push(Span::styled("…", dim_style()));
    }
    spans.push(Span::styled(count, dim_style()));
    Line::from(spans)
}

fn render_logs(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let view_indices = app.view_log_indices();
    let indices = app.visible_log_indices();
    let count = if app.traffic_search_query.is_empty() {
        indices.len().to_string()
    } else {
        format!("{}/{}", indices.len(), view_indices.len())
    };
    let retention = if app.history_len > app.logs.len() {
        format!(" [latest {}/{}]", app.logs.len(), app.history_len)
    } else {
        String::new()
    };
    let block = panel_block(
        format!(
            " Requests ({count}) [{}]{retention} ",
            if app.follow_live { "LIVE" } else { "PAUSED" }
        ),
        app.focus == FocusPane::Logs,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);
    render_traffic_filters(frame, app, sections[0]);
    app.areas.traffic_search = sections[1];
    frame.render_widget(
        Paragraph::new(search_input_line(
            &app.traffic_search_query,
            app.traffic_search_cursor,
            app.search_target == Some(SearchTarget::Traffic),
            "search requests",
            indices.len(),
            view_indices.len(),
            sections[1].width as usize,
        )),
        sections[1],
    );
    let content = sections[2];

    if indices.is_empty() {
        app.areas.detail = content;
        let empty_title = if app.traffic_search_query.is_empty() {
            "No requests in this view"
        } else {
            "No requests match this search"
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(empty_title, dim_style()),
                Line::raw(""),
                Line::styled(
                    "Send traffic to the capture address, or choose another filter above.",
                    dim_style(),
                ),
            ]),
            content,
        );
        return;
    }

    let list_height = if content.height >= 9 {
        (content.height / 3).clamp(3, 6)
    } else {
        content.height.min(2)
    };
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(list_height), Constraint::Min(1)])
        .split(content);
    app.areas.exchanges_list = body[0];
    app.areas.detail = body[1];
    app.areas.tabs = tab_hitboxes(body[1]);

    let items: Vec<_> = indices
        .iter()
        .filter_map(|index| app.logs.get(*index))
        .map(|entry| ListItem::new(exchange_summary(entry, body[0].width as usize)))
        .collect();
    let list = List::new(items)
        .highlight_symbol(" > ")
        .highlight_style(selected_style(app.focus == FocusPane::Logs));
    let mut state = ListState::default();
    state.select(Some(app.selected_exchange.min(indices.len() - 1)));
    frame.render_stateful_widget(list, body[0], &mut state);
    app.exchange_list_offset = state.offset();

    render_exchange_detail(frame, app, body[1], false);
}

fn render_traffic_filters(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let counts: Vec<_> = TrafficView::ALL
        .iter()
        .map(|view| app.traffic_view_count(*view))
        .collect();
    let verbose: Vec<_> = TrafficView::ALL
        .iter()
        .zip(&counts)
        .map(|(view, count)| format!(" {} {} {count} ", view.shortcut(), view.label()))
        .collect();
    let compact: Vec<_> = TrafficView::ALL
        .iter()
        .zip(&counts)
        .map(|(view, count)| format!(" {} {} {count} ", view.shortcut(), view.short_label()))
        .collect();
    let minimal: Vec<_> = TrafficView::ALL
        .iter()
        .map(|view| {
            format!(
                " {}{} ",
                view.shortcut(),
                view.short_label().chars().next().unwrap()
            )
        })
        .collect();
    let fits = |labels: &[String]| {
        labels
            .iter()
            .map(|label| label.chars().count())
            .sum::<usize>()
            + labels.len().saturating_sub(1)
            <= area.width as usize
    };
    let labels = if fits(&verbose) {
        verbose
    } else if fits(&compact) {
        compact
    } else {
        minimal
    };
    let mut spans = Vec::new();
    let mut x = area.x;
    for (index, (view, label)) in TrafficView::ALL.iter().zip(labels).enumerate() {
        let width = label.chars().count() as u16;
        app.areas.traffic_filters[index] =
            Rect::new(x, area.y, width.min(area.right().saturating_sub(x)), 1);
        let style = if *view == app.traffic_view {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            dim_style()
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
        x = x.saturating_add(width + 1);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_expanded_detail(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    frame.render_widget(Clear, area);
    if app.selected_log().is_some() {
        render_exchange_detail(frame, app, area, true);
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

fn render_exchange_detail(frame: &mut Frame<'_>, app: &mut App, area: Rect, expanded: bool) {
    let Some(entry) = app.selected_log().cloned() else {
        app.detail_max_scroll = 0;
        app.detail_scroll = 0;
        return;
    };
    let border_style = if expanded {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let borders = if expanded { Borders::ALL } else { Borders::TOP };
    let block = Block::default()
        .borders(borders)
        .border_style(border_style)
        .title(detail_tabs(app.detail_tab, expanded, area.width));
    let inner = block.inner(area);
    let lines = detail_lines(&entry, app.detail_tab, app.pretty_bodies);
    let mut paragraph = Paragraph::new(lines).block(block);
    if app.wrap_bodies {
        paragraph = paragraph.wrap(Wrap { trim: false });
    }
    let rendered_lines = paragraph.line_count(inner.width);
    let vertical_space = area.height.saturating_sub(inner.height) as usize;
    let content_lines = rendered_lines.saturating_sub(vertical_space);
    app.detail_max_scroll = content_lines
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize);
    app.detail_scroll = app.detail_scroll.min(app.detail_max_scroll);
    let scroll = u16::try_from(app.detail_scroll).unwrap_or(u16::MAX);
    paragraph = paragraph.scroll((scroll, 0));
    frame.render_widget(paragraph, area);
    if app.detail_max_scroll > 0 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("█");
        let mut state = ScrollbarState::new(content_lines)
            .position(app.detail_scroll)
            .viewport_content_length(inner.height as usize);
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: u16::from(expanded),
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

fn detail_tabs(active: DetailTab, expanded: bool, width: u16) -> Line<'static> {
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
        spans.push(Span::styled(
            format!(" {} ", detail_tab_label(tab, width)),
            style,
        ));
        spans.push(Span::raw(" "));
    }
    if expanded {
        spans.push(Span::styled("[e/Esc close]", dim_style()));
    }
    Line::from(spans)
}

fn detail_tab_label(tab: DetailTab, width: u16) -> &'static str {
    if width >= 58 {
        return tab.label();
    }
    match tab {
        DetailTab::Request => "Req",
        DetailTab::Response => "Res",
        DetailTab::Headers => "Hdr",
        DetailTab::Contract => "Ctr",
        DetailTab::Curl => "cURL",
    }
}

fn detail_lines(entry: &LogEntry, tab: DetailTab, pretty: bool) -> Vec<Line<'static>> {
    match tab {
        DetailTab::Request => request_lines(entry, pretty),
        DetailTab::Response => response_lines(entry, pretty),
        DetailTab::Headers => header_detail_lines(entry),
        DetailTab::Contract => contract_lines(entry),
        DetailTab::Curl => curl_lines(entry),
    }
}

fn contract_lines(entry: &LogEntry) -> Vec<Line<'static>> {
    if !entry.contract.checked {
        return vec![
            section_heading("Contract validation"),
            Line::raw(""),
            Line::styled("? This exchange has not been checked", dim_style()),
        ];
    }

    let mut lines = vec![section_heading("Contract validation")];
    if let Some(endpoint) = &entry.contract.matched_endpoint {
        lines.push(Line::from(vec![
            Span::styled("Matched: ", dim_style()),
            Span::raw(endpoint.clone()),
        ]));
    }
    lines.push(Line::raw(""));
    let definite_count = definite_contract_violation_count(entry);
    if definite_count == 0 && !entry.contract.inconclusive {
        lines.push(Line::styled(
            "✓ No contract violations",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
        return lines;
    }

    if definite_count > 0 {
        lines.push(Line::styled(
            format!(
                "! {definite_count} definite contract violation{}",
                if definite_count == 1 { "" } else { "s" }
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    if entry.contract.inconclusive {
        if definite_count > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(
            "~ Validation is also partial; unchecked assertions may hide more findings",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    for violation in &entry.contract.violations {
        let style = if is_partial_contract_finding(&violation.code) {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        };
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(format!("[{}]", violation.code), style),
            Span::styled(format!(" {}", violation.location), dim_style()),
        ]));
        lines.push(Line::raw(format!("  {}", violation.message)));
    }
    lines
}

fn curl_lines(entry: &LogEntry) -> Vec<Line<'static>> {
    match curl_command(entry) {
        Ok(command) => {
            let mut lines = vec![section_heading("Replay as cURL"), Line::raw("")];
            lines.extend(command.lines().map(|line| Line::raw(line.to_string())));
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Tip: use your terminal's select/copy modifier to copy this command.",
                dim_style(),
            ));
            lines
        }
        Err(reason) => vec![
            section_heading("cURL unavailable"),
            Line::raw(""),
            Line::styled(
                format!("! {reason}"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::styled(
                "LazyAPI will not generate a command that could replay different bytes.",
                dim_style(),
            ),
        ],
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
        entry.request.header_count()
    ))];
    lines.extend(header_lines(&entry.request));
    lines.push(Line::raw(""));
    lines.push(section_heading(&format!(
        "Response headers ({})",
        entry.response.header_count()
    )));
    lines.extend(header_lines(&entry.response));
    lines
}

fn header_lines(part: &ExchangePart) -> Vec<Line<'static>> {
    if part.header_count() == 0 {
        return vec![Line::styled("  none", dim_style())];
    }
    part.iter_headers()
        .map(|(name, value)| {
            Line::from(vec![
                Span::styled(format!("{name}: "), Style::default().fg(Color::Cyan)),
                Span::raw(value.to_string()),
            ])
        })
        .collect()
}

fn request_target(entry: &LogEntry) -> String {
    entry.query.as_ref().map_or_else(
        || entry.path.clone(),
        |query| format!("{}?{query}", entry.path),
    )
}

fn curl_command(entry: &LogEntry) -> Result<String, &'static str> {
    if let Some(reason) = non_replayable_reason(entry) {
        return Err(reason);
    }
    let host = entry
        .request
        .header_value("host")
        .unwrap_or("127.0.0.1:3000");
    let mut parts = vec![
        "curl -i".to_string(),
        format!("  -X {}", shell_quote(&entry.method)),
        format!(
            "  {}",
            shell_quote(&format!("http://{host}{}", request_target(entry)))
        ),
    ];
    for (name, value) in entry.request.iter_headers() {
        if name.eq_ignore_ascii_case("host") || name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        parts.insert(
            parts.len() - 1,
            format!("  -H {}", shell_quote(&format!("{name}: {value}"))),
        );
    }
    if !entry.request.body.is_empty() {
        parts.insert(
            parts.len() - 1,
            format!("  --data-raw {}", shell_quote(&entry.request.body)),
        );
    }
    Ok(parts.join(" \\\n"))
}

fn non_replayable_reason(entry: &LogEntry) -> Option<&'static str> {
    if reqwest::Method::from_bytes(entry.method.as_bytes()).is_err() {
        return Some("the captured HTTP method is invalid");
    }
    if entry.request.truncated {
        return Some("the request body capture is truncated");
    }
    if entry.request.body.contains('\u{fffd}') {
        return Some("the request body contains lossy UTF-8 replacement characters");
    }
    let request_was_redacted = entry
        .query
        .as_deref()
        .is_some_and(contains_redaction_marker)
        || entry
            .request
            .iter_headers()
            .any(|(_, value)| contains_redaction_marker(value))
        || contains_redaction_marker(&entry.request.body);
    if request_was_redacted {
        return Some("the request capture contains redacted values");
    }
    None
}

fn capture_contains_redaction(entry: &LogEntry) -> bool {
    entry
        .query
        .as_deref()
        .is_some_and(contains_redaction_marker)
        || exchange_part_contains_redaction(&entry.request)
        || exchange_part_contains_redaction(&entry.response)
}

fn exchange_part_contains_redaction(part: &ExchangePart) -> bool {
    contains_redaction_marker(&part.body)
        || part
            .iter_headers()
            .any(|(_, value)| contains_redaction_marker(value))
}

fn contains_redaction_marker(value: &str) -> bool {
    if value.to_ascii_lowercase().contains("[redacted]") {
        return true;
    }
    form_urlencoded::parse(value.as_bytes()).any(|(key, value)| {
        key.to_ascii_lowercase().contains("[redacted]")
            || value.to_ascii_lowercase().contains("[redacted]")
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn body_lines(part: &ExchangePart, pretty: bool) -> Vec<Line<'static>> {
    let content_type = part.header_value("content-type").unwrap_or_default();
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
    if is_binary_content_type(content_type) {
        lines.push(Line::styled(
            format!(
                "Binary body: {} ({})",
                content_type_or_unknown(content_type),
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
    let content_type = part.header_value("content-type").unwrap_or_default();
    Line::from(vec![
        Span::styled("Content-Type: ", dim_style()),
        Span::raw(content_type_or_unknown(content_type)),
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
    let definite_count = definite_contract_violation_count(entry);
    let (contract_badge, contract_style) = if definite_count > 0 {
        (
            format!(
                " !{definite_count}{} ",
                if entry.contract.inconclusive { "~" } else { "" }
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if entry.contract.inconclusive {
        (
            " ~ ".to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if !entry.contract.checked {
        (" ? ".to_string(), dim_style())
    } else if entry.contract.is_valid() {
        (" ✓ ".to_string(), Style::default().fg(Color::Green))
    } else {
        (
            format!(" !{} ", entry.contract.violation_count()),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    };
    let mut spans = Vec::new();
    let mut used = 0;
    if width >= 64 {
        spans.push(Span::styled(time, dim_style()));
        spans.push(Span::raw(" "));
        used += 9;
    }
    spans.push(Span::styled(
        format!("{:<6}", entry.method),
        Style::default()
            .fg(method_color(&entry.method))
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("{:>3}", entry.status),
        Style::default().fg(status_color(entry.status)),
    ));
    spans.push(Span::raw(" "));
    used += 11;
    if width >= 45 {
        spans.push(Span::styled(
            format!("{:>6} ", format!("{}ms", entry.latency_ms)),
            Style::default().fg(latency_color(entry.latency_ms)),
        ));
        used += 7;
    }
    used += contract_badge.chars().count();
    spans.push(Span::styled(contract_badge, contract_style));
    spans.push(Span::raw(truncate(
        &target,
        width.saturating_sub(used).max(1),
    )));
    Line::from(spans)
}

fn definite_contract_violation_count(entry: &LogEntry) -> usize {
    entry
        .contract
        .violations
        .iter()
        .filter(|violation| !is_partial_contract_finding(&violation.code))
        .count()
}

fn is_partial_contract_finding(code: &str) -> bool {
    matches!(
        code,
        "validation_inconclusive" | "validation_budget_exceeded"
    )
}

fn render_server(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
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

    let block = panel_block(
        format!(" Server output ({}) [o close] ", app.output.len()),
        app.focus == FocusPane::Server,
    );
    let inner = block.inner(area);
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    let rendered_lines = paragraph.line_count(inner.width);
    let vertical_space = area.height.saturating_sub(inner.height) as usize;
    let content_lines = rendered_lines.saturating_sub(vertical_space);
    app.output_max_scroll = content_lines
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize);
    app.output_scroll = app.output_scroll.min(app.output_max_scroll);
    let scroll =
        u16::try_from(app.output_max_scroll.saturating_sub(app.output_scroll)).unwrap_or(u16::MAX);
    let paragraph = paragraph.scroll((scroll, 0));
    frame.render_widget(paragraph, area);
    if app.output_max_scroll > 0 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("█");
        let mut state = ScrollbarState::new(content_lines)
            .position(scroll as usize)
            .viewport_content_length(inner.height as usize);
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

fn render_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let selected = app.selected_endpoint().map_or_else(
        || "No endpoint selected".to_string(),
        |endpoint| format!("Selected: {} {}", endpoint.method, endpoint.path),
    );
    let detail = if app.focus == FocusPane::Logs {
        format!(
            " | {} | {} | {} | {} | {}",
            app.detail_tab.label(),
            if app.pretty_bodies { "pretty" } else { "raw" },
            if app.wrap_bodies { "wrap" } else { "clip" },
            app.traffic_view.label(),
            if app.follow_live {
                "following"
            } else {
                "paused"
            }
        )
    } else {
        format!(" | {}", app.focus.label())
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
    let hint = if let Some(target) = app.search_target {
        match target {
            SearchTarget::Endpoints => {
                "Endpoint search | Enter: apply | Esc: cancel | Ctrl+U: clear"
            }
            SearchTarget::Traffic => "Request search | Enter: apply | Esc: cancel | Ctrl+U: clear",
        }
    } else if app.focus == FocusPane::Logs {
        "1-5: filter | /: search | f: follow | R: replay | c: cURL | e: expand | o: output"
    } else if app.focus == FocusPane::Server {
        "o: close output | PgUp/PgDn or wheel: scroll | Home/End: oldest/latest | Tab: panes"
    } else {
        "Tab: panes | arrows/jk: navigate | /: search | o: server output | ?: help | q: quit"
    };
    frame.render_widget(
        Paragraph::new(truncate(hint, area.width as usize)).style(dim_style()),
        area,
    );
}

fn render_help(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let popup = if area.width < 84 || area.height < 26 {
        area.inner(Margin {
            horizontal: 1,
            vertical: 1,
        })
    } else {
        centered_rect(78, 90, area)
    };
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Help ");
    let inner = block.inner(popup);
    let help = Paragraph::new(
        "LazyAPI\n\n\
         Navigation\n\
         Up/k, Down/j    Navigate focused pane\n\
         Tab/Shift+Tab   Switch panes; Enter opens\n\
         o               Open or close server output\n\
         /               Edit the focused search\n\
         Enter / Esc     Apply / cancel search edits\n\n\
         Traffic\n\
         1 Selected      Only the chosen endpoint\n\
         2 All           Every captured request\n\
         3 Errors        HTTP and contract errors\n\
         4 Slow          Requests at or above 500ms\n\
         5 Unmatched     Requests outside the spec\n\
         v               Cycle traffic filters\n\
         f               Pause or resume live-follow\n\
         R               Review and confirm replay\n\n\
         Inspector\n\
         Left/Right tabs; PgUp/PgDown scroll\n\
         Home/End jump to top/bottom\n\
         p pretty; w wrap; e expand; c show cURL\n\
         ✓ / ~ / !n      Contract status / partial / violations\n\n\
         Mouse: click to focus/select; wheel scroll\n\
         r restart; q/Ctrl+C quit\n\n\
         Scroll for more · ?/Esc/q closes help",
    )
    .block(block)
    .wrap(Wrap { trim: false });
    let rendered_lines = help.line_count(inner.width);
    let vertical_space = popup.height.saturating_sub(inner.height) as usize;
    let content_lines = rendered_lines.saturating_sub(vertical_space);
    app.help_max_scroll = content_lines
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize);
    app.help_scroll = app.help_scroll.min(app.help_max_scroll);
    let help = help.scroll((u16::try_from(app.help_scroll).unwrap_or(u16::MAX), 0));
    frame.render_widget(help, popup);
    if app.help_max_scroll > 0 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("█");
        let mut state = ScrollbarState::new(content_lines)
            .position(app.help_scroll)
            .viewport_content_length(inner.height as usize);
        frame.render_stateful_widget(
            scrollbar,
            popup.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

fn render_replay_confirmation(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(entry) = app.replay_confirmation.as_ref() else {
        return;
    };
    let method = entry.method.clone();
    let target = request_target(entry);
    let captured_host = entry
        .request
        .header_value("host")
        .unwrap_or("not captured")
        .to_string();
    let normalized_method = method.to_ascii_uppercase();
    let mutating = !matches!(
        normalized_method.as_str(),
        "GET" | "HEAD" | "OPTIONS" | "TRACE"
    );
    let popup = centered_fixed_rect(76, if mutating { 11 } else { 10 }, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Confirm replay ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let line_width = rows[0].width as usize;
    let mut lines = Vec::new();
    if mutating {
        lines.push(Line::styled(
            "Warning: this method may change upstream data.",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::raw(""));
    }
    lines.extend([
        Line::from(Span::styled(
            format!(" {method} "),
            Style::default()
                .fg(Color::Black)
                .bg(method_color(&method))
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(truncate(&format!("Target: {target}"), line_width)),
        Line::styled(
            truncate(&format!("Captured Host: {captured_host}"), line_width),
            dim_style(),
        ),
        Line::raw(truncate("Replay route: active capture server", line_width)),
    ]);
    frame.render_widget(Paragraph::new(lines), rows[0]);

    let confirm = " Enter/y: replay ";
    let cancel = " Esc/n: cancel ";
    app.areas.replay_confirm = Rect::new(
        rows[1].x,
        rows[1].y,
        (confirm.chars().count() as u16).min(rows[1].width),
        1,
    );
    let cancel_x = rows[1].x.saturating_add(confirm.chars().count() as u16 + 2);
    app.areas.replay_cancel = Rect::new(
        cancel_x,
        rows[1].y,
        (cancel.chars().count() as u16).min(rows[1].right().saturating_sub(cancel_x)),
        1,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                confirm,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(cancel, dim_style()),
        ])),
        rows[1],
    );
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

fn selected_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    }
}

fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn tab_hitboxes(area: Rect) -> [Rect; 5] {
    let mut tabs = [Rect::default(); 5];
    let mut x = area.x.saturating_add(2);
    for (index, tab) in DetailTab::ALL.iter().enumerate() {
        let width = detail_tab_label(*tab, area.width).chars().count() as u16 + 2;
        tabs[index] = title_hitbox(area, x, width);
        x = x.saturating_add(width + 1);
    }
    tabs
}

fn close_hitbox(area: Rect) -> Rect {
    let tabs_width: u16 = DetailTab::ALL
        .iter()
        .map(|tab| detail_tab_label(*tab, area.width).chars().count() as u16 + 3)
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

fn centered_fixed_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
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
    use std::{
        collections::BTreeMap,
        sync::mpsc,
        time::{Duration, Instant},
    };

    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};
    use tempfile::tempdir;

    use super::{
        Action, App, DISPLAY_LOG_LIMIT, DetailTab, FocusPane, LayoutMode, NOTICE_TTL, SearchTarget,
        TrafficView, body_lines, contract_lines, curl_command, curl_lines, draw, exchange_summary,
        header_detail_lines, human_size, is_binary_content_type, layout_mode, retained_entry_bytes,
        search_input_line, stop_server_and_drain, truncate,
    };
    use crate::model::{
        ContractCheck, ContractViolation, Endpoint, ExchangePart, HeaderValue, LogEntry,
    };
    use crate::server::CaptureServer;
    use crate::session::{SessionRecorder, load};

    fn endpoint(method: &str, path: &str) -> Endpoint {
        Endpoint::new(method, path)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn draw_at(app: &mut App, width: u16, height: u16) {
        let _ = rendered_at(app, width, height);
    }

    fn rendered_at(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for row in 0..height {
            for column in 0..width {
                if let Some(cell) = buffer.cell((column, row)) {
                    rendered.push_str(cell.symbol());
                }
            }
            rendered.push('\n');
        }
        rendered
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

    fn checked_log(method: &str, path: &str, status: u16, latency_ms: u128) -> LogEntry {
        LogEntry {
            method: method.into(),
            path: path.into(),
            status,
            latency_ms,
            contract: ContractCheck {
                checked: true,
                matched_endpoint: Some(format!("{method} {path}")),
                violations: Vec::new(),
                ..ContractCheck::default()
            },
            ..LogEntry::default()
        }
    }

    #[test]
    fn layout_modes_cover_supported_terminal_sizes() {
        assert_eq!(layout_mode(Rect::new(0, 0, 59, 20)), LayoutMode::TooSmall);
        assert_eq!(layout_mode(Rect::new(0, 0, 100, 11)), LayoutMode::TooSmall);
        assert_eq!(layout_mode(Rect::new(0, 0, 60, 12)), LayoutMode::Narrow);
        assert_eq!(layout_mode(Rect::new(0, 0, 100, 15)), LayoutMode::Narrow);
        assert_eq!(layout_mode(Rect::new(0, 0, 88, 16)), LayoutMode::Compact);
        assert_eq!(layout_mode(Rect::new(0, 0, 120, 18)), LayoutMode::Wide);
    }

    #[test]
    fn adaptive_workspace_prioritizes_content_and_opens_server_as_a_drawer() {
        let mut app = App::new(vec![endpoint("GET", "/users")]);
        draw_at(&mut app, 120, 24);
        assert_eq!(app.areas.layout_mode, LayoutMode::Wide);
        assert!(app.areas.endpoints_pane.width > 0);
        assert!(app.areas.logs_pane.width > app.areas.endpoints_pane.width);
        assert_eq!(app.areas.server_pane, Rect::default());
        let errors_filter = app.areas.traffic_filters[2];
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            errors_filter.x + 1,
            errors_filter.y,
        ));
        assert_eq!(app.traffic_view, TrafficView::Errors);

        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        draw_at(&mut app, 120, 24);
        assert_eq!(app.focus, FocusPane::Server);
        assert_eq!(app.areas.server_pane.width, 120);
        assert!(app.areas.server_pane.y > app.areas.logs_pane.y);

        let mut narrow = App::new(vec![endpoint("GET", "/users")]);
        draw_at(&mut narrow, 70, 20);
        assert_eq!(narrow.areas.layout_mode, LayoutMode::Narrow);
        assert!(narrow.areas.endpoints_pane.width > 0);
        assert_eq!(narrow.areas.logs_pane, Rect::default());
        let requests_tab = narrow.areas.pane_tabs[1];
        narrow.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            requests_tab.x + 1,
            requests_tab.y,
        ));
        draw_at(&mut narrow, 70, 20);
        assert_eq!(narrow.focus, FocusPane::Logs);
        assert_eq!(narrow.areas.endpoints_pane, Rect::default());
        assert!(narrow.areas.logs_pane.width > 0);

        let server_tab = narrow.areas.pane_tabs[2];
        narrow.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            server_tab.x + 1,
            server_tab.y,
        ));
        assert!(narrow.show_server);
        draw_at(&mut narrow, 70, 20);
        let requests_tab = narrow.areas.pane_tabs[1];
        narrow.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            requests_tab.x + 1,
            requests_tab.y,
        ));
        assert!(!narrow.show_server);
        narrow.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert!(narrow.show_server);
        assert_eq!(narrow.focus, FocusPane::Server);
    }

    #[test]
    fn traffic_filter_hitboxes_fit_populated_breakpoint_layouts() {
        let logs: Vec<_> = (0..150)
            .map(|index| checked_log("GET", &format!("/users/{index}"), 200, 10))
            .collect();
        for (width, height) in [(60, 12), (88, 16), (120, 18)] {
            let mut app =
                App::with_logs(vec![endpoint("GET", "/users/{id}")], logs.clone()).unwrap();
            app.set_focus(FocusPane::Logs);
            let rendered = rendered_at(&mut app, width, height);
            assert!(app.areas.traffic_filters.iter().all(|area| area.width > 0));
            assert!(
                app.areas.traffic_filters[4].right()
                    <= app.areas.logs_pane.right().saturating_sub(1)
            );
            assert!(
                rendered.contains("5M")
                    || rendered.contains("5 Miss")
                    || rendered.contains("5 Unmatched")
            );
        }
    }

    #[test]
    fn search_preserves_existing_filter_and_supports_apply_or_cancel() {
        let mut app = App::new(vec![endpoint("GET", "/users"), endpoint("GET", "/health")]);
        app.endpoint_search_query = "users".into();
        app.filter_endpoints();
        app.detail_tab = DetailTab::Contract;
        app.detail_scroll = 4;
        app.set_focus(FocusPane::Endpoints);

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(app.endpoint_search_query, "users");
        assert_eq!(app.endpoint_search_cursor, 5);
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "health".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(app.filtered, vec![1]);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.endpoint_search_query, "users");
        assert_eq!(app.filtered, vec![0]);
        assert_eq!(app.detail_tab, DetailTab::Contract);
        assert_eq!(app.detail_scroll, 4);
        assert!(!app.detail_expanded);

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "health".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.endpoint_search_query, "health");
        assert_eq!(app.filtered, vec![1]);
        assert!(app.search_target.is_none());

        let active = line_text(&[search_input_line("health", 3, true, "", 1, 2, 40)]);
        assert!(active.contains('▏'));
    }

    #[test]
    fn active_search_ignores_mouse_and_restores_live_follow_state() {
        let mut app = App::with_logs(
            vec![endpoint("GET", "/users"), endpoint("GET", "/health")],
            vec![checked_log("GET", "/users", 200, 10)],
        )
        .unwrap();
        app.endpoint_search_query = "users".into();
        app.filter_endpoints();
        app.follow_live = true;
        app.set_focus(FocusPane::Endpoints);
        app.begin_search(SearchTarget::Endpoints);
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "health".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.areas.traffic_search = Rect::new(20, 2, 20, 1);
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            app.areas.traffic_search.x,
            app.areas.traffic_search.y,
        ));
        assert_eq!(app.search_target, Some(SearchTarget::Endpoints));
        assert_eq!(app.focus, FocusPane::Endpoints);

        app.push_log(checked_log("GET", "/users", 200, 20)).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.endpoint_search_query, "users");
        assert_eq!(app.filtered, vec![0]);
        assert!(app.follow_live);
        assert_eq!(app.selected_exchange, 1);
    }

    #[test]
    fn notices_expire_without_hiding_status_permanently() {
        let mut app = App::new(vec![endpoint("GET", "/users")]);
        app.set_notice("Saved");
        let deadline = app.notice_expires_at.unwrap();
        app.expire_notice_at(deadline - Duration::from_millis(1));
        assert_eq!(app.notice.as_deref(), Some("Saved"));
        app.expire_notice_at(deadline);
        assert!(app.notice.is_none());

        app.set_notice("Newer");
        assert!(app.notice_expires_at.unwrap() > Instant::now());

        app.set_sticky_notice("Restart failed");
        app.expire_notice_at(Instant::now() + NOTICE_TTL + Duration::from_secs(1));
        assert_eq!(app.notice.as_deref(), Some("Restart failed"));
        assert!(app.notice_expires_at.is_none());
    }

    #[test]
    fn wrapped_detail_scroll_is_measured_and_clamped_in_visual_rows() {
        let mut entry = checked_log("GET", "/users", 200, 10);
        entry.response.body = format!("{}TRAILING_SENTINEL", "wrapped-content ".repeat(300));
        entry.response.size = entry.response.body.len();
        let mut app = App::with_logs(vec![endpoint("GET", "/users")], vec![entry]).unwrap();
        app.set_focus(FocusPane::Logs);
        app.detail_tab = DetailTab::Response;
        app.wrap_bodies = true;
        app.detail_expanded = true;
        app.detail_scroll = usize::MAX;

        let rendered = rendered_at(&mut app, 60, 12);
        assert!(app.detail_max_scroll > 0);
        assert_eq!(app.detail_scroll, app.detail_max_scroll);
        assert!(rendered.contains("TRAILING_SENTINEL"));
        let narrow_max = app.detail_max_scroll;

        let rendered = rendered_at(&mut app, 120, 30);
        assert!(app.detail_max_scroll < narrow_max);
        assert_eq!(app.detail_scroll, app.detail_max_scroll);
        assert!(rendered.contains("TRAILING_SENTINEL"));
    }

    #[test]
    fn help_is_scrollable_at_the_minimum_supported_height() {
        let mut app = App::new(vec![endpoint("GET", "/users")]);
        app.show_help = true;
        draw_at(&mut app, 60, 12);
        assert!(app.help_max_scroll > 0);
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.help_scroll, app.help_max_scroll);
        let rendered = rendered_at(&mut app, 60, 12);
        assert!(rendered.contains("closes help"));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.show_help);
    }

    #[test]
    fn wrapped_server_output_reaches_its_trailing_content() {
        let mut app = App::new(vec![endpoint("GET", "/users")]);
        app.output = vec![format!("{}OUTPUT_SENTINEL", "server-output ".repeat(100))];
        app.show_server = true;
        app.set_focus(FocusPane::Server);
        let rendered = rendered_at(&mut app, 60, 12);
        assert!(app.output_max_scroll > 0);
        assert!(rendered.contains("OUTPUT_SENTINEL"));
    }

    #[test]
    fn expanded_detail_does_not_route_input_to_hidden_panes() {
        let mut app = App::with_logs(
            vec![endpoint("GET", "/users")],
            vec![checked_log("GET", "/users", 200, 10)],
        )
        .unwrap();
        app.set_focus(FocusPane::Logs);
        app.detail_expanded = true;

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert!(app.search_target.is_none());
        assert_eq!(app.focus, FocusPane::Logs);
        assert!(!app.show_server);

        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(!app.detail_expanded);
    }

    #[test]
    fn replay_confirmation_stays_visible_when_terminal_becomes_too_small() {
        let mut entry = checked_log(
            "post",
            &format!("/danger/{}", "very-long-target-".repeat(12)),
            201,
            10,
        );
        entry
            .request
            .headers
            .insert("Host".into(), "captured.example.test".into());
        let mut app = App::with_logs(vec![endpoint("POST", "/danger/{id}")], vec![entry]).unwrap();
        app.set_focus(FocusPane::Logs);
        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT));

        let rendered = rendered_at(&mut app, 59, 11);
        assert_eq!(app.areas.layout_mode, LayoutMode::TooSmall);
        assert!(rendered.contains("Confirm replay"));
        assert!(rendered.contains("Warning: this method may change upstream data."));
        assert!(rendered.contains("Captured Host:"));
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Replay(_)
        ));
    }

    #[test]
    fn display_history_is_bounded_without_save() {
        let initial_count = DISPLAY_LOG_LIMIT + 50;
        let logs: Vec<_> = (0..initial_count)
            .map(|index| checked_log("GET", &format!("/items/{index}"), 200, 1))
            .collect();
        let mut app = App::with_logs(vec![endpoint("GET", "/items/{id}")], logs).unwrap();
        assert_eq!(app.logs.len(), DISPLAY_LOG_LIMIT);
        assert_eq!(app.logs[0].path, "/items/50");

        app.push_log(checked_log(
            "GET",
            &format!("/items/{initial_count}"),
            200,
            1,
        ))
        .unwrap();
        assert_eq!(app.logs.len(), DISPLAY_LOG_LIMIT);
        assert!(app.recorder.is_none());
        assert_eq!(app.history_len, initial_count + 1);
        assert_eq!(
            app.logs.back().unwrap().path,
            format!("/items/{initial_count}")
        );
    }

    #[test]
    fn retention_clamps_selection_when_a_filtered_arrival_evicts_a_visible_row() {
        let logs: Vec<_> = (0..DISPLAY_LOG_LIMIT)
            .map(|index| checked_log("GET", &format!("/keep/{index}"), 200, 1))
            .collect();
        let mut app = App::with_logs(vec![endpoint("GET", "/keep/{id}")], logs).unwrap();
        assert!(app.follow_live);
        assert_eq!(app.selected_exchange, DISPLAY_LOG_LIMIT - 1);

        app.push_log(checked_log("GET", "/hidden", 200, 1)).unwrap();
        assert_eq!(app.visible_log_indices().len(), DISPLAY_LOG_LIMIT - 1);
        assert_eq!(app.selected_exchange, DISPLAY_LOG_LIMIT - 2);
    }

    #[test]
    fn streamed_save_preserves_complete_history_beyond_the_display_window() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("complete.json");
        let initial_count = DISPLAY_LOG_LIMIT + 10;
        let logs: Vec<_> = (0..initial_count)
            .map(|index| checked_log("GET", &format!("/items/{index}"), 200, 1))
            .collect();
        let recorder = SessionRecorder::new(&path).unwrap();
        let mut app =
            App::with_recorder(vec![endpoint("GET", "/items/{id}")], logs, Some(recorder)).unwrap();
        app.push_log(checked_log(
            "GET",
            &format!("/items/{initial_count}"),
            200,
            1,
        ))
        .unwrap();
        assert_eq!(app.logs.len(), DISPLAY_LOG_LIMIT);
        app.finish().unwrap();

        let history = load(&path).unwrap();
        assert_eq!(history.len(), initial_count + 1);
        assert_eq!(history[0].path, "/items/0");
        assert_eq!(
            history.last().unwrap().path,
            format!("/items/{initial_count}")
        );
    }

    #[test]
    fn clean_shutdown_drains_backpressured_logs_into_the_session() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("shutdown.jsonl");
        let recorder = SessionRecorder::new(&path).unwrap();
        let mut app = App::with_recorder(Vec::new(), Vec::new(), Some(recorder)).unwrap();
        let (output_tx, output_rx) = mpsc::sync_channel(32);
        let (logs_tx, logs_rx) = mpsc::sync_channel(1);
        let mut server =
            CaptureServer::new("127.0.0.1:0".into(), None, output_tx, logs_tx).unwrap();
        let address = server.start().unwrap();
        for route in ["one", "two"] {
            reqwest::blocking::get(format!("http://{address}/{route}"))
                .unwrap()
                .text()
                .unwrap();
        }

        stop_server_and_drain(&mut app, &mut server, &output_rx, &logs_rx).unwrap();
        assert_eq!(app.history_len, 2);
        app.finish().unwrap();
        let saved = load(&path).unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].path, "/one");
        assert_eq!(saved[1].path, "/two");
    }

    #[test]
    fn display_history_also_obeys_a_byte_budget() {
        let mut app = App::new(vec![endpoint("GET", "/items/{id}")]);
        let mut first = checked_log("GET", "/items/1", 200, 1);
        first.response.body = "a".repeat(64);
        let per_entry = retained_entry_bytes(&first);
        app.display_byte_limit = per_entry * 2;
        app.push_log(first).unwrap();
        for index in 2..=3 {
            let mut entry = checked_log("GET", &format!("/items/{index}"), 200, 1);
            entry.response.body = "a".repeat(64);
            app.push_log(entry).unwrap();
        }

        assert_eq!(app.logs.len(), 2);
        assert_eq!(app.logs.front().unwrap().path, "/items/2");
        assert_eq!(app.logs.back().unwrap().path, "/items/3");
        assert!(app.display_bytes <= app.display_byte_limit);
    }

    #[test]
    fn loaded_sessions_are_revalidated_against_the_current_spec() {
        let previously_valid = checked_log("GET", "/removed", 200, 1);
        assert!(previously_valid.contract.is_valid());

        let app =
            App::with_logs(vec![endpoint("GET", "/current")], vec![previously_valid]).unwrap();
        assert!(!app.logs[0].contract.is_valid());
        assert_eq!(
            app.logs[0].contract.violations[0].code,
            "undocumented_operation"
        );
    }

    #[test]
    fn loaded_redacted_contract_results_are_preserved_but_marked_partial() {
        let mut previously_valid = checked_log("GET", "/removed", 200, 1);
        previously_valid.request.body = r#"{"token":"[REDACTED]"}"#.into();
        previously_valid
            .contract
            .violations
            .push(ContractViolation::new(
                "saved_finding",
                "response.body",
                "saved validation finding",
            ));
        let mut app =
            App::with_logs(vec![endpoint("GET", "/current")], vec![previously_valid]).unwrap();
        assert!(app.logs[0].contract.checked);
        assert!(app.logs[0].contract.inconclusive);
        assert_eq!(app.logs[0].contract.violations.len(), 2);
        assert_eq!(app.logs[0].contract.violations[0].code, "saved_finding");
        assert_eq!(
            app.logs[0].contract.violations[1].code,
            "validation_inconclusive"
        );
        let details = line_text(&contract_lines(&app.logs[0]));
        assert!(details.contains("Validation is also partial"));
        assert!(!details.contains("No contract violations"));
        app.traffic_view = TrafficView::All;
        app.traffic_search_query = "partial".into();
        assert_eq!(app.visible_log_indices(), vec![0]);
        app.traffic_search_query.clear();
        app.traffic_view = TrafficView::Errors;
        assert_eq!(app.visible_log_indices(), vec![0]);

        let unchecked = LogEntry {
            method: "POST".into(),
            path: "/masked".into(),
            request: ExchangePart {
                body: "[REDACTED]".into(),
                ..ExchangePart::default()
            },
            ..LogEntry::default()
        };
        let app = App::with_logs(vec![endpoint("POST", "/masked")], vec![unchecked]).unwrap();
        assert!(app.logs[0].contract.checked);
        assert!(app.logs[0].contract.inconclusive);
    }

    #[test]
    fn filters_endpoints_and_edits_unicode_search() {
        let mut app = App::new(vec![
            endpoint("GET", "/cafe"),
            endpoint("POST", "/cafeteria"),
        ]);
        app.focus = FocusPane::Endpoints;
        app.search_target = Some(SearchTarget::Endpoints);
        app.handle_key(KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE));
        assert_eq!(app.endpoint_search_query, "é");
        assert!(app.filtered.is_empty());
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.endpoint_search_query, "");
        assert_eq!(app.filtered.len(), 2);
    }

    #[test]
    fn selected_logs_match_path_templates_and_follow_latest() {
        let mut app = App::new(vec![endpoint("GET", "/users/{id}")]);
        app.push_log(LogEntry {
            method: "GET".into(),
            path: "/users/41".into(),
            ..LogEntry::default()
        })
        .unwrap();
        app.push_log(LogEntry {
            method: "POST".into(),
            path: "/users/42".into(),
            ..LogEntry::default()
        })
        .unwrap();
        app.push_log(LogEntry {
            method: "GET".into(),
            path: "/users/42".into(),
            ..LogEntry::default()
        })
        .unwrap();
        assert_eq!(app.visible_log_indices().len(), 2);
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
    fn traffic_views_cover_all_unmatched_errors_and_slow_requests() {
        let mut app = App::new(vec![endpoint("GET", "/users")]);
        app.push_log(checked_log("GET", "/users", 200, 20)).unwrap();
        app.push_log(checked_log("POST", "/other", 200, 30))
            .unwrap();
        app.push_log(checked_log("GET", "/users", 503, 40)).unwrap();
        app.push_log(checked_log("GET", "/users", 200, 700))
            .unwrap();

        assert_eq!(app.visible_log_indices().len(), 3);
        app.traffic_view = TrafficView::All;
        assert_eq!(app.visible_log_indices().len(), 4);
        app.traffic_view = TrafficView::Unmatched;
        assert_eq!(app.visible_log_indices(), vec![1]);
        app.traffic_view = TrafficView::Errors;
        assert_eq!(app.visible_log_indices(), vec![2]);
        app.traffic_view = TrafficView::Slow;
        assert_eq!(app.visible_log_indices(), vec![3]);

        app.focus = FocusPane::Logs;
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(app.traffic_view, TrafficView::Unmatched);
        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(app.traffic_view, TrafficView::Selected);
    }

    #[test]
    fn traffic_search_matches_query_headers_bodies_and_status() {
        let mut app = App::new(vec![endpoint("POST", "/users")]);
        let mut ada = checked_log("POST", "/users", 201, 25);
        ada.query = Some("notify=true".into());
        ada.request
            .headers
            .insert("X-Trace".into(), "violet".into());
        ada.request.body = r#"{"name":"Ada"}"#.into();
        app.push_log(ada).unwrap();
        app.push_log(checked_log("POST", "/users", 409, 12))
            .unwrap();
        app.traffic_view = TrafficView::All;

        for query in ["notify", "x-trace", "violet", "ada", "201"] {
            app.traffic_search_query = query.into();
            assert_eq!(app.visible_log_indices(), vec![0], "query: {query}");
        }

        app.focus = FocusPane::Logs;
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE));
        assert_eq!(app.visible_log_indices(), vec![1]);
    }

    #[test]
    fn endpoint_search_includes_summary_tags_and_operation_id() {
        let mut users = endpoint("GET", "/users");
        users.summary = Some("List every customer".into());
        users.tags = vec!["accounts".into()];
        users.operation_id = Some("listUsers".into());
        let mut app = App::new(vec![users, endpoint("GET", "/health")]);

        for query in ["customer", "accounts", "listusers"] {
            app.endpoint_search_query = query.into();
            app.filter_endpoints();
            assert_eq!(app.filtered, vec![0], "query: {query}");
        }
    }

    #[test]
    fn paused_live_follow_keeps_selection_until_resumed() {
        let mut app = App::new(vec![endpoint("GET", "/users")]);
        app.push_log(checked_log("GET", "/users", 200, 10)).unwrap();
        app.push_log(checked_log("GET", "/users", 200, 20)).unwrap();
        app.selected_exchange = 0;
        app.follow_live = false;
        app.push_log(checked_log("GET", "/users", 200, 30)).unwrap();
        assert_eq!(app.selected_exchange, 0);

        app.focus = FocusPane::Logs;
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert!(app.follow_live);
        assert_eq!(app.selected_exchange, 2);
    }

    #[test]
    fn contract_tab_lists_violations_and_replay_returns_selected_exchange() {
        let mut entry = checked_log("GET", "/users", 200, 15);
        entry.contract.violations.push(ContractViolation {
            code: "response.status".into(),
            location: "response.status".into(),
            message: "status 200 is undocumented".into(),
        });
        let text = line_text(&contract_lines(&entry));
        assert!(text.contains("1 definite contract violation"));
        assert!(text.contains("status 200 is undocumented"));

        entry.contract.inconclusive = true;
        entry.contract.violations.push(ContractViolation::new(
            "validation_inconclusive",
            "response.body",
            "body was truncated",
        ));
        let mixed = line_text(&contract_lines(&entry));
        assert!(mixed.contains("1 definite contract violation"));
        assert!(mixed.contains("Validation is also partial"));
        assert!(line_text(&[exchange_summary(&entry, 120)]).contains("!1~"));

        let mut app = App::with_logs(vec![endpoint("GET", "/users")], vec![entry]).unwrap();
        app.focus = FocusPane::Logs;
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT)),
            Action::Continue
        ));
        assert!(app.replay_confirmation.is_some());
        app.logs[0].path = "/selection-changed".into();
        match app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)) {
            Action::Replay(entry) => assert_eq!(entry.path, "/users"),
            _ => panic!("expected replay action"),
        }
    }

    #[test]
    fn curl_command_preserves_query_headers_body_and_shell_quotes() {
        let mut entry = checked_log("POST", "/users", 201, 10);
        entry.query = Some("notify=true".into());
        entry
            .request
            .headers
            .insert("Host".into(), "localhost:4000".into());
        entry
            .request
            .headers
            .insert("Content-Type".into(), "application/json".into());
        entry.request.body = r#"{"name":"O'Reilly"}"#.into();
        let command = curl_command(&entry).unwrap();
        assert!(command.contains("-X 'POST'"));
        assert!(command.contains("http://localhost:4000/users?notify=true"));
        assert!(command.contains("Content-Type: application/json"));
        assert!(command.contains("O'\\''Reilly"));
    }

    #[test]
    fn header_ui_search_and_curl_prefer_exact_repeated_values() {
        let mut entry = checked_log("GET", "/headers", 200, 1);
        entry
            .request
            .headers
            .insert("X-Repeat".into(), "flattened-only".into());
        entry.request.header_values = vec![
            HeaderValue::new("Host", "exact.test"),
            HeaderValue::new("X-Repeat", "one"),
            HeaderValue::new("X-Repeat", "two"),
        ];

        let command = curl_command(&entry).unwrap();
        assert!(command.contains("http://exact.test/headers"));
        assert!(command.contains("X-Repeat: one"));
        assert!(command.contains("X-Repeat: two"));
        assert!(!command.contains("flattened-only"));
        let details = line_text(&header_detail_lines(&entry));
        assert!(details.contains("Request headers (3)"));
        assert_eq!(details.matches("X-Repeat:").count(), 2);

        let mut app = App::with_logs(vec![endpoint("GET", "/headers")], vec![entry]).unwrap();
        app.traffic_view = TrafficView::All;
        app.traffic_search_query = "two".into();
        assert_eq!(app.visible_log_indices(), vec![0]);
        app.traffic_search_query = "flattened-only".into();
        assert!(app.visible_log_indices().is_empty());
    }

    #[test]
    fn curl_and_ui_replay_reject_invalid_or_unfaithful_captures() {
        let mut invalid_method = checked_log("GET; echo injected", "/users", 200, 1);
        assert_eq!(
            curl_command(&invalid_method).unwrap_err(),
            "the captured HTTP method is invalid"
        );

        invalid_method.method = "POST".into();
        invalid_method.request.truncated = true;
        assert_eq!(
            curl_command(&invalid_method).unwrap_err(),
            "the request body capture is truncated"
        );

        let mut lossy = checked_log("POST", "/users", 200, 1);
        lossy.request.body = "bad \u{fffd} bytes".into();
        assert!(curl_command(&lossy).unwrap_err().contains("lossy UTF-8"));

        let mut redacted = checked_log("POST", "/users", 200, 1);
        redacted.query = Some("token=%5BREDACTED%5D".into());
        let explanation = line_text(&curl_lines(&redacted));
        assert!(explanation.contains("cURL unavailable"));
        assert!(explanation.contains("redacted values"));
        assert!(!explanation.contains("curl -i"));

        let mut app = App::with_logs(vec![endpoint("POST", "/users")], vec![redacted]).unwrap();
        app.focus = FocusPane::Logs;
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT)),
            Action::Continue
        ));
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("Replay unavailable")
        );
    }

    #[test]
    fn redaction_markers_in_headers_and_bodies_disable_curl() {
        let mut header = checked_log("POST", "/users", 200, 1);
        header
            .request
            .headers
            .insert("Authorization".into(), "[REDACTED]".into());
        assert!(curl_command(&header).unwrap_err().contains("redacted"));

        let mut exact_header = checked_log("POST", "/users", 200, 1);
        exact_header.request.header_values =
            vec![HeaderValue::new("Authorization", "Bearer [REDACTED]")];
        assert!(
            curl_command(&exact_header)
                .unwrap_err()
                .contains("redacted")
        );

        let mut body = checked_log("POST", "/users", 200, 1);
        body.request.body = r#"{"password":"[REDACTED]"}"#.into();
        assert!(curl_command(&body).unwrap_err().contains("redacted"));
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
        ]
        .into();
        app.areas.exchanges_list = Rect::new(25, 2, 30, 5);
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 27, 3));
        assert_eq!(app.selected_exchange, 1);
        assert_eq!(app.focus, FocusPane::Logs);

        app.areas.tabs = [
            Rect::new(25, 8, 9, 1),
            Rect::new(35, 8, 10, 1),
            Rect::new(46, 8, 9, 1),
            Rect::new(56, 8, 10, 1),
            Rect::new(67, 8, 6, 1),
        ];
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 48, 8));
        assert_eq!(app.detail_tab, DetailTab::Headers);
    }

    #[test]
    fn mouse_wheel_targets_the_pane_under_the_pointer() {
        let mut app = App::new(vec![endpoint("GET", "/users")]);
        app.areas.detail = Rect::new(20, 8, 40, 12);
        app.detail_max_scroll = 20;
        app.detail_scroll = 6;
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 30, 10));
        assert_eq!(app.detail_scroll, 3);
        assert_eq!(app.focus, FocusPane::Logs);
        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 30, 10));
        assert_eq!(app.detail_scroll, 6);

        app.output_scroll = 2;
        app.output_max_scroll = 20;
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
