//! The live transcript of one run (the console's session view).
//!
//! Pressing `Enter` on a dashboard row opens this screen. It pulls the
//! selected run's merged audit stream through a
//! [`crate::session::TranscriptTailer`]-backed fetch, renders each entry
//! chat-style, and tail-follows until either the operator scrolls up
//! (follow pauses, a new-activity indicator appears) or the run reaches a
//! terminal status (a banner pins the terminal state and the transcript
//! stays scrollable). `Escape` returns to the dashboard with the row
//! selection preserved.
//!
//! The compose box doubles as the persistent input bar: plain text is a
//! `say`; a leading `/` switches the line into a slash command from the
//! shared [`crate::slash`] vocabulary. Slash commands target the viewed
//! run with the exact store-mediated semantics of their CLI twins; a
//! submit-time status re-check enforces the lifecycle rule (say /
//! interrupt only in RUNNING / VALIDATING, approve / reject only in
//! AWAITING_APPROVAL).
//!
//! The screen takes `fetch` and `status` closures rather than a store
//! handle so tests drive deterministic frames without timer races.

use crate::dashboard::MOUSE_SCROLL_LINES;
use crate::session::{EntryKind, TranscriptEntry, is_terminal, short};
use crate::slash::{
    SLASH_APPROVE, SLASH_ARCHIVE, SLASH_AUTOPILOT, SLASH_EXIT, SLASH_HELP, SLASH_INTERRUPT,
    SLASH_QUIT, SLASH_REJECT, SLASH_STATUS, SLASH_WORKER, help_text, is_slash, parse_slash,
    unknown_command_notice,
};
use crate::tui::{InputBar, InputEvent};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use flywheel_core::invoker_client::{
    CONTROL_COMMAND_APPROVE, CONTROL_COMMAND_INTERRUPT, CONTROL_COMMAND_REJECT, CONTROL_COMMAND_SAY,
};
use flywheel_core::lifecycle::Status;
use flywheel_core::store::JsonObject;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use serde_json::Value;
use std::collections::BTreeMap;

/// How often the screen pulls new transcript entries (matches the audit
/// CLI's default `--follow` cadence).
pub const DEFAULT_SESSION_POLL_INTERVAL_SECONDS: f64 = 0.25;

/// The compose box's placeholder, advertising the primary actions plus the
/// slash escape.
pub const COMPOSE_PLACEHOLDER: &str =
    "say message or /help (enter=say, ctrl+x=interrupt, ctrl+y=approve, ctrl+r=reject)";

/// Statuses against which the in-session watcher applies `say` /
/// `interrupt`; outside this set the SDK session is no longer driven.
pub const STEERABLE_STATUSES: [Status; 2] = [Status::Running, Status::Validating];

/// The status at which a parked manual gate accepts approve / reject.
pub const APPROVABLE_STATUSES: [Status; 1] = [Status::AwaitingApproval];

/// Most lines the pending-commands block occupies.
const MAX_PENDING_ROWS: u16 = 6;
/// Most lines the inline notice occupies (the `/help` listing fits).
const MAX_NOTICE_ROWS: u16 = 12;
/// Most lines the gate banner occupies.
const MAX_GATE_ROWS: u16 = 3;

/// Returns the entries appended since the previous call.
pub type FetchEntries = Box<dyn FnMut() -> Result<Vec<TranscriptEntry>, String>>;
/// Samples the viewed run's lifecycle.
pub type StatusProbe = Box<dyn FnMut() -> Result<SessionStatus, String>>;
/// Producer seam: `(kind, payload)` -> the store-assigned command id.
pub type EnqueueCommand = Box<dyn FnMut(&str, JsonObject) -> Result<i64, String>>;
/// `/archive` seam: the moved phase directories, for the inline summary.
pub type ArchiveAction = Box<dyn FnMut() -> Result<Vec<String>, String>>;

/// Snapshot of the viewed run's lifecycle, fed to the screen each tick.
///
/// `missing` is the "lifecycle row gone" sentinel; `awaiting_instruction`
/// is the parked manual gate's instruction when the run is at
/// AWAITING_APPROVAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStatus {
    pub status: Option<Status>,
    pub awaiting_instruction: Option<String>,
    pub missing: bool,
}

impl SessionStatus {
    pub fn new(status: Option<Status>, awaiting_instruction: Option<String>) -> Self {
        Self {
            status,
            awaiting_instruction,
            missing: false,
        }
    }

    pub fn missing() -> Self {
        Self {
            status: None,
            awaiting_instruction: None,
            missing: true,
        }
    }
}

/// Whether a pending command is still outstanding or has failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingState {
    Pending,
    Failed,
}

/// One outstanding operator-issued command awaiting watcher feedback.
///
/// An applied command is removed from the pending list outright (the
/// matching transcript entry already announces the apply); a failed one
/// stays visible so the operator sees the error detail inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCommand {
    pub command_id: i64,
    pub kind: String,
    pub summary: String,
    pub status: PendingState,
    pub error_detail: Option<String>,
}

/// What a key press on the session screen asks the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    None,
    /// `escape`: pop back to the previous screen.
    Back,
    /// `/quit` or `ctrl+c`: route through the app's quit prompt.
    RequestQuit,
    /// `/exit`: stop every supervised daemon and exit immediately.
    ExitAll,
    /// `/worker <arg>`: the app owns the supervisor; it answers with a
    /// notice the screen should show.
    WorkerSlash(String),
    /// `/autopilot <arg>`: same routing as the worker verb.
    AutopilotSlash(String),
}

// --- Block grouping and styles -----------------------------------------------------

/// The four block groups: agent prose, tool activity, operator turns, and
/// lifecycle telemetry (SYSTEM / RESULT fold into lifecycle, USER_TEXT
/// into operator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockGroup {
    Agent,
    Tool,
    Operator,
    Lifecycle,
}

pub fn block_group(entry: &TranscriptEntry) -> BlockGroup {
    match entry.kind {
        EntryKind::AgentText => BlockGroup::Agent,
        EntryKind::ToolCall | EntryKind::ToolResult => BlockGroup::Tool,
        EntryKind::OperatorSay | EntryKind::UserText => BlockGroup::Operator,
        _ => BlockGroup::Lifecycle,
    }
}

/// Whether `curr` should be preceded by a blank line: never for the first
/// entry, never between a tool call and its result, otherwise exactly when
/// the block group changes.
pub fn needs_blank_separator(prev: Option<&TranscriptEntry>, curr: &TranscriptEntry) -> bool {
    let Some(prev) = prev else {
        return false;
    };
    if prev.kind == EntryKind::ToolCall && curr.kind == EntryKind::ToolResult {
        return false;
    }
    block_group(prev) != block_group(curr)
}

