//! The realtime in-flight-run dashboard and the console's screen stack.
//!
//! The app polls a supplied `poll` closure (~1s in production,
//! deterministic in tests) and renders one [`DashboardSnapshot`] per tick.
//! Rows that disappear from the live set linger dimmed for
//! `linger_seconds` before dropping; a polling failure leaves the last good
//! frame on screen with a status-bar warning rather than crashing the app.
//!
//! A persistent input bar at the bottom carries plain-text filtering and
//! the shared [`crate::slash`] vocabulary. The input bar defers to the
//! table for arrows / Enter / q / ? / h when unfocused so keyboard
//! navigation survives unchanged; focus moves to the input on demand via
//! `ctrl+i` (or `tab`, which terminals deliver for the same chord).
//!
//! The app also owns the screen stack (session and history screens pushed
//! on top of the dashboard), the quit-handoff modal, and the
//! [`DaemonControl`] handle the `/worker` and `/autopilot` verbs drive.
//! Everything is pure state plus [`DashboardApp::render`] /
//! [`DashboardApp::handle_key`] / [`DashboardApp::on_timer`], so the whole
//! console is testable headless with ratatui's `TestBackend`.

use crate::history_screen::{HistoryAction, HistoryScreen};
use crate::quit_prompt::{QuitChoice, QuitPrompt, running_daemons_label};
use crate::session_screen::{
    ArchiveAction, DEFAULT_SESSION_POLL_INTERVAL_SECONDS, SessionAction, SessionScreen,
};
use crate::slash::{
    SLASH_APPROVE, SLASH_ARCHIVE, SLASH_AUTOPILOT, SLASH_EXIT, SLASH_HELP, SLASH_HISTORY,
    SLASH_INTERRUPT, SLASH_QUIT, SLASH_REJECT, SLASH_STATUS, SLASH_WORKER, help_text, is_slash,
    parse_slash, unknown_command_notice,
};
use crate::snapshot::{DashboardSnapshot, RowSnapshot, SummaryData};
use crate::tui::{
    DAEMON_AUTOPILOT, DAEMON_WORKER, DaemonControl, DaemonState, DaemonStatus, InputBar, InputEvent,
};
use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use flywheel_core::invoker_client::{
    CONTROL_COMMAND_APPROVE, CONTROL_COMMAND_INTERRUPT, CONTROL_COMMAND_REJECT,
};
use flywheel_core::lifecycle::Status;
use flywheel_core::store::JsonObject;
use flywheel_orchestrator::autopilot_activity::{
    AutopilotActivity, PHASE_IDLE, PHASE_RUNNING, PHASE_STARTING,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// How long a row that just left the active set stays dimmed on screen.
pub const DEFAULT_LINGER_SECONDS: i64 = 30;

/// How often the dashboard re-polls the store.
pub const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 1.0;

/// The `?` help footer.
pub const HELP_LINES: [&str; 10] = [
    "key bindings",
    "  up/down     move row selection",
    "  enter       open session view for the selected run",
    "  escape      close the session view (back to dashboard)",
    "  h           open the finished-run history view",
    "  ctrl+i      focus the input bar (filter + slash commands)",
    "  q / ctrl+c  quit",
    "  ?           toggle this help footer",
    "",
    "input bar: plain text filters rows; /help lists slash commands.",
];

/// [`HELP_LINES`] joined with newlines.
pub fn dashboard_help_text() -> String {
    HELP_LINES.join("\n")
}

/// `(key, label)` pairs: keys are the stable cell identifiers, labels the
/// rendered header.
pub const TABLE_COLUMNS: [(&str, &str); 7] = [
    ("task_id", "task"),
    ("status", "status"),
    ("pos", "attempt/iter"),
    ("age", "age"),
    ("tokens", "tokens"),
    ("cost", "cost"),
    ("last_action", "last action"),
];

/// The input bar's placeholder.
pub const INPUT_PLACEHOLDER: &str = "filter rows, or /help for slash commands";

/// Last-action details can run long; cap the cell so one chatty run does
/// not push every other column off a narrow terminal.
pub const MAX_DETAIL_WIDTH: usize = 60;

/// Most lines the inline notice occupies (the `/help` listing fits).
const MAX_NOTICE_ROWS: u16 = 12;

/// Statuses against which the watcher applies `interrupt`.
pub const STEERABLE_STATUSES: [Status; 2] = [Status::Running, Status::Validating];
/// The status at which a parked manual gate accepts approve / reject.
pub const APPROVABLE_STATUSES: [Status; 1] = [Status::AwaitingApproval];

/// Returns one dashboard frame; an `Err` keeps the last good frame.
pub type PollSnapshot = Box<dyn FnMut() -> Result<DashboardSnapshot, String>>;
/// Builds the session screen for `(run_id, task_id)`; `None` disables Enter.
pub type OpenSession = Box<dyn FnMut(&str, &str) -> Option<SessionScreen>>;
/// Builds the history screen; `None` disables `h` / `/history`.
pub type OpenHistory = Box<dyn FnMut() -> Option<HistoryScreen>>;
/// Producer seam: `(run_id, kind, payload)` -> the store-assigned command id.
pub type EnqueueForRun = Box<dyn FnMut(&str, &str, JsonObject) -> Result<i64, String>>;
/// The wall clock the linger math reads.
pub type Clock = Box<dyn FnMut() -> DateTime<Utc>>;

/// Per-run state carried across polls.
///
/// `last_active_at` is the wall clock of the last poll that observed the
/// run; once `active` flips to `false` the row renders dimmed and after
/// `linger_seconds` it is dropped. The plain cells and the lowercase
/// filter haystack are computed once per observation, not once per
/// render.
#[derive(Debug, Clone)]
struct RowMemo {
    snapshot: RowSnapshot,
    cells: [String; 7],
    haystack: String,
    last_active_at: DateTime<Utc>,
    active: bool,
}

/// One row the table currently shows.
#[derive(Debug, Clone)]
struct VisibleRow {
    run_id: String,
    cells: [String; 7],
    active: bool,
}

/// Which widget owns the keyboard on the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Table,
    Input,
}

/// A screen pushed on top of the dashboard.
pub enum Screen {
    Session(Box<SessionScreen>),
    History(Box<HistoryScreen>),
}

/// The realtime in-flight-run dashboard plus the console's screen stack.
///
/// The constructor takes a `poll` closure rather than a store handle so
/// tests drive deterministic frames without touching SQLite. The optional
/// seams (`open_session`, `open_history`, `enqueue`, `archive`,
/// `daemons`) make the corresponding verbs degrade to an inline "not
/// wired" notice when absent so snapshot-only tests stay light.
pub struct DashboardApp {
    poll: PollSnapshot,
    poll_interval: Duration,
    session_poll_interval: Duration,
    linger_seconds: i64,
    clock: Clock,
    open_session: Option<OpenSession>,
    open_history: Option<OpenHistory>,
    enqueue: Option<EnqueueForRun>,
    archive: Option<ArchiveAction>,
    daemons: Option<Box<dyn DaemonControl>>,
    last_worker_status: Option<DaemonStatus>,
    last_autopilot_status: Option<DaemonStatus>,
    memo: BTreeMap<(String, String), RowMemo>,
    last_snapshot: Option<DashboardSnapshot>,
    last_error: Option<String>,
    visible: Vec<VisibleRow>,
    cursor: usize,
    help_visible: bool,
    filter: String,
    needle: String,
    notice: Option<String>,
    input: InputBar,
    focus: Focus,
    quit_prompt: Option<QuitPrompt>,
    stack: Vec<Screen>,
    exit_code: Option<i32>,
    last_poll: Option<Instant>,
    last_session_poll: Option<Instant>,
}

impl DashboardApp {
    pub fn new(poll: impl FnMut() -> Result<DashboardSnapshot, String> + 'static) -> Self {
        Self {
            poll: Box::new(poll),
            poll_interval: Duration::from_secs_f64(DEFAULT_POLL_INTERVAL_SECONDS),
            session_poll_interval: Duration::from_secs_f64(DEFAULT_SESSION_POLL_INTERVAL_SECONDS),
            linger_seconds: DEFAULT_LINGER_SECONDS,
            clock: Box::new(Utc::now),
            open_session: None,
            open_history: None,
            enqueue: None,
            archive: None,
            daemons: None,
            last_worker_status: None,
            last_autopilot_status: None,
            memo: BTreeMap::new(),
            last_snapshot: None,
            last_error: None,
            visible: Vec::new(),
            cursor: 0,
            help_visible: false,
            filter: String::new(),
            needle: String::new(),
            notice: None,
            input: InputBar::new(INPUT_PLACEHOLDER),
            focus: Focus::Table,
            quit_prompt: None,
            stack: Vec::new(),
            exit_code: None,
            last_poll: None,
            last_session_poll: None,
        }
    }

    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    pub fn with_session_poll_interval(mut self, interval: Duration) -> Self {
        self.session_poll_interval = interval;
        self
    }

    pub fn with_linger_seconds(mut self, seconds: i64) -> Self {
        self.linger_seconds = seconds;
        self
    }

