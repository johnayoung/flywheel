//! Operator-console entry point for the `fw` / `flywheel` shell.
//!
//! Bare `fw` opens the dashboard against the store the orchestrator's
//! policy resolves to; `fw --json` (or any invocation whose stdout is not a
//! TTY) prints one machine-readable [`DashboardSnapshot`] and exits 0 with
//! no ANSI sequences -- the interactive-default + print-mode pattern.
//!
//! Store path resolution mirrors `fw status` / `fw live` (explicit `--db`
//! -> policy `[paths] db` -> `.flywheel/flywheel.sqlite`); the work source
//! (used by both the summary header's task-state counts and the `/archive`
//! slash command) mirrors the same explicit `--tasks-dir` -> policy ->
//! default chain.
//!
//! This module also owns two seams the screens share:
//!
//! * [`DaemonControl`] -- the console's handle on the supervised `worker`
//!   and `autopilot` daemons. The supervisors (process spawning, crash-loop
//!   budgets, liveness probes) live in `worker_supervisor` /
//!   `autopilot_supervisor`; the console only ever calls this trait, so the
//!   screens are testable with a scripted implementation and the real
//!   supervisors are wired in by the caller through [`main_with`].
//! * [`InputBar`] -- the persistent one-line input every screen carries
//!   (plain text filters / says, a leading `/` is a slash command).

use crate::dashboard::{DEFAULT_POLL_INTERVAL_SECONDS, DashboardApp};
use crate::history_screen::HistoryScreen;
use crate::session::TranscriptTailer;
use crate::session_screen::{SessionScreen, SessionStatus};
use crate::snapshot::{
    CachedWorkSource, DashboardSnapshot, build_snapshot, render_snapshot_json,
    resolve_awaiting_instruction,
};
use chrono::Utc;
use clap::Parser;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use flywheel_core::audit::run_file_path;
use flywheel_core::lifecycle::Status;
use flywheel_core::store::{HarnessStore, JsonObject};
use flywheel_core::store_sqlite::SqliteStore;
use flywheel_orchestrator::autopilot_activity::AutopilotActivity;
use flywheel_orchestrator::claims_postgres::register_postgres_backends;
use flywheel_orchestrator::github::register_github_sources;
use flywheel_orchestrator::history::{HistoryQuery, build_task_phase_index, collect_history_rows};
use flywheel_orchestrator::policy::{DEFAULT_TASKS_DIR, WorkPolicy};
use flywheel_orchestrator::source_registry::{SourceBuildContext, default_registry};
use flywheel_orchestrator::sources::{DirectoryWorkSource, WorkSource};
use flywheel_orchestrator::store_factory::{
    PG_DSN_ENV, PG_DSN_FALLBACK_ENV, StoreBundle, open_stores, resolve_postgres_dsn,
};
use flywheel_orchestrator::store_registry::{
    Environ, SqliteCoreOpener, StoreConfig, StoreRegistries,
};
use flywheel_orchestrator::triage_run::load_effective_policy;
use flywheel_worktree::phase::{ArchiveOptions, archive_completed_phases};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The default store path when neither `--db` nor the policy names one.
pub const DEFAULT_STORE_PATH: &str = ".flywheel/flywheel.sqlite";

// --- Daemon control ---------------------------------------------------------------

/// The supervised worker daemon's name.
pub const DAEMON_WORKER: &str = "worker";
/// The supervised autopilot daemon's name.
pub const DAEMON_AUTOPILOT: &str = "autopilot";

/// The states the console's status bar renders for a supervised daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DaemonState {
    /// This console owns a child it spawned and the child is alive.
    Supervised,
    /// A live daemon exists (lease / liveness record) that this console did
    /// not spawn; it is never signaled from here.
    Detached,
    /// No live daemon and no supervised child.
    None,
    /// The supervised child terminated unexpectedly and was not respawned.
    Dead,
    /// The child kept dying and exhausted the crash-loop budget; the
    /// supervisor stopped respawning and latched this terminal state.
    DeadAfterBudget,
    /// The most recent `start` failed before the child could launch.
    Error,
}

impl DaemonState {
    pub const fn as_str(self) -> &'static str {
        match self {
            DaemonState::Supervised => "supervised",
            DaemonState::Detached => "detached",
            DaemonState::None => "none",
            DaemonState::Dead => "dead",
            DaemonState::DeadAfterBudget => "dead_after_budget",
            DaemonState::Error => "error",
        }
    }
}

/// Snapshot of one daemon's supervision state for the status bar.
///
/// `pid` is set for `Supervised` (the live child), `Detached` when the
/// adopted daemon's pid is known, and the two dead states (the last dead
/// child's). `message` carries human-readable detail for the dead states
/// (exit code + captured reason -- the death reason) and for `Error` (the
/// spawn failure). `activity` is the autopilot's live per-cycle record when
/// it has one; always `None` for the worker.
#[derive(Debug, Clone, PartialEq)]
pub struct DaemonStatus {
    pub state: DaemonState,
    pub pid: Option<u32>,
    pub message: Option<String>,
    pub activity: Option<AutopilotActivity>,
}

impl DaemonStatus {
    pub fn new(state: DaemonState) -> Self {
        Self {
            state,
            pid: None,
            message: None,
            activity: None,
        }
    }

    pub fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn with_activity(mut self, activity: AutopilotActivity) -> Self {
        self.activity = Some(activity);
        self
    }
}

/// Why a daemon-control call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DaemonError {
    /// The name is not one of the daemons this control manages.
    #[error("unknown daemon {0:?}")]
    Unknown(String),
    /// The supervisor itself failed (spawn error, probe error, ...).
    #[error("{0}")]
    Supervisor(String),
}