/// Whether `entry` is an error-class line: an erroring tool result or a
/// failed control command.
pub fn is_error_entry(entry: &TranscriptEntry) -> bool {
    (entry.kind == EntryKind::ToolResult && entry.header.ends_with("(error)"))
        || (entry.kind == EntryKind::Lifecycle && entry.control_command_error.is_some())
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// The `(header_style, body_style)` pair for `entry`. Error-class entries
/// are red (never dim+dim) regardless of the static map.
pub fn resolve_styles(entry: &TranscriptEntry) -> (Style, Style) {
    if is_error_entry(entry) {
        return (bold(Color::Red), Style::default().fg(Color::Red));
    }
    match entry.kind {
        EntryKind::AgentText => (bold(Color::Cyan), Style::default()),
        EntryKind::ToolCall => (bold(Color::Magenta), Style::default()),
        EntryKind::ToolResult => (Style::default().fg(Color::Magenta), dim()),
        EntryKind::UserText => (bold(Color::Green), Style::default()),
        EntryKind::OperatorSay => (bold(Color::Yellow), Style::default().fg(Color::Yellow)),
        EntryKind::System | EntryKind::Result => (Style::default().fg(Color::Blue), dim()),
        EntryKind::Lifecycle => (dim(), dim()),
        EntryKind::Gate => (bold(Color::Red), Style::default().fg(Color::Red)),
    }
}

/// Render one entry as styled lines.
///
/// Every entry starts with a dim `HH:MM:SS` prefix followed by two spaces;
/// continuation lines inside a multi-line body do not repeat it.
/// `leading_blank` prepends one empty line (the block-group separator).
/// Single-line entries render as `HH:MM:SS  header  body`; multi-line
/// AGENT_TEXT puts the header on its own line with the prose verbatim
/// below; TOOL_RESULT nests two spaces under its call, with a multi-line
/// error body indented four spaces per line.
pub fn render_entry_text(entry: &TranscriptEntry, leading_blank: bool) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if leading_blank {
        lines.push(Line::default());
    }
    let (header_style, body_style) = resolve_styles(entry);
    let mut first = vec![
        Span::styled(entry.ts.format("%H:%M:%S").to_string(), dim()),
        Span::raw("  "),
    ];
    if entry.kind == EntryKind::ToolResult {
        first.push(Span::raw("  "));
        first.push(Span::styled(entry.header.clone(), header_style));
        if entry.body.is_empty() {
            lines.push(Line::from(first));
        } else if entry.body.contains('\n') {
            lines.push(Line::from(first));
            for line in entry.body.split('\n') {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(line.to_owned(), body_style),
                ]));
            }
        } else {
            first.push(Span::raw("  "));
            first.push(Span::styled(entry.body.clone(), body_style));
            lines.push(Line::from(first));
        }
        return Text::from(lines);
    }
    first.push(Span::styled(entry.header.clone(), header_style));
    if entry.body.is_empty() {
        lines.push(Line::from(first));
    } else if entry.kind == EntryKind::AgentText && entry.body.contains('\n') {
        lines.push(Line::from(first));
        for line in entry.body.split('\n') {
            lines.push(Line::from(Span::styled(line.to_owned(), body_style)));
        }
    } else {
        first.push(Span::raw("  "));
        first.push(Span::styled(entry.body.clone(), body_style));
        lines.push(Line::from(first));
    }
    Text::from(lines)
}

/// The plain (unstyled) form of [`render_entry_text`], lines joined by
/// newlines -- the layout tests' oracle.
pub fn render_entry_plain(entry: &TranscriptEntry, leading_blank: bool) -> String {
    text_to_plain(&render_entry_text(entry, leading_blank))
}

/// Flatten styled text to its plain string.
pub fn text_to_plain(text: &Text<'_>) -> String {
    text.lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Hard-wrap one styled line at `width` characters, preserving span styles
/// across the break. Width 0 returns the line unchanged.
pub fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line.clone()];
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in &line.spans {
        let mut chunk = String::new();
        for ch in span.content.chars() {
            if used == width {
                if !chunk.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut chunk), span.style));
                }
                out.push(Line::from(std::mem::take(&mut current)));
                used = 0;
            }
            chunk.push(ch);
            used += 1;
        }
        if !chunk.is_empty() {
            current.push(Span::styled(chunk, span.style));
        }
    }
    out.push(Line::from(current));
    out
}

fn wrap_plain(text: &str, width: usize) -> Vec<Line<'static>> {
    wrap_line(&Line::from(text.to_owned()), width)
}

// --- The screen ----------------------------------------------------------------

/// Drill-down view of one run's transcript.
///
/// Observable state (public for tests): `entries` (rendered so far),
/// `pending_commands` (keyed by command id, so the list renders in submit
/// order), plus the `follow` / `new_activity` / `notice` accessors.
pub struct SessionScreen {
    run_id: String,
    task_id: String,
    fetch: FetchEntries,
    status: StatusProbe,
    enqueue: Option<EnqueueCommand>,
    archive: Option<ArchiveAction>,
    follow: bool,
    new_activity: bool,
    last_status: Option<SessionStatus>,
    last_error: Option<String>,
    pub entries: Vec<TranscriptEntry>,
    pub pending_commands: BTreeMap<i64, PendingCommand>,
    notice: Option<String>,
    compose: InputBar,
    /// Every rendered transcript line, in order (append-only).
    lines: Vec<Line<'static>>,
    /// `lines` hard-wrapped at `wrapped_width`; `wrapped_upto` is how many
    /// raw lines have been wrapped so an incremental fetch wraps only its
    /// own additions.
    wrapped: Vec<Line<'static>>,
    wrapped_width: usize,
    wrapped_upto: usize,
    /// The top wrapped-line index while follow is paused.
    scroll: usize,
    /// The top index and viewport height observed at the last render.
    last_top: usize,
    viewport_height: usize,
}

impl SessionScreen {
    pub fn new(
        run_id: impl Into<String>,
        task_id: impl Into<String>,
        fetch: impl FnMut() -> Result<Vec<TranscriptEntry>, String> + 'static,
        status: impl FnMut() -> Result<SessionStatus, String> + 'static,
    ) -> Self {
        let mut compose = InputBar::new(COMPOSE_PLACEHOLDER);
        compose.set_focused(true);
        Self {
            run_id: run_id.into(),
            task_id: task_id.into(),
            fetch: Box::new(fetch),
            status: Box::new(status),
            enqueue: None,
            archive: None,
            follow: true,
            new_activity: false,
            last_status: None,
            last_error: None,
            entries: Vec::new(),
            pending_commands: BTreeMap::new(),
            notice: None,
            compose,
            lines: Vec::new(),
            wrapped: Vec::new(),
            wrapped_width: 0,
            wrapped_upto: 0,
            scroll: 0,
            last_top: 0,
            viewport_height: 0,
        }
    }

    /// Wire the control-command producer; without it steering is
    /// disabled (every verb is a silent no-op).
    pub fn with_enqueue(
        mut self,
        enqueue: impl FnMut(&str, JsonObject) -> Result<i64, String> + 'static,
    ) -> Self {
        self.enqueue = Some(Box::new(enqueue));
        self
    }