    pub fn with_clock(mut self, clock: impl FnMut() -> DateTime<Utc> + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    pub fn with_open_session(
        mut self,
        factory: impl FnMut(&str, &str) -> Option<SessionScreen> + 'static,
    ) -> Self {
        self.open_session = Some(Box::new(factory));
        self
    }

    pub fn with_open_history(
        mut self,
        factory: impl FnMut() -> Option<HistoryScreen> + 'static,
    ) -> Self {
        self.open_history = Some(Box::new(factory));
        self
    }

    pub fn with_enqueue(
        mut self,
        enqueue: impl FnMut(&str, &str, JsonObject) -> Result<i64, String> + 'static,
    ) -> Self {
        self.enqueue = Some(Box::new(enqueue));
        self
    }

    pub fn with_archive(
        mut self,
        archive: impl FnMut() -> Result<Vec<String>, String> + 'static,
    ) -> Self {
        self.archive = Some(Box::new(archive));
        self
    }

    pub fn with_daemons(mut self, daemons: Box<dyn DaemonControl>) -> Self {
        self.daemons = Some(daemons);
        self
    }

    // ----- Observable state ----------------------------------------------------

    /// The active row-filter substring.
    pub fn filter_text(&self) -> &str {
        &self.filter
    }

    /// The current inline notice text, if any.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// The most recent worker status, or `None` if no control is wired.
    pub fn worker_status(&self) -> Option<&DaemonStatus> {
        self.last_worker_status.as_ref()
    }

    /// The most recent autopilot status, or `None` if no control is wired.
    pub fn autopilot_status(&self) -> Option<&DaemonStatus> {
        self.last_autopilot_status.as_ref()
    }

    /// The run ids the table shows, in display order.
    pub fn visible_run_order(&self) -> Vec<String> {
        self.visible.iter().map(|r| r.run_id.clone()).collect()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn help_visible(&self) -> bool {
        self.help_visible
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn input(&self) -> &InputBar {
        &self.input
    }

    pub fn quit_prompt(&self) -> Option<&QuitPrompt> {
        self.quit_prompt.as_ref()
    }

    pub fn quit_prompt_active(&self) -> bool {
        self.quit_prompt.is_some()
    }

    /// The exit code once the operator has quit.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    pub fn screen_depth(&self) -> usize {
        self.stack.len()
    }

    /// The topmost pushed screen, if any.
    pub fn top_screen(&self) -> Option<&Screen> {
        self.stack.last()
    }

    pub fn top_screen_mut(&mut self) -> Option<&mut Screen> {
        self.stack.last_mut()
    }

    /// The session screen on top of the stack, if that is what is showing.
    pub fn session(&self) -> Option<&SessionScreen> {
        match self.stack.last() {
            Some(Screen::Session(screen)) => Some(screen),
            _ => None,
        }
    }

    /// The history screen on top of the stack, if that is what is showing.
    pub fn history(&self) -> Option<&HistoryScreen> {
        match self.stack.last() {
            Some(Screen::History(screen)) => Some(screen),
            _ => None,
        }
    }

    /// The run id under the cursor, or `None` when the table is empty.
    pub fn selected_run_id(&self) -> Option<&str> {
        self.visible.get(self.cursor).map(|r| r.run_id.as_str())
    }

    fn memo_for_run(&self, run_id: &str) -> Option<&RowMemo> {
        self.memo.values().find(|m| m.snapshot.run_id == run_id)
    }

    // ----- Screen stack ---------------------------------------------------------

    /// Push a screen; a session screen takes its first tick immediately.
    pub fn push_screen(&mut self, screen: Screen) {
        let mut screen = screen;
        if let Screen::Session(session) = &mut screen {
            session.refresh_now();
            self.last_session_poll = Some(Instant::now());
        }
        self.stack.push(screen);
    }

    /// Pop the top screen; the dashboard's row cursor is intrinsically
    /// preserved because its state is never rebuilt on a push.
    pub fn pop_screen(&mut self) -> Option<Screen> {
        self.stack.pop()
    }

    fn exit(&mut self) {
        self.exit_code = Some(0);
    }

    // ----- Polling + render state ------------------------------------------

    /// Pull one snapshot and rebuild the rows. A poll that fails is
    /// contained: the last good frame stays, the status bar carries the
    /// error, and the next tick retries.
    pub fn refresh_now(&mut self) {
        match (self.poll)() {
            Ok(snapshot) => {
                self.last_snapshot = Some(snapshot);
                self.last_error = None;
            }
            Err(err) => self.last_error = Some(format!("store read failed: {err}")),
        }
        self.last_poll = Some(Instant::now());
        self.absorb_snapshot();
        self.refresh_daemon_statuses();
        self.rebuild_visible();
    }

    /// Fold the last snapshot into the memos: freshly active rows refresh
    /// their memo, absent rows are marked departing, and rows past the
    /// linger window drop.
    fn absorb_snapshot(&mut self) {
        let now = (self.clock)();
        let rows: Vec<RowSnapshot> = self
            .last_snapshot
            .as_ref()
            .map(|s| s.rows.clone())
            .unwrap_or_default();
        let mut active_keys = BTreeSet::new();
        for row in rows {
            let key = (row.task_id.clone(), row.run_id.clone());
            let cells = row_to_cells(&row);
            let haystack = cells.join(" ").to_lowercase();
            active_keys.insert(key.clone());
            self.memo.insert(
                key,
                RowMemo {
                    snapshot: row,
                    cells,
                    haystack,
                    last_active_at: now,
                    active: true,
                },
            );
        }
        let linger = self.linger_seconds;
        self.memo.retain(|key, memo| {
            if active_keys.contains(key) {
                return true;
            }
            memo.active = false;
            (now - memo.last_active_at).num_seconds() < linger
        });
    }

    /// Query both daemons so a dying child surfaces within one tick.
    fn refresh_daemon_statuses(&mut self) {
        let Some(daemons) = self.daemons.as_ref() else {
            return;
        };
        match daemons.status(DAEMON_WORKER) {
            Ok(status) => self.last_worker_status = Some(status),
            Err(err) => self.last_error = Some(format!("worker status read failed: {err}")),
        }
        match daemons.status(DAEMON_AUTOPILOT) {
            Ok(status) => self.last_autopilot_status = Some(status),
            Err(err) => self.last_error = Some(format!("autopilot status read failed: {err}")),
        }
    }

    /// Recompute the visible rows from the memos and the filter, restoring
    /// the selection by run id (rows may have shifted), falling back to
    /// the old index clamped into the new bounds.
    fn rebuild_visible(&mut self) {
        let selected = self.selected_run_id().map(str::to_owned);
        let previous_row = self.cursor;
        self.visible = self
            .memo
            .values()
            .filter(|memo| self.needle.is_empty() || memo.haystack.contains(&self.needle))
            .map(|memo| VisibleRow {
                run_id: memo.snapshot.run_id.clone(),
                cells: memo.cells.clone(),
                active: memo.active,
            })
            .collect();
        if self.visible.is_empty() {
            self.cursor = 0;
            return;
        }
        self.cursor = match selected.and_then(|id| self.visible.iter().position(|r| r.run_id == id))
        {
            Some(index) => index,
            None => previous_row.min(self.visible.len() - 1),
        };
    }

    /// Apply a plain-text filter (case-insensitive substring over the
    /// rendered cells) and rebuild the rows.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_owned();
        self.needle = filter.to_lowercase();
        self.rebuild_visible();
    }

    /// The empty-state line: "no runs at all" versus "filter excludes
    /// everything", or `None` when rows are showing.
    pub fn empty_state_text(&self) -> Option<String> {
        if self.memo.is_empty() {
            Some("(no in-flight runs)".to_owned())
        } else if self.visible.is_empty() {
            Some(format!("(no rows match filter '{}')", self.filter))
        } else {
            None
        }
    }

    /// The worker / autopilot status bar, or `None` when no control is
    /// wired (the bar is hidden entirely).
    pub fn daemon_bar(&self) -> Option<Line<'static>> {
        let now = (Utc::now().timestamp_millis() as f64) / 1000.0;
        let mut spans: Vec<Span<'static>> = Vec::new();
        if let Some(status) = &self.last_worker_status {
            spans.push(format_worker_status(status));
        }
        if let Some(status) = &self.last_autopilot_status {
            if !spans.is_empty() {
                spans.push(Span::raw("   "));
            }
            spans.push(format_autopilot_status(status, now));
        }
        if spans.is_empty() {
            None
        } else {
            Some(Line::from(spans))
        }
    }

    // ----- Timers ------------------------------------------------------------

    /// Fire whichever refreshes are due at `now`: the dashboard poll every
    /// `poll_interval`, the session screen's tick every
    /// `session_poll_interval` while a session is on top.
    pub fn on_timer(&mut self, now: Instant) {
        let poll_due = self
            .last_poll
            .is_none_or(|last| now.duration_since(last) >= self.poll_interval);
        if poll_due {
            self.refresh_now();
            self.last_poll = Some(now);
        }
        if let Some(Screen::Session(session)) = self.stack.last_mut() {
            let due = self
                .last_session_poll
                .is_none_or(|last| now.duration_since(last) >= self.session_poll_interval);
            if due {
                session.refresh_now();
                self.last_session_poll = Some(now);
            }
        }
    }

    /// How long the event loop may block before the next refresh is due.
    pub fn due_in(&self, now: Instant) -> Duration {
        let poll_wait = self.last_poll.map_or(Duration::ZERO, |last| {
            self.poll_interval.saturating_sub(now.duration_since(last))
        });
        if matches!(self.stack.last(), Some(Screen::Session(_))) {
            let session_wait = self.last_session_poll.map_or(Duration::ZERO, |last| {
                self.session_poll_interval
                    .saturating_sub(now.duration_since(last))
            });
            poll_wait.min(session_wait)
        } else {
            poll_wait
        }
    }

    // ----- Keys ---------------------------------------------------------------