/// The console's handle on its supervised daemons.
///
/// Implemented by the worker / autopilot supervisors (one implementation
/// fronting both, keyed by [`DAEMON_WORKER`] / [`DAEMON_AUTOPILOT`]); the
/// console calls nothing else. Contract:
///
/// * `status(name)` is a read-only probe, safe to call every poll tick;
///   it must reflect a dying child within one tick (state `Dead` /
///   `DeadAfterBudget` with the death reason in `message`).
/// * `start(name)` (re)spawns a child when none is live and returns the
///   resulting status: `Supervised` on success, `Detached` when a live
///   foreign daemon already exists (no spawn), `Error` with `message` when
///   the spawn failed. Idempotent on an already-supervised child.
/// * `stop(name)` signals ONLY a child this control owns (SIGTERM + wait);
///   `Ok(true)` when it exited inside the wait window, `Ok(false)` when it
///   did not or when nothing was supervised. A detached / foreign daemon
///   is never signaled.
/// * `detach(name)` forgets a supervised child without signaling it
///   (console exit, quit-prompt `enter`); idempotent.
/// * `death_reason(name)` is the last recorded death detail (exit code +
///   captured stderr tail) regardless of the current state, so an operator
///   can still read why the previous child died after a respawn.
///
/// An unknown `name` fails with [`DaemonError::Unknown`].
pub trait DaemonControl {
    fn status(&self, name: &str) -> Result<DaemonStatus, DaemonError>;
    fn start(&mut self, name: &str) -> Result<DaemonStatus, DaemonError>;
    fn stop(&mut self, name: &str) -> Result<bool, DaemonError>;
    fn detach(&mut self, name: &str);
    fn death_reason(&self, name: &str) -> Option<String>;
}

/// The "no supervision wired" control: every daemon reads as `None`,
/// `start` reports a spawn error naming the gap, `stop` / `detach` are
/// no-ops. Used when the console is launched without supervisors (library
/// callers, `--json` mode, headless tests).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoDaemonControl;

impl DaemonControl for NoDaemonControl {
    fn status(&self, name: &str) -> Result<DaemonStatus, DaemonError> {
        check_daemon_name(name)?;
        Ok(DaemonStatus::new(DaemonState::None))
    }

    fn start(&mut self, name: &str) -> Result<DaemonStatus, DaemonError> {
        check_daemon_name(name)?;
        Ok(DaemonStatus::new(DaemonState::Error)
            .with_message(format!("no {name} supervisor is wired into this console")))
    }

    fn stop(&mut self, name: &str) -> Result<bool, DaemonError> {
        check_daemon_name(name)?;
        Ok(false)
    }

    fn detach(&mut self, _name: &str) {}

    fn death_reason(&self, _name: &str) -> Option<String> {
        None
    }
}

/// Reject names outside the two daemons the console manages.
pub fn check_daemon_name(name: &str) -> Result<(), DaemonError> {
    if name == DAEMON_WORKER || name == DAEMON_AUTOPILOT {
        Ok(())
    } else {
        Err(DaemonError::Unknown(name.to_owned()))
    }
}

// --- Input bar -------------------------------------------------------------------

/// What one key press did to an [`InputBar`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// The value changed (live filter / edit).
    Changed,
    /// Enter: the current value, left in place for the caller to clear.
    Submitted(String),
    /// Escape: the caller should move focus away from the bar.
    Blur,
    /// Not an editing key; the caller may route it elsewhere.
    Ignored,
}

/// A one-line text input with a cursor, a placeholder, and a focus flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputBar {
    value: String,
    cursor: usize,
    focused: bool,
    placeholder: String,
}

impl InputBar {
    pub fn new(placeholder: impl Into<String>) -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            focused: false,
            placeholder: placeholder.into(),
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    /// Replace the value and move the cursor to its end.
    pub fn set_value(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.value.chars().count();
    }

    pub fn clear(&mut self) {
        self.set_value("");
    }

    pub fn focused(&self) -> bool {
        self.focused
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn placeholder(&self) -> &str {
        &self.placeholder
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_index)
            .map_or(self.value.len(), |(i, _)| i)
    }