    /// Wire the `/archive` sweep.
    pub fn with_archive(
        mut self,
        archive: impl FnMut() -> Result<Vec<String>, String> + 'static,
    ) -> Self {
        self.archive = Some(Box::new(archive));
        self
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn follow(&self) -> bool {
        self.follow
    }

    pub fn new_activity(&self) -> bool {
        self.new_activity
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn last_status(&self) -> Option<&SessionStatus> {
        self.last_status.as_ref()
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn compose(&self) -> &InputBar {
        &self.compose
    }

    pub fn compose_mut(&mut self) -> &mut InputBar {
        &mut self.compose
    }

    /// Set the inline notice (the app uses this to answer `/worker` and
    /// `/autopilot`, which it dispatches on the screen's behalf).
    pub fn set_notice(&mut self, message: impl Into<String>) {
        self.notice = Some(message.into());
    }

    // ----- Polling + render state ------------------------------------------

    /// Pull one tailer tick and re-sample the status.
    ///
    /// A failure inside the fetch or the status probe is contained: the
    /// last good frame stays on screen and the banner carries the error.
    pub fn refresh_now(&mut self) {
        let new_entries = match (self.fetch)() {
            Ok(entries) => {
                self.last_error = None;
                entries
            }
            Err(err) => {
                self.last_error = Some(format!("transcript read failed: {err}"));
                Vec::new()
            }
        };
        match (self.status)() {
            Ok(status) => self.last_status = Some(status),
            Err(err) => self.last_error = Some(format!("status read failed: {err}")),
        }
        if !new_entries.is_empty() {
            self.append_entries(new_entries);
        }
    }

    /// Append rendered lines and update the follow state: following, the
    /// viewport pins to the bottom; paused, the lines still land but the
    /// new-activity indicator lights up.
    fn append_entries(&mut self, new_entries: Vec<TranscriptEntry>) {
        for entry in new_entries {
            let leading_blank = needs_blank_separator(self.entries.last(), &entry);
            self.lines
                .extend(render_entry_text(&entry, leading_blank).lines);
            self.reconcile_pending(&entry);
            self.entries.push(entry);
        }
        self.new_activity = !self.follow;
    }

    /// Flip a pending steering command to applied (dropped) / failed
    /// (kept, with detail) when its `control_command_id` lands in the
    /// transcript. Ids this screen never enqueued slide past untouched.
    fn reconcile_pending(&mut self, entry: &TranscriptEntry) {
        let Some(command_id) = entry.control_command_id else {
            return;
        };
        let Some(pending) = self.pending_commands.get_mut(&command_id) else {
            return;
        };
        if entry.kind == EntryKind::Lifecycle && entry.header == "control" {
            if let Some(error) = &entry.control_command_error {
                pending.status = PendingState::Failed;
                pending.error_detail = Some(error.clone());
            } else {
                self.pending_commands.remove(&command_id);
            }
        } else if entry.kind == EntryKind::OperatorSay {
            self.pending_commands.remove(&command_id);
        }
    }

    /// The banner text: the read error, else the missing-lifecycle notice,
    /// else the terminal-state pin, else nothing.
    pub fn banner_text(&self) -> Option<String> {
        if let Some(error) = &self.last_error {
            return Some(error.clone());
        }
        let status = self.last_status.as_ref()?;
        if status.missing {
            return Some(format!(
                "run '{}' not found in store (was it dropped?)",
                self.run_id
            ));
        }
        match status.status {
            Some(s) if is_terminal(s) => Some(format!(
                "-- terminal status: {} -- transcript scrollable; steering disabled.",
                s.as_str()
            )),
            _ => None,
        }
    }

    /// The gate line when the run is parked on a manual gate.
    pub fn gate_text(&self) -> Option<String> {
        self.last_status
            .as_ref()
            .and_then(|s| s.awaiting_instruction.as_ref())
            .filter(|i| !i.is_empty())
            .map(|instruction| format!("manual gate awaiting approval: {instruction}"))
    }

    /// Whether the new-activity indicator is showing.
    pub fn indicator_visible(&self) -> bool {
        !self.follow && self.new_activity
    }

    /// The pending-commands lines: `[#id] kind pending: summary` or
    /// `[#id] kind failed: <error>`.
    pub fn pending_lines(&self) -> Vec<String> {
        self.pending_commands
            .values()
            .map(|command| match command.status {
                PendingState::Failed => format!(
                    "[#{}] {} failed: {}",
                    command.command_id,
                    command.kind,
                    command.error_detail.as_deref().unwrap_or("(no detail)")
                ),
                PendingState::Pending => {
                    let mut line = format!("[#{}] {} pending", command.command_id, command.kind);
                    if !command.summary.is_empty() {
                        line.push_str(&format!(": {}", command.summary));
                    }
                    line
                }
            })
            .collect()
    }

    fn can_say_or_interrupt(&self) -> bool {
        self.last_status
            .as_ref()
            .and_then(|s| s.status)
            .is_some_and(|s| STEERABLE_STATUSES.contains(&s))
    }

    fn can_approve_or_reject(&self) -> bool {
        self.last_status
            .as_ref()
            .and_then(|s| s.status)
            .is_some_and(|s| APPROVABLE_STATUSES.contains(&s))
    }

    /// The steering help line advertising only the verbs that would
    /// succeed for the current status; `None` when steering is disabled.
    pub fn steering_help_text(&self) -> Option<String> {
        self.enqueue.as_ref()?;
        let mut parts = Vec::new();
        if self.can_say_or_interrupt() {
            parts.push("enter=say  ctrl+x=interrupt");
        }
        if self.can_approve_or_reject() {
            parts.push("ctrl+y=approve  ctrl+r=reject (input value = feedback)");
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("  "))
        }
    }

    // ----- Keys ---------------------------------------------------------------

    fn page(&self) -> usize {
        self.viewport_height.max(1)
    }

    /// Flip follow off, anchoring the viewport where it is.
    fn pause_follow(&mut self) {
        if self.follow {
            self.follow = false;
            self.scroll = self.last_top;
        }
    }

    /// Resume tail-follow and snap the viewport to the bottom.
    pub fn follow_end(&mut self) {
        self.follow = true;
        self.new_activity = false;
    }

    pub fn scroll_top(&mut self) {
        self.pause_follow();
        self.scroll = 0;
    }

    pub fn scroll_up(&mut self) {
        self.pause_follow();
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub fn page_up(&mut self) {
        self.pause_follow();
        self.scroll = self.scroll.saturating_sub(self.page());
    }

    pub fn scroll_down(&mut self) {
        if !self.follow {
            self.scroll = self.scroll.saturating_add(1);
        }
    }

    pub fn page_down(&mut self) {
        if !self.follow {
            self.scroll = self.scroll.saturating_add(self.page());
        }
    }

    /// Route one mouse event: the wheel scrolls the transcript
    /// ([`MOUSE_SCROLL_LINES`] per notch). Scrolling up pauses tail-follow
    /// exactly as the Up key does; End (or the keyboard) resumes it.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> SessionAction {
        match event.kind {
            MouseEventKind::ScrollUp => {
                for _ in 0..MOUSE_SCROLL_LINES {
                    self.scroll_up();
                }
            }
            MouseEventKind::ScrollDown => {
                for _ in 0..MOUSE_SCROLL_LINES {
                    self.scroll_down();
                }
            }
            _ => {}
        }
        SessionAction::None
    }

    /// Route one key press. Navigation keys drive the transcript; the
    /// ctrl chords steer; everything else edits the compose box.
    pub fn handle_key(&mut self, key: KeyEvent) -> SessionAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => return SessionAction::RequestQuit,
                KeyCode::Char('x') => {
                    self.submit_interrupt();
                    return SessionAction::None;
                }
                KeyCode::Char('y') => {
                    self.submit_approve();
                    return SessionAction::None;
                }
                KeyCode::Char('r') => {
                    self.reject_with_compose_feedback();
                    return SessionAction::None;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => return SessionAction::Back,
            KeyCode::End => {
                self.follow_end();
                return SessionAction::None;
            }
            KeyCode::Home => {
                self.scroll_top();
                return SessionAction::None;
            }
            KeyCode::PageUp => {
                self.page_up();
                return SessionAction::None;
            }
            KeyCode::PageDown => {
                self.page_down();
                return SessionAction::None;
            }
            KeyCode::Up => {
                self.scroll_up();
                return SessionAction::None;
            }
            KeyCode::Down => {
                self.scroll_down();
                return SessionAction::None;
            }
            _ => {}
        }
        match self.compose.handle_key(key) {
            InputEvent::Submitted(raw) => self.submit_compose(&raw),
            _ => SessionAction::None,
        }
    }

    /// Compose-box submit: slash command, or fall through to `say`.
    pub fn submit_compose(&mut self, raw: &str) -> SessionAction {
        if is_slash(raw) {
            return self.handle_slash(raw);
        }
        if raw.trim_start().starts_with('/') {
            // A bare `/` (or `/` plus whitespace) is not a verb; it is
            // never forwarded to the agent as a `say`.
            self.notice = Some(unknown_command_notice(""));
            return SessionAction::None;
        }
        let text = raw.trim();
        if text.is_empty() {
            self.notice = Some("say message must be non-empty".to_owned());
            return SessionAction::None;
        }
        self.submit_say(text);
        self.compose.clear();
        SessionAction::None
    }

    /// Dispatch one slash command line. Errors leave the input populated
    /// so the operator can edit and resubmit; successes clear it.
    fn handle_slash(&mut self, raw: &str) -> SessionAction {
        let Some(command) = parse_slash(raw) else {
            return SessionAction::None;
        };
        let verb = command.verb.as_str();
        if verb.is_empty() {
            self.notice = Some(unknown_command_notice(""));
            return SessionAction::None;
        }
        match verb {
            SLASH_HELP => {
                self.notice = Some(help_text());
                self.compose.clear();
            }
            SLASH_STATUS => {
                self.notice = Some(self.status_text());
                self.compose.clear();
            }
            SLASH_APPROVE => {
                self.submit_approve();
                self.compose.clear();
            }
            SLASH_REJECT => {
                let feedback = (!command.argument.is_empty()).then_some(command.argument.as_str());
                self.submit_reject(feedback);
                self.compose.clear();
            }
            SLASH_INTERRUPT => {
                self.submit_interrupt();
                self.compose.clear();
            }
            SLASH_ARCHIVE => {
                self.handle_archive();
                self.compose.clear();
            }
            SLASH_WORKER => {
                self.compose.clear();
                return SessionAction::WorkerSlash(command.argument);
            }
            SLASH_AUTOPILOT => {
                self.compose.clear();
                return SessionAction::AutopilotSlash(command.argument);
            }
            SLASH_QUIT => {
                self.compose.clear();
                return SessionAction::RequestQuit;
            }
            SLASH_EXIT => {
                self.compose.clear();
                return SessionAction::ExitAll;
            }
            _ => self.notice = Some(unknown_command_notice(verb)),
        }
        SessionAction::None
    }

    /// The viewed run's lifecycle status for `/status`.
    pub fn status_text(&self) -> String {
        let Some(status) = &self.last_status else {
            return format!("run {}: status unknown", self.run_id);
        };
        if status.missing {
            return format!("run {}: not found in store", self.run_id);
        }
        let Some(current) = status.status else {
            return format!("run {}: lifecycle in unresolved state", self.run_id);
        };
        let mut line = format!("run {}: status={}", self.run_id, current.as_str());
        if let Some(instruction) = status
            .awaiting_instruction
            .as_ref()
            .filter(|i| !i.is_empty())
        {
            line.push_str(&format!("; gate={instruction}"));
        }
        line
    }

    fn handle_archive(&mut self) {
        let Some(archive) = self.archive.as_mut() else {
            self.notice = Some("/archive is not wired on this screen (no archive seam)".to_owned());
            return;
        };
        self.notice = Some(match archive() {
            Err(err) => format!("/archive failed: {err}"),
            Ok(moved) if moved.is_empty() => "/archive: no phases archived".to_owned(),
            Ok(moved) => format!("/archive: moved {}", moved.join(", ")),
        });
    }

    fn reject_with_compose_feedback(&mut self) {
        let raw = self.compose.value().trim().to_owned();
        let feedback = if raw.is_empty() {
            None
        } else {
            self.compose.clear();
            Some(raw)
        };
        self.submit_reject(feedback.as_deref());
    }

    // ----- Steering ------------------------------------------------------------

    /// Enqueue a `say`; `None` when steering is disabled or the run left
    /// the steerable set between render and submit (an inline notice
    /// surfaces; the store is untouched).
    pub fn submit_say(&mut self, text: &str) -> Option<i64> {
        let mut payload = JsonObject::new();
        payload.insert("text".to_owned(), Value::String(text.to_owned()));
        self.enqueue_steering(
            CONTROL_COMMAND_SAY,
            payload,
            short(text, 60),
            &STEERABLE_STATUSES,
        )
    }

    pub fn submit_interrupt(&mut self) -> Option<i64> {
        self.enqueue_steering(
            CONTROL_COMMAND_INTERRUPT,
            JsonObject::new(),
            String::new(),
            &STEERABLE_STATUSES,
        )
    }

    pub fn submit_approve(&mut self) -> Option<i64> {
        self.enqueue_steering(
            CONTROL_COMMAND_APPROVE,
            JsonObject::new(),
            String::new(),
            &APPROVABLE_STATUSES,
        )
    }

    /// Enqueue a `reject`; empty / whitespace feedback is omitted from the
    /// payload so it matches the watcher's expected shape.
    pub fn submit_reject(&mut self, feedback: Option<&str>) -> Option<i64> {
        let mut payload = JsonObject::new();
        let mut summary = String::new();
        if let Some(text) = feedback.filter(|f| !f.trim().is_empty()) {
            payload.insert("feedback".to_owned(), Value::String(text.to_owned()));
            summary = short(text, 60);
        }
        self.enqueue_steering(
            CONTROL_COMMAND_REJECT,
            payload,
            summary,
            &APPROVABLE_STATUSES,
        )
    }

    /// Single enqueue path shared by the four steering verbs, with the
    /// submit-time status re-check.
    fn enqueue_steering(
        &mut self,
        kind: &str,
        payload: JsonObject,
        summary: String,
        permitted: &[Status],
    ) -> Option<i64> {
        self.enqueue.as_ref()?;
        let current = self.last_status.as_ref().and_then(|s| s.status);
        match current {
            Some(status) if permitted.contains(&status) => {}
            _ => {
                self.notice = Some(format!(
                    "run is not steerable for '{kind}' (status={})",
                    current.map_or("unknown", Status::as_str)
                ));
                return None;
            }
        }
        let enqueue = self.enqueue.as_mut()?;
        let command_id = match enqueue(kind, payload) {
            Ok(id) => id,
            Err(err) => {
                self.notice = Some(format!("enqueue failed: {err}"));
                return None;
            }
        };
        self.pending_commands.insert(
            command_id,
            PendingCommand {
                command_id,
                kind: kind.to_owned(),
                summary,
                status: PendingState::Pending,
                error_detail: None,
            },
        );
        self.notice = None;
        Some(command_id)
    }

    // ----- Render ------------------------------------------------------------

    fn ensure_wrapped(&mut self, width: usize) {
        if width != self.wrapped_width {
            self.wrapped_width = width;
            self.wrapped.clear();
            self.wrapped_upto = 0;
        }
        for line in &self.lines[self.wrapped_upto..] {
            self.wrapped.extend(wrap_line(line, width));
        }
        self.wrapped_upto = self.lines.len();
    }

    /// Draw the header, gate, banner, transcript, indicator, pending
    /// list, notice, steering help, and compose box.
    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let width = usize::from(area.width);
        let gate_lines = self
            .gate_text()
            .map(|t| wrap_plain(&t, width))
            .unwrap_or_default();
        let banner = self.banner_text();
        let pending = self.pending_lines();
        let notice_lines: Vec<Line<'static>> = self
            .notice
            .as_deref()
            .map(|n| n.lines().flat_map(|l| wrap_plain(l, width)).collect())
            .unwrap_or_default();
        let steering = self.steering_help_text();
        let clamp = |n: usize, max: u16| u16::try_from(n).unwrap_or(max).min(max);
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(clamp(gate_lines.len(), MAX_GATE_ROWS)),
            Constraint::Length(u16::from(banner.is_some())),
            Constraint::Min(1),
            Constraint::Length(u16::from(self.indicator_visible())),
            Constraint::Length(clamp(pending.len(), MAX_PENDING_ROWS)),
            Constraint::Length(clamp(notice_lines.len(), MAX_NOTICE_ROWS)),
            Constraint::Length(u16::from(steering.is_some())),
            Constraint::Length(1),
        ])
        .split(area);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("run=", dim()),
                Span::styled(self.run_id.clone(), bold(Color::White)),
                Span::raw("  "),
                Span::styled("task=", dim()),
                Span::styled(self.task_id.clone(), bold(Color::White)),
            ])),
            chunks[0],
        );
        if !gate_lines.is_empty() {
            frame.render_widget(
                Paragraph::new(Text::from(gate_lines))
                    .style(Style::default().bg(Color::Red).fg(Color::White)),
                chunks[1],
            );
        }
        if let Some(banner) = banner {
            frame.render_widget(
                Paragraph::new(banner).style(Style::default().fg(Color::Yellow)),
                chunks[2],
            );
        }

        let transcript_area = chunks[3];
        self.ensure_wrapped(usize::from(transcript_area.width));
        let height = usize::from(transcript_area.height);
        let total = self.wrapped.len();
        let max_top = total.saturating_sub(height);
        let top = if self.follow {
            max_top
        } else {
            self.scroll = self.scroll.min(max_top);
            self.scroll
        };
        self.last_top = top;
        self.viewport_height = height;
        let visible: Vec<Line<'static>> = self
            .wrapped
            .iter()
            .skip(top)
            .take(height)
            .cloned()
            .collect();
        frame.render_widget(Paragraph::new(Text::from(visible)), transcript_area);

        if self.indicator_visible() {
            frame.render_widget(
                Paragraph::new("new activity below -- press End to resume follow").style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                chunks[4],
            );
        }
        if !pending.is_empty() {
            let lines: Vec<Line<'static>> = pending.into_iter().map(Line::from).collect();
            frame.render_widget(
                Paragraph::new(Text::from(lines)).style(Style::default().fg(Color::Cyan)),
                chunks[5],
            );
        }
        if !notice_lines.is_empty() {
            frame.render_widget(
                Paragraph::new(Text::from(notice_lines)).style(Style::default().fg(Color::Yellow)),
                chunks[6],
            );
        }
        if let Some(help) = steering {
            frame.render_widget(Paragraph::new(help).style(dim()), chunks[7]);
        }
        self.compose.render(frame, chunks[8]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::{buffer_text, ctrl, key, terminal};
    use chrono::{DateTime, TimeZone, Utc};
    use flywheel_core::lifecycle::Lifecycle;
    use flywheel_core::store::{ControlCommandStore, LifecycleStore};
    use flywheel_core::store_memory::InMemoryStore;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::Arc;

    const TS: &str = "12:00:00";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 10, 12, 0, 0).unwrap()
    }

    fn entry(kind: EntryKind, header: &str, body: &str, sequence: u64) -> TranscriptEntry {
        TranscriptEntry {
            sequence,
            sub_index: 0,
            ts: now(),
            kind,
            header: header.to_owned(),
            body: body.to_owned(),
            attempt_number: Some(1),
            iteration_number: Some(1),
            control_command_id: None,
            control_command_error: None,
        }
    }

    fn running() -> SessionStatus {
        SessionStatus::new(Some(Status::Running), None)
    }

    fn terminal_status(status: Status) -> SessionStatus {
        SessionStatus::new(Some(status), None)
    }

    fn awaiting(instruction: &str) -> SessionStatus {
        SessionStatus::new(Some(Status::AwaitingApproval), Some(instruction.to_owned()))
    }

    /// Hands out queued entry pages, one per fetch.
    type Queue = Rc<RefCell<VecDeque<Vec<TranscriptEntry>>>>;

    fn scripted_fetch() -> (Queue, impl FnMut() -> Result<Vec<TranscriptEntry>, String>) {
        let queue: Queue = Rc::new(RefCell::new(VecDeque::new()));
        let probe = Rc::clone(&queue);
        (queue, move || {
            Ok(probe.borrow_mut().pop_front().unwrap_or_default())
        })
    }

    /// Returns the head of a status queue, holding at the last value.
    fn scripted_status(
        initial: SessionStatus,
    ) -> (
        Rc<RefCell<VecDeque<SessionStatus>>>,
        impl FnMut() -> Result<SessionStatus, String>,
    ) {
        let queue = Rc::new(RefCell::new(VecDeque::from([initial])));
        let probe = Rc::clone(&queue);
        (queue, move || {
            let mut q = probe.borrow_mut();
            if q.len() > 1 {
                q.pop_front();
            }
            Ok(q.front().cloned().unwrap())
        })
    }

    fn store_enqueue(
        store: Arc<InMemoryStore>,
        run_id: &str,
    ) -> impl FnMut(&str, JsonObject) -> Result<i64, String> + use<> {
        let run_id = run_id.to_owned();
        move |kind, payload| {
            store
                .enqueue_command(&run_id, kind, payload, Utc::now())
                .map_err(|e| e.to_string())
                .map(|r| r.id.unwrap())
        }
    }

    fn running_store(task_id: &str) -> (Arc<InMemoryStore>, String) {
        let store = Arc::new(InMemoryStore::new());
        let mut lc = Lifecycle::new(task_id);
        lc.run_id = format!("run-{task_id}");
        lc.transition_to(Status::Ready, "", Some(now())).unwrap();
        lc.transition_to(Status::Running, "", Some(now())).unwrap();
        store.create_lifecycle(&lc).unwrap();
        (store, lc.run_id)
    }

    #[test]
    fn renders_all_message_classes_with_block_layout() {
        let (queue, fetch) = scripted_fetch();
        let prose = "planning the edit\n\nstep two: write the test";
        queue.borrow_mut().push_back(vec![
            entry(EntryKind::AgentText, "agent", prose, 1),
            entry(EntryKind::ToolCall, "tool(Edit)", "README.md", 2),
            entry(EntryKind::ToolResult, "tool_result", "ok", 3),
            entry(
                EntryKind::OperatorSay,
                "operator(say)",
                "please add docstrings",
                4,
            ),
            entry(
                EntryKind::Lifecycle,
                "harness.iteration_completed",
                "iteration=1",
                5,
            ),
            entry(
                EntryKind::Gate,
                "gate(awaiting)",
                "grader=review-migration",
                6,
            ),
        ]);
        let mut screen = SessionScreen::new("run-x", "task-alpha", fetch, || Ok(running()));
        screen.refresh_now();
        assert_eq!(screen.entries.len(), 6);
        let kinds: Vec<EntryKind> = screen.entries.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                EntryKind::AgentText,
                EntryKind::ToolCall,
                EntryKind::ToolResult,
                EntryKind::OperatorSay,
                EntryKind::Lifecycle,
                EntryKind::Gate
            ]
        );
        assert_eq!(screen.entries[0].body, prose);
        // Leading-blank pattern: none before the first entry; blank before
        // each group change; none between a call and its result; none
        // between two lifecycle-group entries.
        let expected_blank = [false, true, false, true, true, false];
        let mut prev: Option<&TranscriptEntry> = None;
        for (entry, blank) in screen.entries.iter().zip(expected_blank) {
            assert_eq!(
                needs_blank_separator(prev, entry),
                blank,
                "{}",
                entry.header
            );
            let plain = render_entry_plain(entry, blank);
            assert_eq!(plain.starts_with('\n'), blank);
            assert_eq!(plain.matches(TS).count(), 1);
            prev = Some(entry);
        }
        let mut term = terminal(100, 24);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("run=run-x  task=task-alpha"));
        assert!(text.contains("12:00:00  agent"));
        assert!(text.contains("planning the edit"));
        assert!(text.contains("step two: write the test"));
        assert!(text.contains("12:00:00  tool(Edit)  README.md"));
        assert!(text.contains("12:00:00    tool_result  ok"));
        assert!(text.contains("operator(say)  please add docstrings"));
        assert!(text.contains("gate(awaiting)  grader=review-migration"));
        assert!(text.contains("> say message or /help"), "{text}");
    }

    #[test]
    fn render_entry_text_layouts() {
        let single = entry(EntryKind::AgentText, "agent", "ack", 1);
        assert_eq!(
            render_entry_plain(&single, false),
            format!("{TS}  agent  ack")
        );
        let ok = entry(EntryKind::ToolResult, "tool_result", "ok", 1);
        assert_eq!(
            render_entry_plain(&ok, false),
            format!("{TS}    tool_result  ok")
        );
        let err = entry(
            EntryKind::ToolResult,
            "tool_result(error)",
            "first error\nsecond error\n... +20 more lines",
            1,
        );
        assert_eq!(
            render_entry_plain(&err, false),
            format!(
                "{TS}    tool_result(error)\n    first error\n    second error\n    ... +20 more lines"
            )
        );
        let rendered = render_entry_text(&err, false);
        for (i, line) in rendered.lines.iter().enumerate() {
            for span in &line.spans {
                let is_timestamp = i == 0 && span.content == TS;
                if !is_timestamp {
                    assert!(
                        !span.style.add_modifier.contains(Modifier::DIM),
                        "dim on error content"
                    );
                }
            }
        }
        let inline_err = entry(EntryKind::ToolResult, "tool_result(error)", "boom", 1);
        assert_eq!(
            render_entry_plain(&inline_err, false),
            format!("{TS}    tool_result(error)  boom")
        );
        let multi = entry(
            EntryKind::AgentText,
            "agent",
            "first paragraph\n\nsecond paragraph",
            1,
        );
        assert_eq!(
            render_entry_plain(&multi, false),
            format!("{TS}  agent\nfirst paragraph\n\nsecond paragraph")
        );
        let first_line = &render_entry_text(&multi, false).lines[0];
        assert_eq!(first_line.spans[0].content, TS);
        assert!(
            first_line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
        let call = entry(EntryKind::ToolCall, "tool(Edit)", "README.md", 1);
        let plain = render_entry_plain(&call, true);
        assert_eq!(plain, format!("\n{TS}  tool(Edit)  README.md"));
        assert_eq!(plain.matches('\n').count(), 1);
        let empty_body = entry(EntryKind::Lifecycle, "harness.x", "", 1);
        assert_eq!(
            render_entry_plain(&empty_body, false),
            format!("{TS}  harness.x")
        );
        let empty_result = entry(EntryKind::ToolResult, "tool_result(error)", "", 1);
        assert_eq!(
            render_entry_plain(&empty_result, false),
            format!("{TS}    tool_result(error)")
        );
    }

    #[test]
    fn blank_separator_persists_across_fetches() {
        let (queue, fetch) = scripted_fetch();
        queue
            .borrow_mut()
            .push_back(vec![entry(EntryKind::AgentText, "agent", "hello", 1)]);
        let mut screen = SessionScreen::new("run-x", "task-alpha", fetch, || Ok(running()));
        screen.refresh_now();
        assert_eq!(screen.entries.len(), 1);
        assert_eq!(screen.lines.len(), 1);
        queue.borrow_mut().push_back(vec![entry(
            EntryKind::ToolCall,
            "tool(Edit)",
            "README.md",
            2,
        )]);
        screen.refresh_now();
        assert_eq!(screen.entries.len(), 2);
        // The second entry arrived in a later fetch and still got its
        // leading blank line: three rendered lines, the middle one empty.
        assert_eq!(screen.lines.len(), 3);
        assert!(screen.lines[1].spans.is_empty());
    }

    #[test]
    fn styles_never_double_dim_errors() {
        let tool_err = entry(EntryKind::ToolResult, "tool_result(error)", "boom", 1);
        let mut control = entry(
            EntryKind::Lifecycle,
            "control",
            "say failed: SDKDisconnected: closed",
            2,
        );
        control.control_command_id = Some(7);
        control.control_command_error = Some("SDKDisconnected: closed".to_owned());
        for e in [&tool_err, &control] {
            assert!(is_error_entry(e));
            let (h, b) = resolve_styles(e);
            assert!(
                !(h.add_modifier.contains(Modifier::DIM) && b.add_modifier.contains(Modifier::DIM))
            );
            assert_eq!(h.fg, Some(Color::Red));
            assert_eq!(b.fg, Some(Color::Red));
        }
        let lifecycle = entry(
            EntryKind::Lifecycle,
            "iteration",
            "iteration 3 | 1.2k tokens",
            1,
        );
        assert!(!is_error_entry(&lifecycle));
        let (h, b) = resolve_styles(&lifecycle);
        assert!(h.add_modifier.contains(Modifier::DIM));
        assert!(b.add_modifier.contains(Modifier::DIM));
        let agent = entry(EntryKind::AgentText, "agent", "hello there", 1);
        assert!(
            !resolve_styles(&agent)
                .1
                .add_modifier
                .contains(Modifier::DIM)
        );
        let a = entry(EntryKind::ToolCall, "tool(Read)", "a.py", 1);
        let b = entry(EntryKind::ToolCall, "tool(Read)", "b.py", 1);
        assert_eq!(block_group(&a), block_group(&b));
        assert!(!needs_blank_separator(Some(&a), &b));
    }

    #[test]
    fn mouse_wheel_scrolls_the_transcript_and_pauses_follow() {
        let (queue, fetch) = scripted_fetch();
        let mut screen = SessionScreen::new("run-x", "task-alpha", fetch, || Ok(running()));
        queue.borrow_mut().push_back(
            (0..40)
                .map(|i| entry(EntryKind::AgentText, "agent", &format!("line {i}"), i))
                .collect(),
        );
        screen.refresh_now();
        let mut term = terminal(80, 12);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        assert!(screen.follow());
        let bottom = screen.last_top;
        let wheel = |kind| MouseEvent {
            kind,
            column: 1,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            screen.handle_mouse(wheel(MouseEventKind::ScrollUp)),
            SessionAction::None
        );
        assert!(!screen.follow());
        term.draw(|f| screen.render(f, f.area())).unwrap();
        assert_eq!(screen.last_top, bottom - MOUSE_SCROLL_LINES);
        screen.handle_mouse(wheel(MouseEventKind::ScrollDown));
        term.draw(|f| screen.render(f, f.area())).unwrap();
        assert_eq!(screen.last_top, bottom);
    }

    #[test]
    fn scroll_up_pauses_follow_and_end_resumes() {
        let (queue, fetch) = scripted_fetch();
        let mut screen = SessionScreen::new("run-x", "task-alpha", fetch, || Ok(running()));
        screen.refresh_now();
        assert!(screen.follow());
        assert!(!screen.indicator_visible());
        assert_eq!(screen.handle_key(key(KeyCode::PageUp)), SessionAction::None);
        assert!(!screen.follow());
        queue.borrow_mut().push_back(vec![entry(
            EntryKind::AgentText,
            "agent",
            "new line while paused",
            10,
        )]);
        screen.refresh_now();
        assert!(screen.new_activity());
        assert!(screen.indicator_visible());
        let mut term = terminal(80, 12);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        assert!(buffer_text(&term).contains("new activity below -- press End to resume follow"));
        screen.handle_key(key(KeyCode::End));
        assert!(screen.follow());
        assert!(!screen.new_activity());
        assert!(!screen.indicator_visible());
        screen.handle_key(key(KeyCode::Up));
        assert!(!screen.follow());
        screen.handle_key(key(KeyCode::Down));
        screen.handle_key(key(KeyCode::PageDown));
        assert!(!screen.follow());
        screen.handle_key(key(KeyCode::Home));
        assert_eq!(screen.scroll, 0);
    }

    #[test]
    fn paused_viewport_anchors_and_follow_pins_bottom() {
        let (queue, fetch) = scripted_fetch();
        let page: Vec<TranscriptEntry> = (0..30)
            .map(|i| {
                entry(
                    EntryKind::Lifecycle,
                    "iteration",
                    &format!("iteration {i}"),
                    i,
                )
            })
            .collect();
        queue.borrow_mut().push_back(page);
        let mut screen = SessionScreen::new("run-x", "task-alpha", fetch, || Ok(running()));
        screen.refresh_now();
        let mut term = terminal(60, 12);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("iteration 29"));
        assert!(!text.contains("iteration 0\n"));
        screen.handle_key(key(KeyCode::Home));
        term.draw(|f| screen.render(f, f.area())).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("iteration 0"));
        assert!(!text.contains("iteration 29"));
        screen.handle_key(key(KeyCode::PageDown));
        term.draw(|f| screen.render(f, f.area())).unwrap();
        assert!(buffer_text(&term).contains("iteration 1"));
    }

    #[test]
    fn terminal_status_shows_banner_and_hides_steering_help() {
        let (_queue, fetch) = scripted_fetch();
        let (statuses, status) = scripted_status(running());
        let mut screen =
            SessionScreen::new("run-x", "task-alpha", fetch, status).with_enqueue(|_, _| Ok(1));
        screen.refresh_now();
        assert_eq!(screen.banner_text(), None);
        assert_eq!(
            screen.steering_help_text().as_deref(),
            Some("enter=say  ctrl+x=interrupt")
        );
        statuses
            .borrow_mut()
            .push_back(terminal_status(Status::Done));
        screen.refresh_now();
        let banner = screen.banner_text().unwrap();
        assert!(banner.contains("terminal status: done"));
        assert!(banner.contains("steering disabled"));
        assert_eq!(screen.steering_help_text(), None);
        let mut term = terminal(100, 10);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        assert!(buffer_text(&term).contains("-- terminal status: done --"));
    }

    #[test]
    fn awaiting_approval_surfaces_gate_instruction() {
        let (_queue, fetch) = scripted_fetch();
        let mut screen = SessionScreen::new("run-x", "task-alpha", fetch, || {
            Ok(awaiting("Confirm the migration before merging."))
        })
        .with_enqueue(|_, _| Ok(1));
        screen.refresh_now();
        assert_eq!(
            screen.gate_text().as_deref(),
            Some("manual gate awaiting approval: Confirm the migration before merging.")
        );
        assert_eq!(
            screen.steering_help_text().as_deref(),
            Some("ctrl+y=approve  ctrl+r=reject (input value = feedback)")
        );
        let mut term = terminal(100, 10);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("manual gate awaiting approval: Confirm the migration"));
        assert!(text.contains("ctrl+y=approve"));
        assert_eq!(
            screen.status_text(),
            "run run-x: status=awaiting_approval; gate=Confirm the migration before merging."
        );
    }

    #[test]
    fn empty_transcript_renders_and_follows_arrivals() {
        let (queue, fetch) = scripted_fetch();
        let mut screen = SessionScreen::new("run-x", "task-alpha", fetch, || Ok(running()));
        screen.refresh_now();
        assert!(screen.entries.is_empty());
        let mut term = terminal(60, 8);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        queue
            .borrow_mut()
            .push_back(vec![entry(EntryKind::AgentText, "agent", "hello", 1)]);
        screen.refresh_now();
        assert_eq!(screen.entries.len(), 1);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        assert!(buffer_text(&term).contains("agent  hello"));
    }

    #[test]
    fn fetch_and_status_errors_are_contained() {
        let calls = Rc::new(RefCell::new(0));
        let probe = Rc::clone(&calls);
        let fetch = move || {
            *probe.borrow_mut() += 1;
            if *probe.borrow() == 1 {
                Ok(vec![entry(EntryKind::AgentText, "agent", "kept", 1)])
            } else {
                Err("db locked".to_owned())
            }
        };
        let mut screen = SessionScreen::new("run-x", "t", fetch, || Ok(running()));
        screen.refresh_now();
        screen.refresh_now();
        assert_eq!(screen.entries.len(), 1);
        assert_eq!(
            screen.banner_text().as_deref(),
            Some("transcript read failed: db locked")
        );
        let mut screen =
            SessionScreen::new("run-x", "t", || Ok(Vec::new()), || Err("gone".to_owned()));
        screen.refresh_now();
        assert_eq!(
            screen.banner_text().as_deref(),
            Some("status read failed: gone")
        );
        assert_eq!(screen.status_text(), "run run-x: status unknown");
        let mut screen = SessionScreen::new(
            "run-x",
            "t",
            || Ok(Vec::new()),
            || Ok(SessionStatus::missing()),
        );
        screen.refresh_now();
        assert_eq!(
            screen.banner_text().as_deref(),
            Some("run 'run-x' not found in store (was it dropped?)")
        );
        assert_eq!(screen.status_text(), "run run-x: not found in store");
        let mut screen = SessionScreen::new(
            "run-x",
            "t",
            || Ok(Vec::new()),
            || Ok(SessionStatus::new(None, None)),
        );
        screen.refresh_now();
        assert_eq!(
            screen.status_text(),
            "run run-x: lifecycle in unresolved state"
        );
    }

    #[test]
    fn say_enqueues_one_command_with_text_payload() {
        let (store, run_id) = running_store("alpha");
        let mut screen = SessionScreen::new(&run_id, "alpha", || Ok(Vec::new()), || Ok(running()))
            .with_enqueue(store_enqueue(Arc::clone(&store), &run_id));
        screen.refresh_now();
        let id = screen.submit_say("please add docstrings").unwrap();
        let claimed = store.claim_commands(&run_id, Utc::now()).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].kind, "say");
        assert_eq!(claimed[0].payload["text"], "please add docstrings");
        assert_eq!(screen.pending_commands[&id].kind, "say");
        assert_eq!(
            screen.pending_lines(),
            vec![format!("[#{id}] say pending: please add docstrings")]
        );
    }

    #[test]
    fn interrupt_enqueues_with_empty_payload_via_key_and_slash() {
        let (store, run_id) = running_store("alpha");
        let mut screen = SessionScreen::new(&run_id, "alpha", || Ok(Vec::new()), || Ok(running()))
            .with_enqueue(store_enqueue(Arc::clone(&store), &run_id));
        screen.refresh_now();
        assert_eq!(screen.handle_key(ctrl('x')), SessionAction::None);
        screen.compose_mut().set_value("/interrupt");
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), SessionAction::None);
        assert_eq!(screen.compose().value(), "");
        let claimed = store.claim_commands(&run_id, Utc::now()).unwrap();
        assert_eq!(
            claimed.iter().map(|c| c.kind.as_str()).collect::<Vec<_>>(),
            vec!["interrupt", "interrupt"]
        );
        assert!(claimed[0].payload.is_empty());
    }

    #[test]
    fn approve_and_reject_only_when_awaiting_approval() {
        let store = Arc::new(InMemoryStore::new());
        let run_id = "run-gated";
        let mut lc = Lifecycle::new("gated");
        lc.run_id = run_id.to_owned();
        for s in [
            Status::Ready,
            Status::Running,
            Status::Validating,
            Status::AwaitingApproval,
        ] {
            lc.transition_to(s, "", Some(now())).unwrap();
        }
        store.create_lifecycle(&lc).unwrap();
        let mut screen = SessionScreen::new(
            run_id,
            "gated",
            || Ok(Vec::new()),
            || Ok(awaiting("Confirm the migration.")),
        )
        .with_enqueue(store_enqueue(Arc::clone(&store), run_id));
        screen.refresh_now();
        assert!(screen.submit_approve().is_some());
        assert!(screen.submit_reject(None).is_some());
        assert!(screen.submit_reject(Some("   ")).is_some());
        assert!(
            screen
                .submit_reject(Some("please redo the migration"))
                .is_some()
        );
        screen.compose_mut().set_value("typed feedback");
        screen.handle_key(ctrl('r'));
        assert_eq!(screen.compose().value(), "");
        screen.compose_mut().set_value("/reject via slash");
        screen.handle_key(key(KeyCode::Enter));
        screen.compose_mut().set_value("/approve");
        screen.handle_key(key(KeyCode::Enter));
        screen.handle_key(ctrl('y'));
        let claimed = store.claim_commands(run_id, Utc::now()).unwrap();
        let kinds: Vec<&str> = claimed.iter().map(|c| c.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "approve", "reject", "reject", "reject", "reject", "reject", "approve", "approve"
            ]
        );
        assert!(claimed[1].payload.is_empty());
        assert!(claimed[2].payload.is_empty());
        assert_eq!(claimed[3].payload["feedback"], "please redo the migration");
        assert_eq!(claimed[4].payload["feedback"], "typed feedback");
        assert_eq!(claimed[5].payload["feedback"], "via slash");
        // say / interrupt are refused on a parked run.
        assert_eq!(screen.submit_say("hi"), None);
        assert_eq!(
            screen.notice(),
            Some("run is not steerable for 'say' (status=awaiting_approval)")
        );
    }

    #[test]
    fn pending_to_applied_and_failed_transitions() {
        let (store, run_id) = running_store("alpha");
        let (queue, fetch) = scripted_fetch();
        let mut screen = SessionScreen::new(&run_id, "alpha", fetch, || Ok(running()))
            .with_enqueue(store_enqueue(Arc::clone(&store), &run_id));
        screen.refresh_now();
        let said = screen.submit_say("ship it").unwrap();
        let oops = screen.submit_say("oops").unwrap();
        let interrupt = screen.submit_interrupt().unwrap();
        assert_eq!(screen.pending_commands.len(), 3);
        let mut applied = entry(EntryKind::OperatorSay, "operator(say)", "ship it", 20);
        applied.control_command_id = Some(said);
        let mut failed = entry(
            EntryKind::Lifecycle,
            "control",
            "say failed: SDKDisconnected: closed",
            21,
        );
        failed.control_command_id = Some(oops);
        failed.control_command_error = Some("SDKDisconnected: session was closed".to_owned());
        let mut control_applied = entry(EntryKind::Lifecycle, "control", "interrupt applied", 22);
        control_applied.control_command_id = Some(interrupt);
        let mut stranger = entry(EntryKind::OperatorSay, "operator(say)", "from the CLI", 23);
        stranger.control_command_id = Some(said + 999);
        queue
            .borrow_mut()
            .push_back(vec![applied, failed, control_applied, stranger]);
        screen.refresh_now();
        assert!(!screen.pending_commands.contains_key(&said));
        assert!(!screen.pending_commands.contains_key(&interrupt));
        let pending = &screen.pending_commands[&oops];
        assert_eq!(pending.status, PendingState::Failed);
        assert!(
            pending
                .error_detail
                .as_deref()
                .unwrap()
                .contains("SDKDisconnected")
        );
        assert_eq!(
            screen.pending_lines(),
            vec![format!(
                "[#{oops}] say failed: SDKDisconnected: session was closed"
            )]
        );
        let mut term = terminal(100, 12);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("say failed: SDKDisconnected"));
    }

    #[test]
    fn steering_disabled_on_done_run_writes_nothing() {
        let store = Arc::new(InMemoryStore::new());
        let mut screen = SessionScreen::new(
            "run-done",
            "done",
            || Ok(Vec::new()),
            || Ok(terminal_status(Status::Done)),
        )
        .with_enqueue(store_enqueue(Arc::clone(&store), "run-done"));
        screen.refresh_now();
        assert_eq!(screen.steering_help_text(), None);
        assert_eq!(screen.submit_say("late"), None);
        assert_eq!(screen.submit_interrupt(), None);
        assert_eq!(screen.submit_approve(), None);
        assert_eq!(screen.submit_reject(Some("late feedback")), None);
        assert!(
            store
                .claim_commands("run-done", Utc::now())
                .unwrap()
                .is_empty()
        );
        assert!(screen.notice().unwrap().contains("not steerable"));
        let mut term = terminal(100, 10);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        assert!(buffer_text(&term).contains("not steerable"));
    }

    #[test]
    fn status_left_active_between_render_and_submit() {
        let (store, run_id) = running_store("alpha");
        let (statuses, status) = scripted_status(running());
        let mut screen = SessionScreen::new(&run_id, "alpha", || Ok(Vec::new()), status)
            .with_enqueue(store_enqueue(Arc::clone(&store), &run_id));
        screen.refresh_now();
        statuses
            .borrow_mut()
            .push_back(terminal_status(Status::Done));
        screen.refresh_now();
        assert_eq!(screen.submit_say("too late"), None);
        assert!(
            store
                .claim_commands(&run_id, Utc::now())
                .unwrap()
                .is_empty()
        );
        assert!(screen.notice().is_some());
    }

    #[test]
    fn enqueue_failure_surfaces_notice() {
        let mut screen = SessionScreen::new("run-x", "t", || Ok(Vec::new()), || Ok(running()))
            .with_enqueue(|_, _| Err("disk full".to_owned()));
        screen.refresh_now();
        assert_eq!(screen.submit_say("x"), None);
        assert_eq!(screen.notice(), Some("enqueue failed: disk full"));
        // Without an enqueue seam every verb is a silent no-op.
        let mut bare = SessionScreen::new("run-x", "t", || Ok(Vec::new()), || Ok(running()));
        bare.refresh_now();
        assert_eq!(bare.submit_say("x"), None);
        assert_eq!(bare.notice(), None);
    }

    #[test]
    fn compose_submit_routes_say_help_status_unknown_and_empty() {
        let (store, run_id) = running_store("alpha");
        let mut screen = SessionScreen::new(&run_id, "alpha", || Ok(Vec::new()), || Ok(running()))
            .with_enqueue(store_enqueue(Arc::clone(&store), &run_id));
        screen.refresh_now();
        for c in "hi there".chars() {
            screen.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(screen.compose().value(), "hi there");
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), SessionAction::None);
        assert_eq!(screen.compose().value(), "");
        let claimed = store.claim_commands(&run_id, Utc::now()).unwrap();
        assert_eq!(claimed[0].payload["text"], "hi there");
        screen.compose_mut().set_value("   ");
        screen.handle_key(key(KeyCode::Enter));
        assert_eq!(screen.notice(), Some("say message must be non-empty"));
        screen.compose_mut().set_value("/help");
        screen.handle_key(key(KeyCode::Enter));
        let notice = screen.notice().unwrap();
        assert!(notice.contains("/help"));
        assert!(notice.contains("/quit"));
        assert_eq!(screen.compose().value(), "");
        screen.compose_mut().set_value("/status");
        screen.handle_key(key(KeyCode::Enter));
        assert_eq!(
            screen.notice(),
            Some(format!("run {run_id}: status=running").as_str())
        );
        screen.compose_mut().set_value("/nope");
        screen.handle_key(key(KeyCode::Enter));
        assert_eq!(
            screen.notice(),
            Some("unknown slash command /nope; try /help")
        );
        assert_eq!(screen.compose().value(), "/nope");
        screen.compose_mut().set_value("/ x");
        screen.handle_key(key(KeyCode::Enter));
        assert_eq!(screen.notice(), Some("unknown slash command /; try /help"));
        screen.compose_mut().set_value("/ ");
        screen.handle_key(key(KeyCode::Enter));
        assert_eq!(screen.notice(), Some("unknown slash command /; try /help"));
        assert_eq!(screen.compose().value(), "/ ");
        assert!(
            store
                .claim_commands(&run_id, Utc::now())
                .unwrap()
                .is_empty()
        );
        let mut term = terminal(100, 20);
        term.draw(|f| screen.render(f, f.area())).unwrap();
        assert!(buffer_text(&term).contains("unknown slash command /; try /help"));
    }

    #[test]
    fn routed_slash_verbs_return_actions() {
        let mut screen = SessionScreen::new("run-x", "t", || Ok(Vec::new()), || Ok(running()));
        screen.refresh_now();
        screen.compose_mut().set_value("/worker start");
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            SessionAction::WorkerSlash("start".to_owned())
        );
        assert_eq!(screen.compose().value(), "");
        screen.compose_mut().set_value("/autopilot stop");
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            SessionAction::AutopilotSlash("stop".to_owned())
        );
        screen.compose_mut().set_value("/quit");
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            SessionAction::RequestQuit
        );
        screen.compose_mut().set_value("/exit");
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            SessionAction::ExitAll
        );
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), SessionAction::Back);
        assert_eq!(screen.handle_key(ctrl('c')), SessionAction::RequestQuit);
        screen.set_notice("/worker start: supervised (pid=1)");
        assert_eq!(screen.notice(), Some("/worker start: supervised (pid=1)"));
    }

    #[test]
    fn archive_slash_reports_outcome() {
        let mut screen = SessionScreen::new("run-x", "t", || Ok(Vec::new()), || Ok(running()));
        screen.compose_mut().set_value("/archive");
        screen.handle_key(key(KeyCode::Enter));
        assert_eq!(
            screen.notice(),
            Some("/archive is not wired on this screen (no archive seam)")
        );
        let mut screen = SessionScreen::new("run-x", "t", || Ok(Vec::new()), || Ok(running()))
            .with_archive(|| Ok(vec!["archive/10-a".to_owned(), "archive/11-b".to_owned()]));
        screen.compose_mut().set_value("/archive");
        screen.handle_key(key(KeyCode::Enter));
        assert_eq!(
            screen.notice(),
            Some("/archive: moved archive/10-a, archive/11-b")
        );
        let mut screen = SessionScreen::new("run-x", "t", || Ok(Vec::new()), || Ok(running()))
            .with_archive(|| Ok(Vec::new()));
        screen.submit_compose("/archive");
        assert_eq!(screen.notice(), Some("/archive: no phases archived"));
        let mut screen = SessionScreen::new("run-x", "t", || Ok(Vec::new()), || Ok(running()))
            .with_archive(|| Err("gate failed".to_owned()));
        screen.submit_compose("/archive");
        assert_eq!(screen.notice(), Some("/archive failed: gate failed"));
    }

    #[test]
    fn wrap_line_splits_at_width_preserving_styles() {
        let line = Line::from(vec![
            Span::styled("abcdef", Style::default().fg(Color::Red)),
            Span::raw("ghij"),
        ]);
        let wrapped = wrap_line(&line, 4);
        assert_eq!(wrapped.len(), 3);
        assert_eq!(
            text_to_plain(&Text::from(wrapped.clone())),
            "abcd\nefgh\nij"
        );
        assert_eq!(wrapped[1].spans[0].style.fg, Some(Color::Red));
        assert_eq!(wrap_line(&line, 0).len(), 1);
        assert_eq!(wrap_line(&Line::default(), 4).len(), 1);
    }
}