    /// Route one key press: the quit prompt wins while active, then the
    /// top pushed screen, then the dashboard itself.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if let Some(prompt) = &self.quit_prompt {
            if let Some(choice) = prompt.handle_key(key) {
                self.handle_quit_choice(choice);
            }
            return;
        }
        match self.stack.last_mut() {
            Some(Screen::Session(session)) => {
                let action = session.handle_key(key);
                self.handle_session_action(action);
            }
            Some(Screen::History(history)) => {
                let action = history.handle_key(key);
                self.handle_history_action(action);
            }
            None => self.handle_dashboard_key(key),
        }
    }

    fn handle_session_action(&mut self, action: SessionAction) {
        match action {
            SessionAction::None => {}
            SessionAction::Back => {
                self.pop_screen();
            }
            SessionAction::RequestQuit => self.request_quit(),
            SessionAction::ExitAll => self.handle_exit_slash(),
            SessionAction::WorkerSlash(argument) => {
                let notice = self.handle_worker_slash(&argument);
                if let Some(Screen::Session(session)) = self.stack.last_mut() {
                    session.set_notice(notice);
                }
            }
            SessionAction::AutopilotSlash(argument) => {
                let notice = self.handle_autopilot_slash(&argument);
                if let Some(Screen::Session(session)) = self.stack.last_mut() {
                    session.set_notice(notice);
                }
            }
        }
    }

    fn handle_history_action(&mut self, action: HistoryAction) {
        match action {
            HistoryAction::None => {}
            HistoryAction::Back => {
                self.pop_screen();
            }
            HistoryAction::RequestQuit => self.request_quit(),
            HistoryAction::OpenSession { run_id, task_id } => {
                self.open_session_for(&run_id, &task_id);
            }
        }
    }

    fn handle_dashboard_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.request_quit();
            return;
        }
        if (ctrl && key.code == KeyCode::Char('i')) || key.code == KeyCode::Tab {
            self.toggle_input_focus();
            return;
        }
        match self.focus {
            Focus::Input => match self.input.handle_key(key) {
                InputEvent::Submitted(raw) => self.on_input_submitted(&raw),
                InputEvent::Changed => self.on_input_changed(),
                InputEvent::Blur => self.set_focus(Focus::Table),
                InputEvent::Ignored => {}
            },
            Focus::Table => match key.code {
                KeyCode::Up => self.cursor_up(),
                KeyCode::Down => self.cursor_down(),
                KeyCode::Enter => self.action_open_session(),
                KeyCode::Char('q') => self.request_quit(),
                KeyCode::Char('?') => self.toggle_help(),
                KeyCode::Char('h') => self.action_open_history(),
                _ => {}
            },
        }
    }

    pub fn cursor_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn cursor_down(&mut self) {
        if self.cursor + 1 < self.visible.len() {
            self.cursor += 1;
        }
    }

    pub fn toggle_help(&mut self) {
        self.help_visible = !self.help_visible;
    }

    pub fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
        self.input.set_focused(focus == Focus::Input);
    }

    /// `ctrl+i` / `tab`: move focus to the input bar, or back to the table
    /// when it already has it.
    pub fn toggle_input_focus(&mut self) {
        match self.focus {
            Focus::Table => self.set_focus(Focus::Input),
            Focus::Input => self.set_focus(Focus::Table),
        }
    }

    /// Live-update the row filter as the operator types. Slash keystrokes
    /// do NOT filter: the previous filter stays until Enter or a non-slash
    /// edit.
    fn on_input_changed(&mut self) {
        let value = self.input.value().to_owned();
        if value.starts_with('/') {
            return;
        }
        self.set_filter(&value);
    }

    /// Input submit: slash command, or set / clear the filter (leaving
    /// the input populated so the operator can edit it).
    fn on_input_submitted(&mut self, raw: &str) {
        if is_slash(raw) {
            self.handle_slash(raw);
            return;
        }
        self.set_filter(raw);
    }

    /// Route one slash command from the input bar. Commands needing a
    /// target run read the cursor's selection; failures surface an inline
    /// notice and leave the input populated for editing.
    pub fn handle_slash(&mut self, raw: &str) {
        let Some(command) = parse_slash(raw) else {
            return;
        };
        let verb = command.verb.as_str();
        if verb.is_empty() {
            self.set_notice(unknown_command_notice(""));
            return;
        }
        match verb {
            SLASH_HELP => {
                self.set_notice(help_text());
                self.input.clear();
            }
            SLASH_STATUS => {
                let text = self.status_text();
                self.set_notice(text);
                self.input.clear();
            }
            SLASH_APPROVE => self.dispatch_run_command(
                CONTROL_COMMAND_APPROVE,
                JsonObject::new(),
                &APPROVABLE_STATUSES,
            ),
            SLASH_REJECT => {
                let mut payload = JsonObject::new();
                if !command.argument.is_empty() {
                    payload.insert(
                        "feedback".to_owned(),
                        Value::String(command.argument.clone()),
                    );
                }
                self.dispatch_run_command(CONTROL_COMMAND_REJECT, payload, &APPROVABLE_STATUSES);
            }
            SLASH_INTERRUPT => self.dispatch_run_command(
                CONTROL_COMMAND_INTERRUPT,
                JsonObject::new(),
                &STEERABLE_STATUSES,
            ),
            SLASH_ARCHIVE => {
                self.handle_archive();
                self.input.clear();
            }
            SLASH_HISTORY => {
                if self.open_history.is_none() {
                    self.set_notice("/history is not wired on this screen");
                    return;
                }
                self.input.clear();
                self.action_open_history();
            }
            SLASH_WORKER => {
                let notice = self.handle_worker_slash(&command.argument);
                self.set_notice(notice);
                self.input.clear();
            }
            SLASH_AUTOPILOT => {
                let notice = self.handle_autopilot_slash(&command.argument);
                self.set_notice(notice);
                self.input.clear();
            }
            SLASH_QUIT => {
                self.input.clear();
                self.request_quit();
            }
            SLASH_EXIT => {
                self.input.clear();
                self.handle_exit_slash();
            }
            _ => self.set_notice(unknown_command_notice(verb)),
        }
    }

    /// Enqueue a control command against the cursor-selected run. Inline
    /// notices cover the failure shapes: no enqueue seam, nothing
    /// selected, a run that left the active set, a status that does not
    /// permit the command, and a store failure.
    fn dispatch_run_command(&mut self, kind: &str, payload: JsonObject, permitted: &[Status]) {
        if self.enqueue.is_none() {
            self.set_notice(format!("/{kind} is not wired on this screen"));
            return;
        }
        let Some(run_id) = self.selected_run_id().map(str::to_owned) else {
            self.set_notice(format!("/{kind}: no run selected"));
            return;
        };
        let Some(memo) = self.memo_for_run(&run_id) else {
            self.set_notice(format!("/{kind}: run {run_id} no longer in active set"));
            return;
        };
        let status_value = memo.snapshot.status.clone();
        let steerable = status_value
            .parse::<Status>()
            .is_ok_and(|status| permitted.contains(&status));
        if !steerable {
            self.set_notice(format!(
                "run is not steerable for '{kind}' (status={status_value})"
            ));
            return;
        }
        let outcome = self
            .enqueue
            .as_mut()
            .map(|enqueue| enqueue(&run_id, kind, payload));
        match outcome {
            Some(Ok(_)) => {
                self.set_notice(format!("/{kind} enqueued for {run_id}"));
                self.input.clear();
            }
            Some(Err(err)) => self.set_notice(format!("/{kind} failed: {err}")),
            None => self.set_notice(format!("/{kind} is not wired on this screen")),
        }
    }

    fn handle_archive(&mut self) {
        let Some(archive) = self.archive.as_mut() else {
            self.set_notice("/archive is not wired on this screen");
            return;
        };
        let notice = match archive() {
            Err(err) => format!("/archive failed: {err}"),
            Ok(moved) if moved.is_empty() => "/archive: no phases archived".to_owned(),
            Ok(moved) => format!("/archive: moved {}", moved.join(", ")),
        };
        self.set_notice(notice);
    }

    /// Push the session screen for the selected row; a no-op without a
    /// session factory, on an empty table, or while the prompt is up.
    pub fn action_open_session(&mut self) {
        if self.quit_prompt.is_some() || self.open_session.is_none() {
            return;
        }
        let Some(run_id) = self.selected_run_id().map(str::to_owned) else {
            return;
        };
        let Some(task_id) = self
            .memo_for_run(&run_id)
            .map(|m| m.snapshot.task_id.clone())
        else {
            return;
        };
        self.open_session_for(&run_id, &task_id);
    }

    fn open_session_for(&mut self, run_id: &str, task_id: &str) {
        let Some(factory) = self.open_session.as_mut() else {
            return;
        };
        if let Some(screen) = factory(run_id, task_id) {
            self.push_screen(Screen::Session(Box::new(screen)));
        }
    }

    /// Push the finished-run history screen (`h` / `/history`).
    pub fn action_open_history(&mut self) {
        if self.quit_prompt.is_some() {
            return;
        }
        let Some(factory) = self.open_history.as_mut() else {
            return;
        };
        if let Some(screen) = factory() {
            self.push_screen(Screen::History(Box::new(screen)));
        }
    }

    /// The selected run's status for `/status`, or the summary aggregate
    /// with no selection.
    pub fn status_text(&self) -> String {
        let memo = self.selected_run_id().and_then(|id| self.memo_for_run(id));
        let Some(memo) = memo else {
            let Some(snapshot) = &self.last_snapshot else {
                return "/status: no snapshot yet".to_owned();
            };
            let s = &snapshot.summary;
            return format!(
                "/status: active={} tokens={} cost=${:.4}",
                s.active_workers, s.tokens_total, s.cost_usd_total
            );
        };
        let row = &memo.snapshot;
        let mut line = format!(
            "run {}: task={} status={}",
            row.run_id, row.task_id, row.status
        );
        if let Some(instruction) = row.awaiting_instruction.as_ref().filter(|i| !i.is_empty()) {
            line.push_str(&format!("; gate={instruction}"));
        }
        line
    }

    pub fn set_notice(&mut self, message: impl Into<String>) {
        self.notice = Some(message.into());
    }

    // ----- Quit handoff ------------------------------------------------------

    fn daemon_is_supervised(&self, name: &str) -> bool {
        self.daemons
            .as_ref()
            .and_then(|d| d.status(name).ok())
            .is_some_and(|s| s.state == DaemonState::Supervised)
    }

    /// Shared entry for `q`, `ctrl+c`, and `/quit`: route through the
    /// supervised-daemon prompt when this console owns a live child, exit
    /// silently otherwise (never block quit on a status hiccup, never kill
    /// what we do not own).
    pub fn request_quit(&mut self) {
        if self.quit_prompt.is_some() {
            return;
        }
        let worker_live = self.daemon_is_supervised(DAEMON_WORKER);
        let autopilot_live = self.daemon_is_supervised(DAEMON_AUTOPILOT);
        if !worker_live && !autopilot_live {
            self.exit();
            return;
        }
        self.quit_prompt = Some(QuitPrompt::new(running_daemons_label(
            worker_live,
            autopilot_live,
        )));
    }

    /// Apply the operator's quit-prompt choice across BOTH daemons.
    pub fn handle_quit_choice(&mut self, choice: QuitChoice) {
        self.quit_prompt = None;
        match choice {
            QuitChoice::Detach => {
                self.detach_daemons();
                self.exit();
            }
            QuitChoice::Stop => {
                self.stop_daemons();
                self.exit();
            }
            QuitChoice::Cancel => {}
        }
    }

    /// Detach every supervised child (they keep running); idempotent.
    pub fn detach_daemons(&mut self) {
        if let Some(daemons) = self.daemons.as_mut() {
            daemons.detach(DAEMON_WORKER);
            daemons.detach(DAEMON_AUTOPILOT);
        }
    }

    /// Stop every supervised child; a failure on one never blocks the other.
    fn stop_daemons(&mut self) {
        if let Some(daemons) = self.daemons.as_mut() {
            let _ = daemons.stop(DAEMON_WORKER);
            let _ = daemons.stop(DAEMON_AUTOPILOT);
        }
    }

    /// `/exit`: stop every supervised daemon and exit with no prompt.
    pub fn handle_exit_slash(&mut self) {
        self.stop_daemons();
        self.exit();
    }

    /// Dispatch `/worker start|stop`; returns the inline notice to show.
    pub fn handle_worker_slash(&mut self, argument: &str) -> String {
        self.handle_daemon_slash(DAEMON_WORKER, argument)
    }

    /// Dispatch `/autopilot start|stop`; returns the inline notice to show.
    pub fn handle_autopilot_slash(&mut self, argument: &str) -> String {
        self.handle_daemon_slash(DAEMON_AUTOPILOT, argument)
    }

    fn remember_status(&mut self, name: &str, status: DaemonStatus) {
        if name == DAEMON_WORKER {
            self.last_worker_status = Some(status);
        } else {
            self.last_autopilot_status = Some(status);
        }
    }

    fn handle_daemon_slash(&mut self, name: &str, argument: &str) -> String {
        let sub = argument.trim().to_lowercase();
        match sub.as_str() {
            "start" => {
                let Some(daemons) = self.daemons.as_mut() else {
                    return format!("/{name} start is not wired on this screen");
                };
                let status = match daemons.start(name) {
                    Ok(status) => status,
                    Err(err) => return format!("/{name} start failed: {err}"),
                };
                let notice = match status.state {
                    DaemonState::Supervised => format!(
                        "/{name} start: supervised (pid={})",
                        status
                            .pid
                            .map_or_else(|| "None".to_owned(), |p| p.to_string())
                    ),
                    DaemonState::Detached if name == DAEMON_WORKER => {
                        format!(
                            "/{name} start: a worker already holds a live lease (detached); no spawn"
                        )
                    }
                    DaemonState::Error => format!(
                        "/{name} start failed: {}",
                        status.message.as_deref().unwrap_or("unknown")
                    ),
                    other => format!("/{name} start: state={}", other.as_str()),
                };
                self.remember_status(name, status);
                notice
            }
            "stop" => {
                let Some(daemons) = self.daemons.as_mut() else {
                    return format!("/{name} stop is not wired on this screen");
                };
                let current = daemons.status(name).ok();
                if current
                    .as_ref()
                    .is_none_or(|s| s.state != DaemonState::Supervised)
                {
                    let state_name = current.map_or("unknown", |s| s.state.as_str());
                    return if name == DAEMON_WORKER {
                        format!("/worker stop: no supervised child to stop (state={state_name})")
                    } else {
                        format!(
                            "/autopilot stop: no supervised autopilot to stop (state={state_name})"
                        )
                    };
                }
                let stopped = match daemons.stop(name) {
                    Ok(stopped) => stopped,
                    Err(err) => return format!("/{name} stop failed: {err}"),
                };
                if let Ok(status) = daemons.status(name) {
                    self.remember_status(name, status);
                }
                if stopped {
                    format!("/{name} stop: {name} terminated gracefully")
                } else {
                    format!("/{name} stop: {name} did not exit within the wait window")
                }
            }
            _ => format!("/{name}: unknown sub-verb '{sub}'; try 'start' or 'stop'"),
        }
    }

    // ----- Render ------------------------------------------------------------

    /// Draw the whole console: the top pushed screen (or the dashboard),
    /// with the quit prompt overlaid while it is active.
    pub fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        match self.stack.last_mut() {
            Some(Screen::Session(session)) => session.render(frame, area),
            Some(Screen::History(history)) => history.render(frame, area),
            None => self.render_dashboard(frame, area),
        }
        if let Some(prompt) = &self.quit_prompt {
            prompt.render(frame, area);
        }
    }

    fn render_dashboard(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let summary = self.last_snapshot.as_ref().map_or_else(
            || format_summary(&SummaryData::empty()),
            |s| format_summary(&s.summary),
        );
        let daemon_bar = self.daemon_bar();
        let empty_state = self.empty_state_text();
        let notice_lines: Vec<Line<'static>> = self
            .notice
            .as_deref()
            .map(|n| n.lines().map(|l| Line::from(l.to_owned())).collect())
            .unwrap_or_default();
        let notice_rows = u16::try_from(notice_lines.len())
            .unwrap_or(MAX_NOTICE_ROWS)
            .min(MAX_NOTICE_ROWS);
        let help_rows = if self.help_visible {
            u16::try_from(HELP_LINES.len()).unwrap_or(u16::MAX)
        } else {
            0
        };
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(u16::from(daemon_bar.is_some())),
            Constraint::Min(2),
            Constraint::Length(u16::from(empty_state.is_some())),
            Constraint::Length(u16::from(self.last_error.is_some())),
            Constraint::Length(notice_rows),
            Constraint::Length(help_rows),
            Constraint::Length(1),
        ])
        .split(area);

        frame.render_widget(Paragraph::new(summary), chunks[0]);
        if let Some(bar) = daemon_bar {
            frame.render_widget(Paragraph::new(bar), chunks[1]);
        }
        self.render_table(frame, chunks[2]);
        if let Some(text) = empty_state {
            frame.render_widget(
                Paragraph::new(text).style(Style::default().add_modifier(Modifier::DIM)),
                chunks[3],
            );
        }
        if let Some(error) = &self.last_error {
            frame.render_widget(
                Paragraph::new(error.clone()).style(Style::default().fg(Color::Yellow)),
                chunks[4],
            );
        }
        if !notice_lines.is_empty() {
            frame.render_widget(
                Paragraph::new(Text::from(notice_lines)).style(Style::default().fg(Color::Yellow)),
                chunks[5],
            );
        }
        if self.help_visible {
            let lines: Vec<Line<'static>> = HELP_LINES.iter().map(|l| Line::from(*l)).collect();
            frame.render_widget(Paragraph::new(Text::from(lines)), chunks[6]);
        }
        self.input.render(frame, chunks[7]);
    }

    fn render_table(&self, frame: &mut Frame<'_>, area: Rect) {
        let header = Row::new(
            TABLE_COLUMNS
                .iter()
                .map(|(_, label)| Cell::from(*label))
                .collect::<Vec<_>>(),
        )
        .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
        let rows: Vec<Row> = self
            .visible
            .iter()
            .map(|row| {
                let mut cells: Vec<Cell> =
                    row.cells.iter().map(|c| Cell::from(c.as_str())).collect();
                if row.active {
                    if let Some(style) = status_style(&row.cells[1]) {
                        cells[1] = Cell::from(row.cells[1].as_str()).style(style);
                    }
                    Row::new(cells)
                } else {
                    Row::new(cells).style(Style::default().add_modifier(Modifier::DIM))
                }
            })
            .collect();
        let labels: Vec<&str> = TABLE_COLUMNS.iter().map(|(_, label)| *label).collect();
        let all_cells: Vec<[String; 7]> = self.visible.iter().map(|r| r.cells.clone()).collect();
        let widths = table_widths(&labels, &all_cells);
        let table = Table::new(rows, widths)
            .header(header)
            .column_spacing(2)
            .highlight_symbol("> ")
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = TableState::default();
        if !self.visible.is_empty() {
            state.select(Some(self.cursor));
        }
        frame.render_stateful_widget(table, area, &mut state);
    }
}