    /// Apply an editing key. Printable characters insert at the cursor;
    /// `backspace` / `delete` / arrows / `home` / `end` (and `ctrl+a` /
    /// `ctrl+e`) edit and move; `enter` submits; `esc` blurs.
    pub fn handle_key(&mut self, key: KeyEvent) -> InputEvent {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => InputEvent::Submitted(self.value.clone()),
            KeyCode::Esc => InputEvent::Blur,
            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                InputEvent::Changed
            }
            KeyCode::Char('e') if ctrl => {
                self.cursor = self.value.chars().count();
                InputEvent::Changed
            }
            KeyCode::Char('u') if ctrl => {
                let tail: String = self.value.chars().skip(self.cursor).collect();
                self.value = tail;
                self.cursor = 0;
                InputEvent::Changed
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                let at = self.byte_index(self.cursor);
                self.value.insert(at, c);
                self.cursor += 1;
                InputEvent::Changed
            }
            KeyCode::Backspace => {
                if self.cursor == 0 {
                    return InputEvent::Ignored;
                }
                let start = self.byte_index(self.cursor - 1);
                let end = self.byte_index(self.cursor);
                self.value.replace_range(start..end, "");
                self.cursor -= 1;
                InputEvent::Changed
            }
            KeyCode::Delete => {
                if self.cursor >= self.value.chars().count() {
                    return InputEvent::Ignored;
                }
                let start = self.byte_index(self.cursor);
                let end = self.byte_index(self.cursor + 1);
                self.value.replace_range(start..end, "");
                InputEvent::Changed
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                InputEvent::Changed
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.value.chars().count());
                InputEvent::Changed
            }
            KeyCode::Home => {
                self.cursor = 0;
                InputEvent::Changed
            }
            KeyCode::End => {
                self.cursor = self.value.chars().count();
                InputEvent::Changed
            }
            _ => InputEvent::Ignored,
        }
    }

    /// Draw the bar on one row: a `> ` prompt, then the value (or the dim
    /// placeholder when empty); a focused bar shows a reversed cursor cell
    /// and scrolls so the cursor stays visible.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let prompt_style = if self.focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut spans = vec![Span::styled("> ", prompt_style)];
        let width = usize::from(area.width).saturating_sub(2);
        if self.value.is_empty() {
            let placeholder: String = self.placeholder.chars().take(width).collect();
            if self.focused {
                let head = placeholder
                    .chars()
                    .next()
                    .map_or(" ".to_owned(), |c| c.to_string());
                spans.push(Span::styled(
                    head,
                    Style::default().add_modifier(Modifier::REVERSED | Modifier::DIM),
                ));
                spans.push(Span::styled(
                    placeholder.chars().skip(1).collect::<String>(),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            } else {
                spans.push(Span::styled(
                    placeholder,
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
        } else {
            let chars: Vec<char> = self.value.chars().collect();
            let start = if width == 0 {
                0
            } else {
                self.cursor.saturating_sub(width.saturating_sub(1))
            };
            let visible: Vec<char> = chars.iter().skip(start).take(width).copied().collect();
            let cursor_at = self.cursor.saturating_sub(start);
            if self.focused {
                let before: String = visible.iter().take(cursor_at).collect();
                let at: String = visible
                    .get(cursor_at)
                    .map_or(" ".to_owned(), |c| c.to_string());
                let after: String = visible.iter().skip(cursor_at + 1).collect();
                spans.push(Span::raw(before));
                spans.push(Span::styled(
                    at,
                    Style::default().add_modifier(Modifier::REVERSED),
                ));
                spans.push(Span::raw(after));
            } else {
                spans.push(Span::raw(visible.into_iter().collect::<String>()));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

// --- Console flags ------------------------------------------------------------------

/// The console's command-line flags (the Python `fw` argparse surface).
#[derive(Debug, Clone, PartialEq, Parser)]
#[command(
    name = "fw",
    disable_help_subcommand = true,
    // argparse accepts any unambiguous long-option prefix (`--no-wor`).
    infer_long_args = true,
    about = "Interactive realtime dashboard of in-flight flywheel runs. Bare invocation opens \
             the console; --json (or a non-TTY stdout) prints one snapshot and exits."
)]
pub struct TuiArgs {
    /// Store path. Defaults to the policy's [paths] db (else
    /// .flywheel/flywheel.sqlite). Matches fw status / fw live.
    #[arg(long)]
    pub db: Option<String>,
    /// Work-policy file selecting the work source (default: flywheel.toml
    /// if present).
    #[arg(long)]
    pub policy: Option<String>,
    /// Tasks root directory for the summary's task-state counts and
    /// /archive (default: policy source, else .flywheel/tasks).
    #[arg(long = "tasks-dir")]
    pub tasks_dir: Option<String>,
    /// Print one JSON snapshot of the dashboard data and exit. Implied
    /// when stdout is not a TTY.
    #[arg(long)]
    pub json: bool,
    /// Seconds between dashboard polls.
    #[arg(long = "poll-interval", default_value_t = DEFAULT_POLL_INTERVAL_SECONDS)]
    pub poll_interval: f64,
    /// Agent model id forwarded to the spawned worker as --model. Overrides
    /// the [agent] model setting in flywheel.toml; absent both, the SDK
    /// uses the Claude Code default.
    #[arg(long)]
    pub model: Option<String>,
    /// Skip spawning a supervised worker on console launch. The status bar
    /// shows worker: none until you type '/worker start' or run 'flywheel
    /// worker' in another terminal.
    #[arg(long = "no-worker")]
    pub no_worker: bool,
}

impl Default for TuiArgs {
    fn default() -> Self {
        Self {
            db: None,
            policy: None,
            tasks_dir: None,
            json: false,
            poll_interval: DEFAULT_POLL_INTERVAL_SECONDS,
            model: None,
            no_worker: false,
        }
    }
}

/// Parse the console flags from `argv` (without the program name).
pub fn parse_args(argv: &[String]) -> Result<TuiArgs, clap::Error> {
    TuiArgs::try_parse_from(std::iter::once("fw").chain(argv.iter().map(String::as_str)))
}

// --- Resolution helpers ----------------------------------------------------------

/// The store path with `fw status`'s precedence: explicit `--db`, else the
/// policy's `[paths] db`, else [`DEFAULT_STORE_PATH`].
pub fn resolve_db_path(db_arg: Option<&str>, policy: Option<&WorkPolicy>) -> PathBuf {
    if let Some(db) = db_arg.filter(|d| !d.is_empty()) {
        return PathBuf::from(db);
    }
    if let Some(db) = policy.and_then(|p| p.db_path.clone()) {
        return db;
    }
    PathBuf::from(DEFAULT_STORE_PATH)
}

/// The agent model id the spawned worker should run with: an explicit
/// `--model` wins, else the policy's `[agent] model`, else `None` so the
/// worker omits `--model` and the SDK falls through to its default.
pub fn resolve_model_for_worker(
    model_arg: Option<&str>,
    policy: Option<&WorkPolicy>,
) -> Option<String> {
    if let Some(model) = model_arg.filter(|m| !m.is_empty()) {
        return Some(model.to_owned());
    }
    policy.and_then(|p| p.model.clone())
}

/// The `--tasks-dir` the spawned worker should use: the explicit flag, else
/// the policy's directory-source `tasks_dir`, else `None` (the worker's own
/// default).
pub fn resolve_tasks_dir_for_worker(
    tasks_dir_arg: Option<&str>,
    policy: Option<&WorkPolicy>,
) -> Option<PathBuf> {
    if let Some(dir) = tasks_dir_arg.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    policy.and_then(|p| {
        (p.source_kind == "directory")
            .then(|| p.tasks_dir.clone())
            .flatten()
    })
}

/// The directory `/archive` should sweep, or `None` for tracker sources
/// (the slash command degrades to an inline notice).
pub fn resolve_archive_tasks_dir(
    tasks_dir_arg: Option<&str>,
    policy: Option<&WorkPolicy>,
) -> Option<PathBuf> {
    if let Some(dir) = tasks_dir_arg.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    match policy {
        None => Some(PathBuf::from(DEFAULT_TASKS_DIR)),
        Some(p) if p.source_kind == "directory" => p.tasks_dir.clone(),
        Some(_) => None,
    }
}

/// The repo root a tasks dir belongs to: the parent of the enclosing
/// `.flywheel` directory, else the current working directory.
pub fn repo_root_for_tasks_dir(tasks_dir: &Path) -> PathBuf {
    let resolved = tasks_dir
        .canonicalize()
        .unwrap_or_else(|_| tasks_dir.to_path_buf());
    for parent in resolved.ancestors().skip(1) {
        if parent.file_name().is_some_and(|name| name == ".flywheel") {
            return parent
                .parent()
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The `[source]` table a policy's source fields re-materialize to, so the
/// registry can build the adapter the policy selects.
pub fn source_settings_for_policy(policy: &WorkPolicy) -> toml::Table {
    let mut settings = toml::Table::new();
    let set = |settings: &mut toml::Table, key: &str, value: Option<String>| {
        if let Some(value) = value {
            settings.insert(key.to_owned(), toml::Value::String(value));
        }
    };
    settings.insert(
        "kind".to_owned(),
        toml::Value::String(policy.source_kind.clone()),
    );
    match policy.source_kind.as_str() {
        "github" => {
            set(&mut settings, "repo", policy.github_repo.clone());
            set(&mut settings, "label", policy.github_label.clone());
            set(
                &mut settings,
                "done_action",
                Some(policy.github_done_action.clone()),
            );
            settings.insert(
                "require_triage_receipt".to_owned(),
                toml::Value::Boolean(policy.github_require_triage_receipt),
            );
        }
        "github_ci" => {
            set(&mut settings, "repo", policy.github_ci_repo.clone());
            set(
                &mut settings,
                "failure_filter",
                Some(policy.github_ci_failure_filter.clone()),
            );
        }
        "github_review" => {
            set(&mut settings, "repo", policy.github_review_repo.clone());
        }
        _ => {
            set(
                &mut settings,
                "tasks_dir",
                policy
                    .tasks_dir
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
            );
        }
    }
    settings
}

/// Mirror `fw status`'s work-source precedence: an explicit `--tasks-dir`
/// selects the directory source, else the policy decides, else the default
/// `.flywheel/tasks` directory. Directory sources are wrapped in a
/// fingerprint cache so the per-tick listing cost is one metadata walk.
pub fn resolve_work_source(
    tasks_dir_arg: Option<&str>,
    policy: Option<&WorkPolicy>,
) -> Result<Box<dyn WorkSource>, String> {
    let cached_directory = |dir: PathBuf| -> Box<dyn WorkSource> {
        Box::new(CachedWorkSource::for_directory(
            Box::new(DirectoryWorkSource::new(&dir)),
            dir,
        ))
    };
    if let Some(dir) = tasks_dir_arg.filter(|d| !d.is_empty()) {
        return Ok(cached_directory(PathBuf::from(dir)));
    }
    let Some(policy) = policy else {
        return Ok(cached_directory(PathBuf::from(DEFAULT_TASKS_DIR)));
    };
    if policy.source_kind == "directory" {
        let dir = policy
            .tasks_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_TASKS_DIR));
        return Ok(cached_directory(dir));
    }
    let context = SourceBuildContext::new(source_settings_for_policy(policy))
        .with_default_graders(policy.default_graders.clone());
    let mut registry = default_registry();
    register_github_sources(&mut registry);
    registry
        .build_from_context(&context)
        .map_err(|err| err.to_string())
}

/// The process environment as the store factory's `Environ`.
pub fn process_environ() -> Environ {
    std::env::vars().collect()
}

/// The shipped store registries: sqlite (core + claims) plus postgres.
pub fn store_registries() -> StoreRegistries {
    let opener: SqliteCoreOpener = Arc::new(|path: &Path| {
        SqliteStore::open(path).map(|store| Arc::new(store) as Arc<dyn HarnessStore>)
    });
    let mut registries = StoreRegistries::with_sqlite(opener);
    // A fresh registry cannot already hold the postgres name.
    let _ = register_postgres_backends(&mut registries);
    registries
}

/// Open the core + claim stores the policy selects at `db_path`.
pub fn open_store_bundle(
    policy: Option<&WorkPolicy>,
    db_path: &Path,
) -> Result<StoreBundle, String> {
    let config = StoreConfig {
        backend: policy.map_or_else(|| "sqlite".to_owned(), |p| p.store_backend.clone()),
        db_path: db_path.to_path_buf(),
        schema: policy.and_then(|p| p.store_schema.clone()),
    };
    open_stores(&store_registries(), &config, &process_environ()).map_err(|err| err.to_string())
}

/// The missing-sqlite-store message (exit 2) with the `fw init` remedy.
pub fn missing_store_message(db_path: &Path) -> String {
    format!(
        "fw: no store found at {}\n  run 'fw init' to scaffold .flywheel/ and the work policy, \
         then start the orchestrator to populate the store.",
        db_path.display()
    )
}

/// The postgres analogue: no DSN env var set.
pub fn missing_dsn_message() -> String {
    format!(
        "fw: store backend is postgres but neither {PG_DSN_ENV} nor {PG_DSN_FALLBACK_ENV} is set\n  \
         export one with a postgres connection string, then rerun fw."
    )
}

// --- Entry points ----------------------------------------------------------------

/// What the console resolved for the daemons it supervises, handed to the
/// caller's [`DaemonFactory`] so the supervisors spawn the same worker the
/// console is watching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonSpawnSpec {
    pub db_path: PathBuf,
    pub tasks_dir: Option<PathBuf>,
    pub model: Option<String>,
    /// `false` under `--no-worker`: construct the supervisors but do not
    /// auto-spawn the worker. Autopilot is never auto-spawned.
    pub auto_start_worker: bool,
}

/// Builds the daemon control for a resolved spawn spec.
pub type DaemonFactory = Box<dyn FnOnce(&DaemonSpawnSpec) -> Box<dyn DaemonControl>>;

/// The console with no supervisors wired (status bar shows `none`).
pub fn main(argv: &[String]) -> i32 {
    main_with(argv, None)
}

/// Resolve the store, then dispatch to the console or the JSON snapshot.
///
/// Returns 0 on a clean exit, 2 when the resolved store is missing, the
/// policy is invalid, or the store cannot be opened. The missing-store
/// check is backend-aware: sqlite checks file existence at the resolved
/// path (with the `fw init` remedy); postgres checks DSN env presence.
/// Snapshot mode auto-engages when stdout is not a TTY.
pub fn main_with(argv: &[String], daemons: Option<DaemonFactory>) -> i32 {
    let args = match parse_args(argv) {
        Ok(args) => args,
        Err(err) => {
            let _ = err.print();
            return if err.use_stderr() { 2 } else { 0 };
        }
    };
    let policy = match load_effective_policy(args.policy.as_deref().map(Path::new)) {
        Ok(policy) => policy,
        Err(err) => {
            eprintln!("fw: policy error: {err}");
            return 2;
        }
    };
    let db_path = resolve_db_path(args.db.as_deref(), policy.as_ref());
    if policy
        .as_ref()
        .is_some_and(|p| p.store_backend == "postgres")
    {
        if resolve_postgres_dsn(&process_environ()).is_none() {
            eprintln!("{}", missing_dsn_message());
            return 2;
        }
    } else if !db_path.exists() {
        eprintln!("{}", missing_store_message(&db_path));
        return 2;
    }
    let work_source = match resolve_work_source(args.tasks_dir.as_deref(), policy.as_ref()) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("fw: {err}");
            return 2;
        }
    };
    let bundle = match open_store_bundle(policy.as_ref(), &db_path) {
        Ok(bundle) => bundle,
        Err(err) => {
            eprintln!("fw: {err}");
            return 2;
        }
    };
    let json_mode = args.json || !std::io::stdout().is_terminal();
    if json_mode {
        return run_snapshot(bundle.core.as_ref(), work_source.as_ref());
    }
    let spec = DaemonSpawnSpec {
        db_path: db_path.clone(),
        tasks_dir: resolve_tasks_dir_for_worker(args.tasks_dir.as_deref(), policy.as_ref()),
        model: resolve_model_for_worker(args.model.as_deref(), policy.as_ref()),
        auto_start_worker: !args.no_worker,
    };
    let mut control: Box<dyn DaemonControl> = match daemons {
        Some(factory) => factory(&spec),
        None => Box::new(NoDaemonControl),
    };
    if spec.auto_start_worker {
        // The spawn outcome lands on the status bar through `status`; a
        // failure here must not abort the console.
        let _ = control.start(DAEMON_WORKER);
    }
    let archive_dir = resolve_archive_tasks_dir(args.tasks_dir.as_deref(), policy.as_ref());
    let mut app = build_dashboard(
        bundle,
        Arc::from(work_source),
        archive_dir,
        db_path,
        policy.as_ref(),
        control,
        Duration::from_secs_f64(args.poll_interval.max(0.05)),
    );
    let rc = run_terminal(&mut app);
    // Console exit detaches by default: the supervised children keep
    // running so a SIGINT on the console never silently kills them. The
    // operator's explicit stop choice has already signaled them by now.
    app.detach_daemons();
    rc
}

/// Build one snapshot, print it as JSON, exit 0 -- no ANSI ever touches
/// stdout in this mode.
pub fn run_snapshot(store: &dyn HarnessStore, work_source: &dyn WorkSource) -> i32 {
    let now = Utc::now();
    match build_snapshot(store, Some(work_source), now, now) {
        Ok(snapshot) => {
            println!("{}", render_snapshot_json(&snapshot));
            0
        }
        Err(err) => {
            eprintln!("fw: {err}");
            2
        }
    }
}

/// Wire the dashboard's seams over the opened stores.
///
/// Every closure the screens call is bound here: the poll, the per-run
/// session factory (a fresh [`TranscriptTailer`] per open so re-opening a
/// run rewinds to the top), the history factory (phase fallback index
/// rebuilt per open), the control-command enqueue, and the `/archive`
/// sweep with the same gates the worker / CLI sweeps apply.
pub fn build_dashboard(
    bundle: StoreBundle,
    work_source: Arc<dyn WorkSource>,
    archive_tasks_dir: Option<PathBuf>,
    db_path: PathBuf,
    policy: Option<&WorkPolicy>,
    daemons: Box<dyn DaemonControl>,
    poll_interval: Duration,
) -> DashboardApp {
    let started_at = Utc::now();
    let store = Arc::clone(&bundle.core);
    let poll_store = Arc::clone(&store);
    let poll_source = Arc::clone(&work_source);
    let poll = move || -> Result<DashboardSnapshot, String> {
        build_snapshot(
            poll_store.as_ref(),
            Some(poll_source.as_ref()),
            Utc::now(),
            started_at,
        )
        .map_err(|err| err.to_string())
    };

    let enqueue_store = Arc::clone(&store);
    let enqueue = move |run_id: &str, kind: &str, payload: JsonObject| -> Result<i64, String> {
        let record = enqueue_store
            .enqueue_command(run_id, kind, payload, Utc::now())
            .map_err(|err| err.to_string())?;
        record
            .id
            .ok_or_else(|| "store returned a command without an id".to_owned())
    };

    let landing_base = policy
        .and_then(|p| p.submit_base.clone())
        .unwrap_or_else(|| "HEAD".to_owned());
    let phase_verify = policy.and_then(|p| p.phase_verify.clone());
    let archive_bundle = bundle.clone();
    let archive_dir = archive_tasks_dir.clone();
    let archive = move || -> Result<Vec<String>, String> {
        let Some(dir) = &archive_dir else {
            return Ok(Vec::new());
        };
        let repo_root = repo_root_for_tasks_dir(dir);
        let options = ArchiveOptions {
            repo_root: Some(&repo_root),
            phase_verify: phase_verify.as_deref(),
            landing_base: Some(&landing_base),
            true_base: Some(&landing_base),
            claims: Some(archive_bundle.claims.as_ref()),
            ..ArchiveOptions::default()
        };
        archive_completed_phases(dir, &archive_bundle.core, &options)
            .map(|moved| moved.iter().map(|p| p.display().to_string()).collect())
            .map_err(|err| err.to_string())
    };
    let archive_for_sessions = Arc::new(Mutex::new(archive));
    let archive_for_dashboard = Arc::clone(&archive_for_sessions);

    let logs_root = db_path
        .parent()
        .map_or_else(PathBuf::new, Path::to_path_buf)
        .join("logs");
    let session_store = Arc::clone(&store);
    let open_session = move |run_id: &str, task_id: &str| -> Option<SessionScreen> {
        let mut tailer = TranscriptTailer::new(run_file_path(&logs_root, run_id), run_id);
        let fetch = move || tailer.fetch().map_err(|err| err.to_string());
        let status_store = Arc::clone(&session_store);
        let status_run = run_id.to_owned();
        // The gate instruction is resolved once per ordinal, not once per
        // 250ms tick: a parked run's task definition does not change.
        let mut instruction_cache: Option<(u32, Option<String>)> = None;
        let status = move || -> Result<SessionStatus, String> {
            let Some(lifecycle) = status_store
                .load_lifecycle(&status_run)
                .map_err(|err| err.to_string())?
            else {
                return Ok(SessionStatus::missing());
            };
            let instruction = match (lifecycle.status, lifecycle.awaiting_manual_ordinal) {
                (Status::AwaitingApproval, Some(ordinal)) => {
                    if let Some((cached, text)) = &instruction_cache
                        && *cached == ordinal
                    {
                        text.clone()
                    } else {
                        let text = resolve_awaiting_instruction(
                            status_store.as_ref(),
                            &status_run,
                            lifecycle.status,
                            Some(ordinal),
                        )
                        .map_err(|err| err.to_string())?;
                        instruction_cache = Some((ordinal, text.clone()));
                        text
                    }
                }
                _ => None,
            };
            Ok(SessionStatus::new(Some(lifecycle.status), instruction))
        };
        let enqueue_store = Arc::clone(&session_store);
        let enqueue_run = run_id.to_owned();
        let enqueue = move |kind: &str, payload: JsonObject| -> Result<i64, String> {
            let record = enqueue_store
                .enqueue_command(&enqueue_run, kind, payload, Utc::now())
                .map_err(|err| err.to_string())?;
            record
                .id
                .ok_or_else(|| "store returned a command without an id".to_owned())
        };
        let archive = Arc::clone(&archive_for_sessions);
        let archive_action = move || -> Result<Vec<String>, String> {
            let guard = archive
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (*guard)()
        };
        Some(
            SessionScreen::new(run_id, task_id, fetch, status)
                .with_enqueue(enqueue)
                .with_archive(archive_action),
        )
    };

    let history_store = Arc::clone(&store);
    let history_dir = archive_tasks_dir;
    let open_history = move || -> Option<HistoryScreen> {
        let fallback: BTreeMap<String, String> = history_dir
            .as_ref()
            .map(build_task_phase_index)
            .unwrap_or_default();
        let store = Arc::clone(&history_store);
        let fetch = move || {
            let query = HistoryQuery::default().with_fallback_phases(fallback.clone());
            collect_history_rows(store.as_ref(), &query).map_err(|err| err.to_string())
        };
        Some(HistoryScreen::new(fetch))
    };

    DashboardApp::new(poll)
        .with_poll_interval(poll_interval)
        .with_open_session(open_session)
        .with_open_history(open_history)
        .with_enqueue(enqueue)
        .with_archive(move || {
            let guard = archive_for_dashboard
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (*guard)()
        })
        .with_daemons(daemons)
}

/// Undo everything [`run_terminal`] set up: mouse capture, the alternate
/// screen, the hidden cursor, and raw mode. Idempotent and best-effort, so
/// both the drop guard and the panic hook may call it.
pub fn restore_terminal() {
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = crossterm::terminal::disable_raw_mode();
}

/// The panic hook in force while the console owns the terminal.
type PanicHook = Arc<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

/// Compose `restore` in front of `previous`: the terminal is handed back
/// (alternate screen left, raw mode and mouse capture off) *before* the
/// previous hook prints the panic, so the message lands on the operator's
/// normal screen instead of vanishing with the alternate one.
pub fn restoring_panic_hook(
    restore: Arc<dyn Fn() + Send + Sync + 'static>,
    previous: PanicHook,
) -> Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static> {
    Box::new(move |info| {
        restore();
        previous(info);
    })
}

/// Restores the terminal (and the process's prior panic hook) on drop, so
/// neither an error return nor a panic unwinding out of the loop leaves the
/// operator's shell in raw / alternate-screen / mouse-capture mode.
struct TerminalGuard {
    previous_hook: PanicHook,
}

impl TerminalGuard {
    /// Install the restoring panic hook in front of the current one.
    fn install() -> Self {
        let previous_hook: PanicHook = Arc::from(std::panic::take_hook());
        std::panic::set_hook(restoring_panic_hook(
            Arc::new(restore_terminal),
            Arc::clone(&previous_hook),
        ));
        Self { previous_hook }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
        // Hand the process its prior hook back (not while panicking: the
        // hook has already run and swapping it mid-unwind would itself
        // panic).
        if !std::thread::panicking() {
            let previous = Arc::clone(&self.previous_hook);
            let _ = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

/// Drive `app` against the real terminal until it requests exit.
///
/// Key events route through [`DashboardApp::handle_key`]; the dashboard
/// poll and the per-screen refresh fire from [`DashboardApp::on_timer`]
/// between events, with the event wait bounded by
/// [`DashboardApp::due_in`] so the loop never spins.
pub fn run_terminal(app: &mut DashboardApp) -> i32 {
    if crossterm::terminal::enable_raw_mode().is_err() {
        eprintln!("fw: cannot enable raw mode on this terminal");
        return 2;
    }
    let _guard = TerminalGuard::install();
    if crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        crossterm::cursor::Hide
    )
    .is_err()
    {
        eprintln!("fw: cannot enter the alternate screen");
        return 2;
    }
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = match ratatui::Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(err) => {
            eprintln!("fw: cannot initialise the terminal: {err}");
            return 2;
        }
    };
    app.refresh_now();
    loop {
        if let Some(rc) = app.exit_code() {
            return rc;
        }
        if terminal.draw(|frame| app.render(frame)).is_err() {
            return 2;
        }
        let wait = app.due_in(Instant::now());
        match crossterm::event::poll(wait) {
            Ok(true) => match crossterm::event::read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => app.handle_key(key),
                Ok(Event::Mouse(mouse)) => app.handle_mouse(mouse),
                Ok(_) => {}
                Err(_) => return 2,
            },
            Ok(false) => {}
            Err(_) => return 2,
        }
        app.on_timer(Instant::now());
    }
}

/// Headless rendering helpers shared by the screen tests.
#[cfg(test)]
pub(crate) mod test_support {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// The buffer as one string per row, trailing spaces trimmed.
    pub fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut out = String::new();
        for y in 0..area.height {
            let mut row = String::new();
            for x in 0..area.width {
                row.push_str(buffer[(x, y)].symbol());
            }
            out.push_str(row.trim_end());
            out.push('\n');
        }
        out
    }

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    pub fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(width, height)).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{buffer_text, key, terminal};
    use super::*;
    use crossterm::event::KeyCode;
    use flywheel_core::store::LifecycleStore;

    fn directory_policy(tasks_dir: &Path, model: Option<&str>) -> WorkPolicy {
        WorkPolicy {
            source_kind: "directory".to_owned(),
            tasks_dir: Some(tasks_dir.to_path_buf()),
            model: model.map(str::to_owned),
            ..WorkPolicy::default()
        }
    }

    #[test]
    fn parse_args_accepts_every_console_flag_with_defaults() {
        let args = parse_args(&[]).unwrap();
        assert_eq!(args, TuiArgs::default());
        assert!(!args.no_worker);
        assert_eq!(args.poll_interval, DEFAULT_POLL_INTERVAL_SECONDS);
        let argv: Vec<String> = [
            "--db",
            "x.sqlite",
            "--policy",
            "p.toml",
            "--tasks-dir",
            "t",
            "--json",
            "--poll-interval",
            "0.5",
            "--model",
            "m",
            "--no-worker",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let args = parse_args(&argv).unwrap();
        assert_eq!(args.db.as_deref(), Some("x.sqlite"));
        assert_eq!(args.policy.as_deref(), Some("p.toml"));
        assert_eq!(args.tasks_dir.as_deref(), Some("t"));
        assert!(args.json);
        assert_eq!(args.poll_interval, 0.5);
        assert_eq!(args.model.as_deref(), Some("m"));
        assert!(args.no_worker);
        assert!(parse_args(&["--bogus".to_owned()]).is_err());
    }

    #[test]
    fn parse_args_accepts_unambiguous_prefixes_like_argparse() {
        let argv =
            |items: &[&str]| -> Vec<String> { items.iter().map(|s| (*s).to_owned()).collect() };
        let args = parse_args(&argv(&["--no-wor", "--js", "--poll", "2", "--tasks", "t"])).unwrap();
        assert!(args.no_worker);
        assert!(args.json);
        assert_eq!(args.poll_interval, 2.0);
        assert_eq!(args.tasks_dir.as_deref(), Some("t"));
        assert_eq!(
            parse_args(&argv(&["--mod=m"])).unwrap().model.as_deref(),
            Some("m")
        );
        // `--p` could be --policy or --poll-interval: ambiguous, rejected.
        assert!(parse_args(&argv(&["--p", "x"])).is_err());
    }

    #[test]
    fn resolve_model_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let policy = directory_policy(dir.path(), Some("claude-sonnet-4-5"));
        assert_eq!(
            resolve_model_for_worker(Some("claude-opus-4-8"), Some(&policy)).as_deref(),
            Some("claude-opus-4-8")
        );
        assert_eq!(
            resolve_model_for_worker(None, Some(&policy)).as_deref(),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(resolve_model_for_worker(None, None), None);
        assert_eq!(
            resolve_model_for_worker(None, Some(&directory_policy(dir.path(), None))),
            None
        );
        assert_eq!(
            resolve_model_for_worker(Some("claude-haiku"), None).as_deref(),
            Some("claude-haiku")
        );
    }

    #[test]
    fn resolve_paths_follow_flag_policy_default_chain() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = dir.path().join("tasks");
        let mut policy = directory_policy(&tasks, None);
        policy.db_path = Some(dir.path().join("p.sqlite"));
        assert_eq!(
            resolve_db_path(Some("x.sqlite"), Some(&policy)),
            PathBuf::from("x.sqlite")
        );
        assert_eq!(
            resolve_db_path(None, Some(&policy)),
            dir.path().join("p.sqlite")
        );
        assert_eq!(
            resolve_db_path(Some(""), None),
            PathBuf::from(DEFAULT_STORE_PATH)
        );
        assert_eq!(
            resolve_tasks_dir_for_worker(Some("t"), None),
            Some(PathBuf::from("t"))
        );
        assert_eq!(
            resolve_tasks_dir_for_worker(None, Some(&policy)),
            Some(tasks.clone())
        );
        assert_eq!(resolve_tasks_dir_for_worker(None, None), None);
        assert_eq!(
            resolve_archive_tasks_dir(None, None),
            Some(PathBuf::from(DEFAULT_TASKS_DIR))
        );
        assert_eq!(resolve_archive_tasks_dir(None, Some(&policy)), Some(tasks));
        let tracker = WorkPolicy {
            source_kind: "github".to_owned(),
            ..WorkPolicy::default()
        };
        assert_eq!(resolve_archive_tasks_dir(None, Some(&tracker)), None);
        assert_eq!(resolve_tasks_dir_for_worker(None, Some(&tracker)), None);
    }

    #[test]
    fn repo_root_is_parent_of_dot_flywheel() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = dir.path().join(".flywheel/tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        assert_eq!(
            repo_root_for_tasks_dir(&tasks),
            dir.path().canonicalize().unwrap()
        );
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert_eq!(
            repo_root_for_tasks_dir(&elsewhere),
            std::env::current_dir().unwrap()
        );
    }

    #[test]
    fn source_settings_mirror_policy_fields() {
        let policy = WorkPolicy {
            source_kind: "github".to_owned(),
            github_repo: Some("o/r".to_owned()),
            github_label: Some("ready".to_owned()),
            ..WorkPolicy::default()
        };
        let settings = source_settings_for_policy(&policy);
        assert_eq!(settings["kind"].as_str(), Some("github"));
        assert_eq!(settings["repo"].as_str(), Some("o/r"));
        assert_eq!(settings["label"].as_str(), Some("ready"));
        assert_eq!(settings["done_action"].as_str(), Some("comment"));
        assert_eq!(settings["require_triage_receipt"].as_bool(), Some(false));
        let source = resolve_work_source(None, Some(&policy)).expect("github source builds");
        assert_eq!(source.source_kind(), "github_issue");
        let dir = tempfile::tempdir().unwrap();
        let directory =
            resolve_work_source(None, Some(&directory_policy(dir.path(), None))).unwrap();
        assert_eq!(directory.source_kind(), "directory");
        assert_eq!(
            resolve_work_source(None, None).unwrap().source_kind(),
            "directory"
        );
    }

    #[test]
    fn messages_name_remedies() {
        let msg = missing_store_message(Path::new("/tmp/absent.sqlite"));
        assert!(msg.starts_with("fw: no store found at /tmp/absent.sqlite"));
        assert!(msg.contains("fw init"));
        let dsn = missing_dsn_message();
        assert!(dsn.contains("FLYWHEEL_PG_DSN"));
        assert!(dsn.contains("DATABASE_URL"));
        assert!(!dsn.contains("no store found"));
    }

    #[test]
    fn main_missing_store_exits_2_and_json_mode_prints_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.sqlite");
        let argv = vec![
            "--db".to_owned(),
            missing.display().to_string(),
            "--json".to_owned(),
        ];
        assert_eq!(main(&argv), 2);
        let db = dir.path().join("db.sqlite");
        {
            let store = SqliteStore::open(&db).unwrap();
            let mut lc = flywheel_core::lifecycle::Lifecycle::new("alpha");
            lc.run_id = "run-alpha".to_owned();
            lc.transition_to(Status::Ready, "", None).unwrap();
            lc.transition_to(Status::Running, "", None).unwrap();
            store.create_lifecycle(&lc).unwrap();
        }
        let argv = vec![
            "--db".to_owned(),
            db.display().to_string(),
            "--json".to_owned(),
            "--tasks-dir".to_owned(),
            dir.path().join("tasks").display().to_string(),
        ];
        assert_eq!(main(&argv), 0);
    }

    #[test]
    fn main_postgres_backend_without_dsn_exits_2() {
        let dir = tempfile::tempdir().unwrap();
        let policy = dir.path().join("flywheel.toml");
        std::fs::write(
            &policy,
            "[source]\nkind = \"directory\"\n[store]\nbackend = \"postgres\"\n",
        )
        .unwrap();
        // The check reads the process environment; both DSN variables are
        // unset in the test environment unless the operator exported one.
        if std::env::var_os(PG_DSN_ENV).is_some() || std::env::var_os(PG_DSN_FALLBACK_ENV).is_some()
        {
            return;
        }
        let argv = vec![
            "--policy".to_owned(),
            policy.display().to_string(),
            "--json".to_owned(),
        ];
        assert_eq!(main(&argv), 2);
    }

    #[test]
    fn no_daemon_control_reads_none_and_refuses_unknown_names() {
        let mut control = NoDaemonControl;
        assert_eq!(
            control.status(DAEMON_WORKER).unwrap().state,
            DaemonState::None
        );
        assert_eq!(
            control.status(DAEMON_AUTOPILOT).unwrap().state,
            DaemonState::None
        );
        assert_eq!(
            control.status("cron"),
            Err(DaemonError::Unknown("cron".to_owned()))
        );
        let started = control.start(DAEMON_WORKER).unwrap();
        assert_eq!(started.state, DaemonState::Error);
        assert!(started.message.unwrap().contains("no worker supervisor"));
        assert_eq!(control.stop(DAEMON_AUTOPILOT), Ok(false));
        control.detach(DAEMON_WORKER);
        assert_eq!(control.death_reason(DAEMON_WORKER), None);
        assert_eq!(DaemonState::DeadAfterBudget.as_str(), "dead_after_budget");
    }

    #[test]
    fn panic_hook_restores_the_terminal_before_reporting() {
        let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let restore_log = Arc::clone(&order);
        let report_log = Arc::clone(&order);
        let hook = restoring_panic_hook(
            Arc::new(move || restore_log.lock().unwrap().push("restore")),
            Arc::new(move |_info: &std::panic::PanicHookInfo<'_>| {
                report_log.lock().unwrap().push("report");
            }),
        );
        let previous = std::panic::take_hook();
        std::panic::set_hook(hook);
        let outcome = std::panic::catch_unwind(|| panic!("console blew up"));
        std::panic::set_hook(previous);
        assert!(outcome.is_err());
        assert_eq!(*order.lock().unwrap(), vec!["restore", "report"]);
    }

    #[test]
    fn input_bar_edits_submits_and_blurs() {
        let mut bar = InputBar::new("type here");
        assert_eq!(bar.handle_key(key(KeyCode::Char('a'))), InputEvent::Changed);
        assert_eq!(bar.handle_key(key(KeyCode::Char('c'))), InputEvent::Changed);
        assert_eq!(bar.handle_key(key(KeyCode::Left)), InputEvent::Changed);
        assert_eq!(bar.handle_key(key(KeyCode::Char('b'))), InputEvent::Changed);
        assert_eq!(bar.value(), "abc");
        assert_eq!(bar.handle_key(key(KeyCode::Backspace)), InputEvent::Changed);
        assert_eq!(bar.value(), "ac");
        assert_eq!(bar.handle_key(key(KeyCode::Home)), InputEvent::Changed);
        assert_eq!(bar.handle_key(key(KeyCode::Delete)), InputEvent::Changed);
        assert_eq!(bar.value(), "c");
        assert_eq!(bar.handle_key(key(KeyCode::End)), InputEvent::Changed);
        assert_eq!(bar.handle_key(key(KeyCode::Delete)), InputEvent::Ignored);
        assert_eq!(
            bar.handle_key(key(KeyCode::Enter)),
            InputEvent::Submitted("c".to_owned())
        );
        assert_eq!(bar.handle_key(key(KeyCode::Esc)), InputEvent::Blur);
        assert_eq!(bar.handle_key(key(KeyCode::F(1))), InputEvent::Ignored);
        bar.set_value("hello");
        assert_eq!(
            bar.handle_key(super::test_support::ctrl('a')),
            InputEvent::Changed
        );
        assert_eq!(bar.handle_key(key(KeyCode::Right)), InputEvent::Changed);
        assert_eq!(
            bar.handle_key(super::test_support::ctrl('u')),
            InputEvent::Changed
        );
        assert_eq!(bar.value(), "ello");
        bar.clear();
        assert_eq!(bar.value(), "");
        assert_eq!(bar.placeholder(), "type here");
    }

    #[test]
    fn input_bar_renders_placeholder_value_and_cursor() {
        let mut term = terminal(30, 1);
        let mut bar = InputBar::new("filter rows, or /help");
        term.draw(|f| bar.render(f, f.area())).unwrap();
        assert!(buffer_text(&term).starts_with("> filter rows, or /help"));
        bar.set_value("/status");
        bar.set_focused(true);
        term.draw(|f| bar.render(f, f.area())).unwrap();
        assert!(buffer_text(&term).starts_with("> /status"));
        assert!(bar.focused());
        // A long value scrolls so the cursor stays visible.
        bar.set_value("x".repeat(60));
        term.draw(|f| bar.render(f, f.area())).unwrap();
        let text = buffer_text(&term);
        assert!(text.starts_with("> xxxx"));
        assert!(text.trim_end().len() <= 30);
    }
}