// --- Formatting helpers -----------------------------------------------------------

/// Column constraints sized to the widest header / cell per column; the
/// last column takes whatever remains.
pub fn table_widths(labels: &[&str], cells: &[[String; 7]]) -> Vec<Constraint> {
    labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let widest = cells
                .iter()
                .map(|row| row[i].chars().count())
                .max()
                .unwrap_or(0)
                .max(label.chars().count());
            let width = u16::try_from(widest).unwrap_or(u16::MAX);
            if i + 1 == labels.len() {
                Constraint::Min(width)
            } else {
                Constraint::Length(width)
            }
        })
        .collect()
}

fn status_style(status: &str) -> Option<Style> {
    if status == Status::Running.as_str() {
        Some(
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else if status == Status::Validating.as_str() {
        Some(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if status == Status::AwaitingApproval.as_str() {
        Some(
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        None
    }
}

/// Render one `worker:` span for the status bar, following the spec's
/// vocabulary verbatim (`supervised` / `detached` / `none` / `dead` /
/// `error: <reason>`); healthy states render calm, dead / error loud.
pub fn format_worker_status(status: &DaemonStatus) -> Span<'static> {
    let detail = status
        .message
        .as_ref()
        .map_or_else(String::new, |m| format!(" ({m})"));
    match status.state {
        DaemonState::Supervised => {
            let pid = status.pid.map_or_else(String::new, |p| format!(" pid={p}"));
            Span::styled(
                format!("worker: supervised{pid}"),
                Style::default().fg(Color::Green),
            )
        }
        DaemonState::Detached => Span::styled(
            "worker: detached (this console did not spawn it)",
            Style::default().fg(Color::Yellow),
        ),
        DaemonState::None => Span::styled(
            "worker: none -- type '/worker start' to spawn one",
            Style::default().add_modifier(Modifier::DIM),
        ),
        DaemonState::Dead => Span::styled(
            format!("worker: dead -- type '/worker start' to respawn{detail}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        DaemonState::DeadAfterBudget => Span::styled(
            format!(
                "worker: dead (crash-loop budget exhausted) -- type '/worker start' to re-arm{detail}"
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        DaemonState::Error => Span::styled(
            format!(
                "worker: error: {}",
                status.message.as_deref().unwrap_or("unknown")
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    }
}

/// Summarize one activity snapshot: `<phase> | last: E emitted, D dropped`
/// with a countdown (`next in Xm Ys`) derived from `next_cycle_at`. The
/// last-cycle summary appears only once a cycle has completed.
pub fn format_autopilot_activity(activity: &AutopilotActivity, now: f64) -> String {
    let mut parts: Vec<String> = Vec::new();
    match activity.phase.as_str() {
        PHASE_RUNNING => parts.push(format!("cycle {} running", activity.cycle_index)),
        PHASE_IDLE => match activity.next_cycle_at {
            Some(next) => parts.push(format!(
                "idle, next in {}",
                format_duration((next - now).round() as i64)
            )),
            None => parts.push("idle".to_owned()),
        },
        PHASE_STARTING => parts.push("starting".to_owned()),
        other => parts.push(other.to_owned()),
    }
    let completed = activity.phase == PHASE_IDLE
        || (activity.phase == PHASE_RUNNING && activity.cycle_index > 1);
    if completed {
        parts.push(format!(
            "last: {} emitted, {} dropped",
            activity.last_emitted, activity.last_dropped
        ));
    }
    parts.join(" | ")
}

/// Render one `autopilot:` span, mirroring [`format_worker_status`]'s
/// vocabulary. A live daemon (owned or adopted) with recorded activity has
/// its per-cycle summary appended.
pub fn format_autopilot_status(status: &DaemonStatus, now: f64) -> Span<'static> {
    let pid = status.pid.map_or_else(String::new, |p| format!(" pid={p}"));
    let activity = status.activity.as_ref().map_or_else(String::new, |a| {
        format!(" -- {}", format_autopilot_activity(a, now))
    });
    let detail = status
        .message
        .as_ref()
        .map_or_else(String::new, |m| format!(" ({m})"));
    match status.state {
        DaemonState::Supervised => Span::styled(
            format!("autopilot: supervised{pid}{activity}"),
            Style::default().fg(Color::Green),
        ),
        DaemonState::Detached => Span::styled(
            format!("autopilot: detached{pid} (this console did not spawn it){activity}"),
            Style::default().fg(Color::Yellow),
        ),
        DaemonState::None => Span::styled(
            "autopilot: none -- type '/autopilot start' to spawn one",
            Style::default().add_modifier(Modifier::DIM),
        ),
        DaemonState::Dead => Span::styled(
            format!("autopilot: dead -- type '/autopilot start' to respawn{detail}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        DaemonState::DeadAfterBudget => Span::styled(
            format!(
                "autopilot: dead (crash-loop budget exhausted) -- type '/autopilot start' to re-arm{detail}"
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        DaemonState::Error => Span::styled(
            format!(
                "autopilot: error: {}",
                status.message.as_deref().unwrap_or("unknown")
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    }
}

/// The summary header: one line so it stays readable on narrow terminals;
/// counts roll up to match `flywheel status`. Zero counts render dim so
/// the populated ones carry the eye; `failed` goes red once non-zero.
pub fn format_summary(summary: &SummaryData) -> Line<'static> {
    let queued = summary.count("fresh");
    let done = summary.count("done");
    let failed = summary.count("retryable");
    let dim = Style::default().add_modifier(Modifier::DIM);
    let pick = |non_zero: bool, style: Style| if non_zero { style } else { dim };
    Line::from(vec![
        Span::styled(
            format!("active={}", summary.active_workers),
            pick(
                summary.active_workers > 0,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        Span::raw("  "),
        Span::styled(
            format!("queued={queued}"),
            pick(queued > 0, Style::default().fg(Color::Cyan)),
        ),
        Span::raw("  "),
        Span::styled(
            format!("done={done}"),
            pick(done > 0, Style::default().fg(Color::Green)),
        ),
        Span::raw("  "),
        Span::styled(
            format!("failed={failed}"),
            pick(
                failed > 0,
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ),
        Span::raw("  "),
        Span::raw(format!("tokens={}", format_tokens(summary.tokens_total))),
        Span::raw("  "),
        Span::raw(format!("cost={}", format_cost(summary.cost_usd_total))),
        Span::raw("  "),
        Span::raw(format!(
            "runtime={}",
            format_duration(summary.runtime_seconds)
        )),
    ])
}

/// `5s` / `2m05s` / `1h02m03s`; negatives clamp to `0s`.
pub fn format_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m{secs:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{secs:02}s")
    } else {
        format!("{secs}s")
    }
}

/// Humanize token counts: `980` / `56k` / `36.0M`.
pub fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 10_000 {
        format!("{:.0}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// Dollars: cents once past $1, tenths of a cent below it.
pub fn format_cost(cost_usd: f64) -> String {
    if cost_usd >= 1.0 {
        format!("${cost_usd:.2}")
    } else {
        format!("${cost_usd:.4}")
    }
}

/// Cap `text` at `width` characters with an ASCII `...` tail.
pub fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let keep = width.saturating_sub(3);
    let mut out: String = text.chars().take(keep).collect();
    out.push_str("...");
    out
}

/// The seven plain cell strings for one row (the filter haystack and the
/// table share this one source).
pub fn row_to_cells(row: &RowSnapshot) -> [String; 7] {
    let attempt = row
        .attempt
        .map_or_else(|| "?".to_owned(), |a| a.to_string());
    let iteration = row
        .iteration
        .map_or_else(|| "?".to_owned(), |i| i.to_string());
    let age = row
        .age_seconds
        .map_or_else(|| "-".to_owned(), format_duration);
    let detail = truncate(&row.last_detail, MAX_DETAIL_WIDTH);
    let action = format!("{} {}", row.last_kind, detail).trim().to_owned();
    [
        row.task_id.clone(),
        row.status.clone(),
        format!("{attempt}/{iteration}"),
        age,
        format_tokens(row.tokens),
        format_cost(row.cost_usd),
        action,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_screen::SessionStatus;
    use crate::tui::test_support::{buffer_text, ctrl, key, terminal};
    use chrono::TimeZone;
    use flywheel_orchestrator::history::{HistoryRow, HistoryRun};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn row(task_id: &str) -> RowSnapshot {
        RowSnapshot {
            run_id: format!("run-{task_id}"),
            task_id: task_id.to_owned(),
            status: "running".to_owned(),
            attempt: Some(1),
            iteration: Some(2),
            age_seconds: Some(3),
            idle_seconds: Some(1),
            tokens: 42,
            cost_usd: 0.123,
            turns: 1,
            iterations_completed: 1,
            last_kind: "ASSISTANT".to_owned(),
            last_detail: "Edit(file_path=README.md)".to_owned(),
            awaiting_instruction: None,
        }
    }

    fn snapshot(rows: &[RowSnapshot]) -> DashboardSnapshot {
        DashboardSnapshot {
            summary: SummaryData {
                active_workers: rows.len(),
                task_counts: Vec::new(),
                tokens_total: 0,
                cost_usd_total: 0.0,
                runtime_seconds: 0,
            },
            rows: rows.to_vec(),
        }
    }

    fn app_with(rows: Vec<RowSnapshot>) -> DashboardApp {
        let snap = snapshot(&rows);
        let mut app = DashboardApp::new(move || Ok(snap.clone()));
        app.refresh_now();
        app
    }

    fn type_line(app: &mut DashboardApp, line: &str) {
        app.input_mut_for_test().set_value(line);
        app.handle_key(key(KeyCode::Enter));
    }

    impl DashboardApp {
        fn input_mut_for_test(&mut self) -> &mut InputBar {
            &mut self.input
        }
    }

    /// Faithful double of the supervisors: stop / detach act (and record)
    /// only when a live child is owned.
    struct ScriptedDaemons {
        states: BTreeMap<&'static str, DaemonStatus>,
        start_to: BTreeMap<&'static str, DaemonStatus>,
        calls: Rc<RefCell<Vec<String>>>,
    }

    impl ScriptedDaemons {
        fn new(worker: DaemonStatus, autopilot: DaemonStatus) -> (Self, Rc<RefCell<Vec<String>>>) {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let daemons = Self {
                states: BTreeMap::from([(DAEMON_WORKER, worker), (DAEMON_AUTOPILOT, autopilot)]),
                start_to: BTreeMap::new(),
                calls: Rc::clone(&calls),
            };
            (daemons, calls)
        }
    }

    impl DaemonControl for ScriptedDaemons {
        fn status(&self, name: &str) -> Result<DaemonStatus, crate::tui::DaemonError> {
            self.states
                .get(name)
                .cloned()
                .ok_or_else(|| crate::tui::DaemonError::Unknown(name.to_owned()))
        }

        fn start(&mut self, name: &str) -> Result<DaemonStatus, crate::tui::DaemonError> {
            self.calls.borrow_mut().push(format!("{name}:start"));
            let status = self
                .start_to
                .get(name)
                .cloned()
                .unwrap_or_else(|| DaemonStatus::new(DaemonState::Supervised).with_pid(4321));
            self.states.insert(
                if name == DAEMON_WORKER {
                    DAEMON_WORKER
                } else {
                    DAEMON_AUTOPILOT
                },
                status.clone(),
            );
            Ok(status)
        }

        fn stop(&mut self, name: &str) -> Result<bool, crate::tui::DaemonError> {
            let live = self
                .states
                .get(name)
                .is_some_and(|s| s.state == DaemonState::Supervised);
            if !live {
                return Ok(false);
            }
            self.calls.borrow_mut().push(format!("{name}:stop"));
            self.states.insert(
                if name == DAEMON_WORKER {
                    DAEMON_WORKER
                } else {
                    DAEMON_AUTOPILOT
                },
                DaemonStatus::new(DaemonState::None),
            );
            Ok(true)
        }

        fn detach(&mut self, name: &str) {
            if self
                .states
                .get(name)
                .is_some_and(|s| s.state == DaemonState::Supervised)
            {
                self.calls.borrow_mut().push(format!("{name}:detach"));
            }
        }

        fn death_reason(&self, _name: &str) -> Option<String> {
            None
        }
    }

    fn live(pid: u32) -> DaemonStatus {
        DaemonStatus::new(DaemonState::Supervised).with_pid(pid)
    }

    fn none() -> DaemonStatus {
        DaemonStatus::new(DaemonState::None)
    }

    #[test]
    fn keyboard_navigation_help_and_quit() {
        let mut app = app_with(vec![row("alpha"), row("beta"), row("gamma")]);
        assert_eq!(app.cursor(), 0);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.cursor(), 1);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.cursor(), 2);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.cursor(), 2);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.cursor(), 1);
        assert!(!app.help_visible());
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.help_visible());
        let mut term = terminal(100, 30);
        term.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("key bindings"));
        assert!(text.contains("q / ctrl+c  quit"));
        app.handle_key(key(KeyCode::Char('?')));
        assert!(!app.help_visible());
        assert_eq!(app.exit_code(), None);
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.exit_code(), Some(0));
    }

    #[test]
    fn ctrl_c_quits_from_any_focus() {
        let mut app = app_with(vec![row("alpha")]);
        app.handle_key(ctrl('i'));
        assert_eq!(app.focus(), Focus::Input);
        app.handle_key(ctrl('c'));
        assert_eq!(app.exit_code(), Some(0));
    }

    #[test]
    fn empty_state_renders_header_and_keeps_polling() {
        let mut app = app_with(Vec::new());
        assert_eq!(
            app.empty_state_text().as_deref(),
            Some("(no in-flight runs)")
        );
        let mut term = terminal(80, 10);
        term.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("active=0"));
        assert!(text.contains("(no in-flight runs)"));
        assert!(text.contains("> filter rows, or /help for slash commands"));
        app.refresh_now();
        assert_eq!(
            app.empty_state_text().as_deref(),
            Some("(no in-flight runs)")
        );
    }

    #[test]
    fn store_read_error_preserves_last_good_frame() {
        let mode = Rc::new(RefCell::new("good"));
        let probe = Rc::clone(&mode);
        let mut app = DashboardApp::new(move || {
            if *probe.borrow() == "good" {
                Ok(snapshot(&[row("alpha")]))
            } else {
                Err("database is locked".to_owned())
            }
        });
        app.refresh_now();
        assert_eq!(app.visible_run_order(), vec!["run-alpha"]);
        assert_eq!(app.last_error(), None);
        *mode.borrow_mut() = "error";
        app.refresh_now();
        assert_eq!(app.visible_run_order(), vec!["run-alpha"]);
        assert_eq!(
            app.last_error(),
            Some("store read failed: database is locked")
        );
        let mut term = terminal(80, 10);
        term.draw(|f| app.render(f)).unwrap();
        assert!(buffer_text(&term).contains("database is locked"));
        *mode.borrow_mut() = "good";
        app.refresh_now();
        assert_eq!(app.last_error(), None);
    }

    #[test]
    fn lingers_dropped_row_then_drops_it() {
        let state = Rc::new(RefCell::new(snapshot(&[row("alpha"), row("beta")])));
        let clock_now = Rc::new(RefCell::new(
            Utc.with_ymd_and_hms(2026, 6, 10, 12, 0, 0).unwrap(),
        ));
        let poll_state = Rc::clone(&state);
        let clock_probe = Rc::clone(&clock_now);
        let mut app = DashboardApp::new(move || Ok(poll_state.borrow().clone()))
            .with_linger_seconds(30)
            .with_clock(move || *clock_probe.borrow());
        app.refresh_now();
        assert_eq!(app.visible_run_order(), vec!["run-alpha", "run-beta"]);
        *state.borrow_mut() = snapshot(&[row("alpha")]);
        *clock_now.borrow_mut() += chrono::Duration::seconds(1);
        app.refresh_now();
        assert_eq!(app.visible_run_order(), vec!["run-alpha", "run-beta"]);
        assert!(!app.visible[1].active);
        *clock_now.borrow_mut() += chrono::Duration::seconds(28);
        app.refresh_now();
        assert!(app.visible_run_order().contains(&"run-beta".to_owned()));
        *clock_now.borrow_mut() += chrono::Duration::seconds(2);
        app.refresh_now();
        assert_eq!(app.visible_run_order(), vec!["run-alpha"]);
    }

    #[test]
    fn summary_header_shows_counts_and_totals() {
        let snap = DashboardSnapshot {
            summary: SummaryData {
                active_workers: 2,
                task_counts: vec![
                    ("fresh".into(), 3),
                    ("done".into(), 5),
                    ("retryable".into(), 1),
                ],
                tokens_total: 12345,
                cost_usd_total: 1.2345,
                runtime_seconds: 125,
            },
            rows: vec![row("alpha"), row("beta")],
        };
        let mut app = DashboardApp::new(move || Ok(snap.clone()));
        app.refresh_now();
        let mut term = terminal(100, 10);
        term.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains(
            "active=2  queued=3  done=5  failed=1  tokens=12k  cost=$1.23  runtime=2m05s"
        ));
        assert!(
            text.contains("  task   status   attempt/iter  age  tokens  cost     last action"),
            "{text}"
        );
        assert!(text.contains("> alpha  running  1/2           3s   42      $0.1230  ASSISTANT Edit(file_path=README.md)"), "{text}");
    }

    #[test]
    fn cursor_survives_refresh_and_follows_selected_run() {
        let mut app = app_with(vec![row("alpha"), row("beta"), row("gamma")]);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.cursor(), 1);
        app.refresh_now();
        assert_eq!(app.cursor(), 1);
        let state = Rc::new(RefCell::new(snapshot(&[
            row("alpha"),
            row("beta"),
            row("gamma"),
        ])));
        let probe = Rc::clone(&state);
        let mut app = DashboardApp::new(move || Ok(probe.borrow().clone())).with_linger_seconds(0);
        app.refresh_now();
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.selected_run_id(), Some("run-beta"));
        *state.borrow_mut() = snapshot(&[row("beta"), row("gamma")]);
        app.refresh_now();
        assert_eq!(app.selected_run_id(), Some("run-beta"));
        // The selected run itself departs: the index clamps.
        *state.borrow_mut() = snapshot(&[row("gamma")]);
        app.refresh_now();
        assert_eq!(app.selected_run_id(), Some("run-gamma"));
        assert_eq!(app.cursor(), 0);
    }

    #[test]
    fn help_slash_shows_help_text_and_clears_input() {
        let mut app = app_with(vec![row("alpha")]);
        app.handle_key(ctrl('i'));
        type_line(&mut app, "/help");
        let notice = app.notice().unwrap();
        assert!(notice.contains("/approve"));
        assert!(notice.contains("/interrupt"));
        assert_eq!(app.input().value(), "");
        let mut term = terminal(100, 30);
        term.draw(|f| app.render(f)).unwrap();
        assert!(buffer_text(&term).contains("available slash commands"));
    }

    #[test]
    fn plain_text_filters_rows_live() {
        let mut app = app_with(vec![row("alpha"), row("beta"), row("gamma")]);
        assert_eq!(
            app.visible_run_order(),
            vec!["run-alpha", "run-beta", "run-gamma"]
        );
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus(), Focus::Input);
        for c in "ALPHA".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.visible_run_order(), vec!["run-alpha"]);
        assert_eq!(app.filter_text(), "ALPHA");
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Backspace));
        }
        assert_eq!(
            app.visible_run_order(),
            vec!["run-alpha", "run-beta", "run-gamma"]
        );
        assert_eq!(app.filter_text(), "");
        // Enter confirms a filter and leaves the input populated.
        type_line(&mut app, "beta");
        assert_eq!(app.visible_run_order(), vec!["run-beta"]);
        assert_eq!(app.input().value(), "beta");
        // Typing a slash command does not disturb the active filter.
        app.input_mut_for_test().clear();
        for c in "/status".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.filter_text(), "beta");
        assert_eq!(app.visible_run_order(), vec!["run-beta"]);
    }

    #[test]
    fn zero_match_filter_shows_empty_state() {
        let mut app = app_with(vec![row("alpha"), row("beta")]);
        app.set_filter("no-such-task");
        assert!(app.visible_run_order().is_empty());
        assert_eq!(
            app.empty_state_text().as_deref(),
            Some("(no rows match filter 'no-such-task')")
        );
        let mut term = terminal(80, 10);
        term.draw(|f| app.render(f)).unwrap();
        assert!(buffer_text(&term).contains("no-such-task"));
        app.set_filter("");
        assert_eq!(app.empty_state_text(), None);
        assert_eq!(app.visible_run_order(), vec!["run-alpha", "run-beta"]);
    }

    #[test]
    fn unknown_slash_command_inline_error_preserves_input() {
        let mut app = app_with(vec![row("alpha")]);
        app.handle_key(ctrl('i'));
        type_line(&mut app, "/nope");
        assert_eq!(app.notice(), Some("unknown slash command /nope; try /help"));
        assert_eq!(app.input().value(), "/nope");
        type_line(&mut app, "/ x");
        assert_eq!(app.notice(), Some("unknown slash command /; try /help"));
    }

    #[test]
    fn unfocused_input_does_not_steal_arrows_or_q() {
        let mut app = app_with(vec![row("alpha"), row("beta")]);
        assert_eq!(app.focus(), Focus::Table);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.cursor(), 1);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.cursor(), 0);
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.help_visible());
        // Focused, letters are text and escape returns focus to the table.
        app.handle_key(ctrl('i'));
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.exit_code(), None);
        assert_eq!(app.input().value(), "q");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.focus(), Focus::Table);
        app.handle_key(ctrl('i'));
        app.handle_key(ctrl('i'));
        assert_eq!(app.focus(), Focus::Table);
    }

    #[test]
    fn enter_opens_session_and_escape_restores_selection() {
        let opened = Rc::new(RefCell::new(Vec::new()));
        let probe = Rc::clone(&opened);
        let snap = snapshot(&[row("alpha"), row("beta")]);
        let mut app = DashboardApp::new(move || Ok(snap.clone())).with_open_session(
            move |run_id, task_id| {
                probe.borrow_mut().push(run_id.to_owned());
                Some(SessionScreen::new(
                    run_id,
                    task_id,
                    || Ok(Vec::new()),
                    || Ok(SessionStatus::new(Some(Status::Running), None)),
                ))
            },
        );
        app.refresh_now();
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(*opened.borrow(), vec!["run-beta"]);
        assert_eq!(app.session().map(|s| s.run_id()), Some("run-beta"));
        let mut term = terminal(80, 12);
        term.draw(|f| app.render(f)).unwrap();
        assert!(buffer_text(&term).contains("run=run-beta  task=beta"));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.session().is_none());
        assert_eq!(app.cursor(), 1);
        // Without a factory Enter is a silent no-op.
        let mut bare = app_with(vec![row("alpha")]);
        bare.handle_key(key(KeyCode::Enter));
        assert_eq!(bare.screen_depth(), 0);
    }

    fn hrow(task_id: &str) -> HistoryRow {
        let t0 = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        HistoryRow {
            task_id: task_id.to_owned(),
            phase: Some("30-a".to_owned()),
            latest: HistoryRun {
                run_id: format!("run-{task_id}"),
                task_id: task_id.to_owned(),
                status: Status::Done,
                source: String::new(),
                started_at: Some(t0),
                finished_at: Some(t0),
                retries: 0,
                error: String::new(),
                attempts: 1,
                tokens_total: 10,
                cost_usd_total: 0.1,
                turns_total: 1,
            },
            prior_runs: Vec::new(),
        }
    }

    #[test]
    fn history_key_and_slash_push_screen_and_drill_into_session() {
        let mut app = DashboardApp::new(|| Ok(snapshot(&[])))
            .with_open_history(|| {
                Some(HistoryScreen::new(|| Ok(vec![hrow("alpha"), hrow("beta")])))
            })
            .with_open_session(|run_id, task_id| {
                Some(SessionScreen::new(
                    run_id,
                    task_id,
                    || Ok(Vec::new()),
                    || Ok(SessionStatus::new(Some(Status::Done), None)),
                ))
            });
        app.refresh_now();
        app.handle_key(key(KeyCode::Char('h')));
        assert!(app.history().is_some());
        assert_eq!(
            app.history().unwrap().visible_run_order,
            vec!["run-alpha", "run-beta"]
        );
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.session().map(|s| s.run_id()), Some("run-beta"));
        assert_eq!(app.screen_depth(), 2);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.history().is_some());
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen_depth(), 0);
        app.handle_key(ctrl('i'));
        type_line(&mut app, "/history");
        assert!(app.history().is_some());
        assert_eq!(app.input().value(), "");
        app.handle_key(ctrl('c'));
        assert_eq!(app.exit_code(), Some(0));
    }

    #[test]
    fn history_not_wired_degrades_to_notice() {
        let mut app = app_with(Vec::new());
        app.handle_key(key(KeyCode::Char('h')));
        assert!(app.history().is_none());
        app.handle_key(ctrl('i'));
        type_line(&mut app, "/history");
        assert_eq!(app.notice(), Some("/history is not wired on this screen"));
    }

    #[test]
    fn run_commands_target_the_selected_row() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let probe = Rc::clone(&calls);
        let mut gated = row("gated");
        gated.status = "awaiting_approval".to_owned();
        gated.awaiting_instruction = Some("Confirm it.".to_owned());
        let snap = snapshot(&[row("alpha"), gated]);
        let mut app = DashboardApp::new(move || Ok(snap.clone())).with_enqueue(
            move |run_id, kind, payload| {
                probe
                    .borrow_mut()
                    .push((run_id.to_owned(), kind.to_owned(), payload));
                Ok(7)
            },
        );
        app.refresh_now();
        app.handle_key(ctrl('i'));
        type_line(&mut app, "/interrupt");
        assert_eq!(app.notice(), Some("/interrupt enqueued for run-alpha"));
        assert_eq!(app.input().value(), "");
        type_line(&mut app, "/approve");
        assert_eq!(
            app.notice(),
            Some("run is not steerable for 'approve' (status=running)")
        );
        assert_eq!(app.input().value(), "/approve");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(ctrl('i'));
        type_line(&mut app, "/reject needs more tests");
        assert_eq!(app.notice(), Some("/reject enqueued for run-gated"));
        type_line(&mut app, "/approve");
        type_line(&mut app, "/interrupt");
        assert_eq!(
            app.notice(),
            Some("run is not steerable for 'interrupt' (status=awaiting_approval)")
        );
        type_line(&mut app, "/status");
        assert_eq!(
            app.notice(),
            Some("run run-gated: task=gated status=awaiting_approval; gate=Confirm it.")
        );
        let recorded = calls.borrow();
        assert_eq!(recorded.len(), 3);
        assert_eq!(recorded[0].0, "run-alpha");
        assert_eq!(recorded[0].1, "interrupt");
        assert!(recorded[0].2.is_empty());
        assert_eq!(recorded[1].1, "reject");
        assert_eq!(recorded[1].2["feedback"], "needs more tests");
        assert_eq!(recorded[2].1, "approve");
    }

    #[test]
    fn run_command_failure_shapes() {
        let mut app = app_with(vec![row("alpha")]);
        app.handle_slash("/interrupt");
        assert_eq!(app.notice(), Some("/interrupt is not wired on this screen"));
        let mut app = DashboardApp::new(|| Ok(snapshot(&[]))).with_enqueue(|_, _, _| Ok(1));
        app.refresh_now();
        app.handle_slash("/interrupt");
        assert_eq!(app.notice(), Some("/interrupt: no run selected"));
        app.handle_slash("/status");
        assert_eq!(
            app.notice(),
            Some("/status: active=0 tokens=0 cost=$0.0000")
        );
        let snap = snapshot(&[row("alpha")]);
        let mut app = DashboardApp::new(move || Ok(snap.clone()))
            .with_enqueue(|_, _, _| Err("disk full".to_owned()));
        app.refresh_now();
        app.handle_slash("/interrupt");
        assert_eq!(app.notice(), Some("/interrupt failed: disk full"));
        let mut fresh = DashboardApp::new(|| Err("boom".to_owned()));
        assert_eq!(fresh.status_text(), "/status: no snapshot yet");
        fresh.refresh_now();
        assert_eq!(fresh.status_text(), "/status: no snapshot yet");
    }

    #[test]
    fn archive_slash_reports_outcome() {
        let mut app = app_with(Vec::new());
        app.handle_slash("/archive");
        assert_eq!(app.notice(), Some("/archive is not wired on this screen"));
        let mut app = DashboardApp::new(|| Ok(snapshot(&[])))
            .with_archive(|| Ok(vec!["archive/10-a".to_owned()]));
        app.handle_slash("/archive");
        assert_eq!(app.notice(), Some("/archive: moved archive/10-a"));
        let mut app = DashboardApp::new(|| Ok(snapshot(&[]))).with_archive(|| Ok(Vec::new()));
        app.handle_slash("/archive");
        assert_eq!(app.notice(), Some("/archive: no phases archived"));
        let mut app =
            DashboardApp::new(|| Ok(snapshot(&[]))).with_archive(|| Err("gate failed".to_owned()));
        app.handle_slash("/archive");
        assert_eq!(app.notice(), Some("/archive failed: gate failed"));
    }

    #[test]
    fn worker_bar_renders_every_state() {
        let cases = [
            (live(4242), "worker: supervised pid=4242"),
            (
                DaemonStatus::new(DaemonState::Detached),
                "worker: detached (this console did not spawn it)",
            ),
            (none(), "worker: none -- type '/worker start' to spawn one"),
            (
                DaemonStatus::new(DaemonState::Dead)
                    .with_pid(999)
                    .with_message("exit=1"),
                "worker: dead -- type '/worker start' to respawn (exit=1)",
            ),
            (
                DaemonStatus::new(DaemonState::DeadAfterBudget).with_message("exit=1"),
                "worker: dead (crash-loop budget exhausted) -- type '/worker start' to re-arm (exit=1)",
            ),
            (
                DaemonStatus::new(DaemonState::Error).with_message("no such file"),
                "worker: error: no such file",
            ),
            (
                DaemonStatus::new(DaemonState::Error),
                "worker: error: unknown",
            ),
        ];
        for (status, expected) in cases {
            assert_eq!(format_worker_status(&status).content, expected);
        }
        let (daemons, _) = ScriptedDaemons::new(live(4242), none());
        let mut app = DashboardApp::new(|| Ok(snapshot(&[]))).with_daemons(Box::new(daemons));
        app.refresh_now();
        let mut term = terminal(120, 8);
        term.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains(
            "worker: supervised pid=4242   autopilot: none -- type '/autopilot start' to spawn one"
        ));
        assert_eq!(app.worker_status().unwrap().pid, Some(4242));
        let mut bare = app_with(Vec::new());
        assert!(bare.daemon_bar().is_none());
        assert!(bare.worker_status().is_none());
        let mut term = terminal(120, 8);
        term.draw(|f| bare.render(f)).unwrap();
        assert!(!buffer_text(&term).contains("worker:"));
    }

    #[test]
    fn autopilot_status_and_activity_lines() {
        let mut activity = AutopilotActivity::new(7, PHASE_IDLE, 3, 1000.0, 300.0);
        activity.next_cycle_at = Some(1125.0);
        activity.last_emitted = 2;
        activity.last_dropped = 1;
        assert_eq!(
            format_autopilot_activity(&activity, 1000.0),
            "idle, next in 2m05s | last: 2 emitted, 1 dropped"
        );
        activity.next_cycle_at = None;
        assert_eq!(
            format_autopilot_activity(&activity, 1000.0),
            "idle | last: 2 emitted, 1 dropped"
        );
        activity.phase = PHASE_RUNNING.to_owned();
        assert_eq!(
            format_autopilot_activity(&activity, 1000.0),
            "cycle 3 running | last: 2 emitted, 1 dropped"
        );
        activity.cycle_index = 1;
        assert_eq!(
            format_autopilot_activity(&activity, 1000.0),
            "cycle 1 running"
        );
        activity.phase = PHASE_STARTING.to_owned();
        assert_eq!(format_autopilot_activity(&activity, 1000.0), "starting");
        activity.phase = "weird".to_owned();
        assert_eq!(format_autopilot_activity(&activity, 1000.0), "weird");
        let supervised =
            live(7).with_activity(AutopilotActivity::new(7, PHASE_STARTING, 0, 0.0, 300.0));
        assert_eq!(
            format_autopilot_status(&supervised, 0.0).content,
            "autopilot: supervised pid=7 -- starting"
        );
        let detached = DaemonStatus::new(DaemonState::Detached).with_pid(9);
        assert_eq!(
            format_autopilot_status(&detached, 0.0).content,
            "autopilot: detached pid=9 (this console did not spawn it)"
        );
        assert_eq!(
            format_autopilot_status(&none(), 0.0).content,
            "autopilot: none -- type '/autopilot start' to spawn one"
        );
        assert_eq!(
            format_autopilot_status(
                &DaemonStatus::new(DaemonState::Dead).with_message("exit=2"),
                0.0
            )
            .content,
            "autopilot: dead -- type '/autopilot start' to respawn (exit=2)"
        );
        assert_eq!(
            format_autopilot_status(&DaemonStatus::new(DaemonState::DeadAfterBudget), 0.0).content,
            "autopilot: dead (crash-loop budget exhausted) -- type '/autopilot start' to re-arm"
        );
        assert_eq!(
            format_autopilot_status(
                &DaemonStatus::new(DaemonState::Error).with_message("bad env"),
                0.0
            )
            .content,
            "autopilot: error: bad env"
        );
    }

    #[test]
    fn slash_worker_start_and_stop_drive_the_control() {
        let (daemons, calls) = ScriptedDaemons::new(none(), none());
        let mut app = DashboardApp::new(|| Ok(snapshot(&[]))).with_daemons(Box::new(daemons));
        app.refresh_now();
        app.handle_key(ctrl('i'));
        type_line(&mut app, "/worker start");
        assert_eq!(*calls.borrow(), vec!["worker:start"]);
        assert_eq!(app.notice(), Some("/worker start: supervised (pid=4321)"));
        assert_eq!(app.input().value(), "");
        assert_eq!(app.worker_status().unwrap().state, DaemonState::Supervised);
        type_line(&mut app, "/worker stop");
        assert_eq!(*calls.borrow(), vec!["worker:start", "worker:stop"]);
        assert_eq!(
            app.notice(),
            Some("/worker stop: worker terminated gracefully")
        );
        assert_eq!(app.worker_status().unwrap().state, DaemonState::None);
        type_line(&mut app, "/worker tickle");
        assert_eq!(
            app.notice(),
            Some("/worker: unknown sub-verb 'tickle'; try 'start' or 'stop'")
        );
        type_line(&mut app, "/autopilot start");
        assert_eq!(
            app.notice(),
            Some("/autopilot start: supervised (pid=4321)")
        );
        type_line(&mut app, "/autopilot stop");
        assert_eq!(
            app.notice(),
            Some("/autopilot stop: autopilot terminated gracefully")
        );
        type_line(&mut app, "/autopilot stop");
        assert_eq!(
            app.notice(),
            Some("/autopilot stop: no supervised autopilot to stop (state=none)")
        );
        type_line(&mut app, "/autopilot bogus");
        assert_eq!(
            app.notice(),
            Some("/autopilot: unknown sub-verb 'bogus'; try 'start' or 'stop'")
        );
    }

    #[test]
    fn slash_worker_start_with_live_lease_and_stop_on_detached() {
        let (mut daemons, calls) =
            ScriptedDaemons::new(DaemonStatus::new(DaemonState::Detached), none());
        daemons
            .start_to
            .insert(DAEMON_WORKER, DaemonStatus::new(DaemonState::Detached));
        let mut app = DashboardApp::new(|| Ok(snapshot(&[]))).with_daemons(Box::new(daemons));
        app.refresh_now();
        assert_eq!(
            app.handle_worker_slash("start"),
            "/worker start: a worker already holds a live lease (detached); no spawn"
        );
        assert_eq!(
            app.handle_worker_slash("stop"),
            "/worker stop: no supervised child to stop (state=detached)"
        );
        assert_eq!(*calls.borrow(), vec!["worker:start"]);
        let (mut daemons, _) = ScriptedDaemons::new(none(), none());
        daemons.start_to.insert(
            DAEMON_WORKER,
            DaemonStatus::new(DaemonState::Error).with_message("spawn failed: no python"),
        );
        let mut app = DashboardApp::new(|| Ok(snapshot(&[]))).with_daemons(Box::new(daemons));
        assert_eq!(
            app.handle_worker_slash("start"),
            "/worker start failed: spawn failed: no python"
        );
        let mut unwired = app_with(Vec::new());
        assert_eq!(
            unwired.handle_worker_slash("start"),
            "/worker start is not wired on this screen"
        );
        assert_eq!(
            unwired.handle_worker_slash("stop"),
            "/worker stop is not wired on this screen"
        );
        assert_eq!(
            unwired.handle_autopilot_slash("start"),
            "/autopilot start is not wired on this screen"
        );
    }

    #[test]
    fn quit_prompts_only_when_a_daemon_is_supervised() {
        let (daemons, calls) = ScriptedDaemons::new(none(), none());
        let mut app = DashboardApp::new(|| Ok(snapshot(&[]))).with_daemons(Box::new(daemons));
        app.handle_key(key(KeyCode::Char('q')));
        assert!(!app.quit_prompt_active());
        assert_eq!(app.exit_code(), Some(0));
        assert!(calls.borrow().is_empty());

        let (daemons, calls) = ScriptedDaemons::new(none(), live(2));
        let mut app = DashboardApp::new(|| Ok(snapshot(&[]))).with_daemons(Box::new(daemons));
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.quit_prompt_active());
        assert_eq!(
            app.quit_prompt().unwrap().running_label(),
            "supervised autopilot is running"
        );
        // A second q does not stack a second prompt; Enter on the dashboard
        // is swallowed by the modal, not routed to the table.
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.quit_prompt_active());
        let mut term = terminal(80, 12);
        term.draw(|f| app.render(f)).unwrap();
        assert!(buffer_text(&term).contains("supervised autopilot is running"));
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.exit_code(), Some(0));
        assert_eq!(*calls.borrow(), vec!["autopilot:stop"]);
    }

    #[test]
    fn quit_stop_and_detach_cover_both_daemons_and_escape_cancels() {
        let (daemons, calls) = ScriptedDaemons::new(live(1), live(2));
        let mut app = DashboardApp::new(|| Ok(snapshot(&[]))).with_daemons(Box::new(daemons));
        app.request_quit();
        assert_eq!(
            app.quit_prompt().unwrap().running_label(),
            "supervised worker and autopilot are running"
        );
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.quit_prompt_active());
        assert_eq!(app.exit_code(), None);
        assert!(calls.borrow().is_empty());
        app.handle_key(ctrl('c'));
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(*calls.borrow(), vec!["worker:stop", "autopilot:stop"]);
        assert_eq!(app.exit_code(), Some(0));

        let (daemons, calls) = ScriptedDaemons::new(live(1), live(2));
        let mut app = DashboardApp::new(|| Ok(snapshot(&[]))).with_daemons(Box::new(daemons));
        app.handle_key(ctrl('i'));
        type_line(&mut app, "/quit");
        assert!(app.quit_prompt_active());
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(*calls.borrow(), vec!["worker:detach", "autopilot:detach"]);
        assert_eq!(app.exit_code(), Some(0));
    }

    #[test]
    fn slash_exit_stops_all_daemons_without_prompt() {
        let (daemons, calls) = ScriptedDaemons::new(live(1), live(2));
        let mut app = DashboardApp::new(|| Ok(snapshot(&[]))).with_daemons(Box::new(daemons));
        app.handle_key(ctrl('i'));
        type_line(&mut app, "/exit");
        assert!(!app.quit_prompt_active());
        assert_eq!(*calls.borrow(), vec!["worker:stop", "autopilot:stop"]);
        assert_eq!(app.exit_code(), Some(0));
    }

    #[test]
    fn session_slash_verbs_route_through_the_app() {
        let (daemons, calls) = ScriptedDaemons::new(none(), live(2));
        let snap = snapshot(&[row("alpha")]);
        let mut app = DashboardApp::new(move || Ok(snap.clone()))
            .with_daemons(Box::new(daemons))
            .with_open_session(|run_id, task_id| {
                Some(SessionScreen::new(
                    run_id,
                    task_id,
                    || Ok(Vec::new()),
                    || Ok(SessionStatus::new(Some(Status::Running), None)),
                ))
            });
        app.refresh_now();
        app.handle_key(key(KeyCode::Enter));
        assert!(app.session().is_some());
        for c in "/worker start".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.session().unwrap().notice(),
            Some("/worker start: supervised (pid=4321)")
        );
        assert_eq!(*calls.borrow(), vec!["worker:start"]);
        for c in "/autopilot stop".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.session().unwrap().notice(),
            Some("/autopilot stop: autopilot terminated gracefully")
        );
        for c in "/quit".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(app.quit_prompt_active());
        let mut term = terminal(80, 12);
        term.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("run=run-alpha"));
        assert!(text.contains("supervised worker is running"));
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.quit_prompt_active());
        for c in "/exit".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.exit_code(), Some(0));
        assert_eq!(
            *calls.borrow(),
            vec!["worker:start", "autopilot:stop", "worker:stop"]
        );
    }

    #[test]
    fn timers_fire_dashboard_and_session_refreshes() {
        let polls = Rc::new(RefCell::new(0));
        let probe = Rc::clone(&polls);
        let fetches = Rc::new(RefCell::new(0));
        let fetch_probe = Rc::clone(&fetches);
        let mut app = DashboardApp::new(move || {
            *probe.borrow_mut() += 1;
            Ok(snapshot(&[row("alpha")]))
        })
        .with_poll_interval(Duration::from_millis(100))
        .with_session_poll_interval(Duration::from_millis(10))
        .with_open_session(move |run_id, task_id| {
            let fp = Rc::clone(&fetch_probe);
            Some(SessionScreen::new(
                run_id,
                task_id,
                move || {
                    *fp.borrow_mut() += 1;
                    Ok(Vec::new())
                },
                || Ok(SessionStatus::new(Some(Status::Running), None)),
            ))
        });
        let start = Instant::now();
        app.on_timer(start);
        assert_eq!(*polls.borrow(), 1);
        app.on_timer(start + Duration::from_millis(50));
        assert_eq!(*polls.borrow(), 1);
        assert!(app.due_in(start + Duration::from_millis(50)) <= Duration::from_millis(50));
        app.on_timer(start + Duration::from_millis(100));
        assert_eq!(*polls.borrow(), 2);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(*fetches.borrow(), 1);
        app.on_timer(start + Duration::from_millis(100) + Duration::from_millis(200));
        assert_eq!(*fetches.borrow(), 2);
        assert_eq!(*polls.borrow(), 3);
        assert!(app.due_in(Instant::now()) <= Duration::from_millis(100));
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(format_duration(5), "5s");
        assert_eq!(format_duration(125), "2m05s");
        assert_eq!(format_duration(3723), "1h02m03s");
        assert_eq!(format_duration(-4), "0s");
        assert_eq!(format_tokens(980), "980");
        assert_eq!(format_tokens(56_000), "56k");
        assert_eq!(format_tokens(36_000_000), "36.0M");
        assert_eq!(format_cost(1.234), "$1.23");
        assert_eq!(format_cost(0.00123), "$0.0012");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(
            truncate(&"x".repeat(70), MAX_DETAIL_WIDTH).chars().count(),
            MAX_DETAIL_WIDTH
        );
        let mut r = row("alpha");
        r.attempt = None;
        r.age_seconds = None;
        r.last_detail = String::new();
        let cells = row_to_cells(&r);
        assert_eq!(
            cells,
            ["alpha", "running", "?/2", "-", "42", "$0.1230", "ASSISTANT"]
        );
        let widths = table_widths(
            &["a", "bb"],
            &[[
                "x".into(),
                "y".into(),
                "".into(),
                "".into(),
                "".into(),
                "".into(),
                "".into(),
            ]],
        );
        assert_eq!(widths, vec![Constraint::Length(1), Constraint::Min(2)]);
        assert_eq!(dashboard_help_text().lines().count(), HELP_LINES.len());
    }
}
