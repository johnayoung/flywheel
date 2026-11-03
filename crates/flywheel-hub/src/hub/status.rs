//! Agent status glue: screen and hook signals into the status board,
//! viewers (seen-tracking), alerts, and status snapshots.

use super::{Hub, HubError, HubEvent, MONITOR_POLL, SCREEN_LOOK, badge_key};
use crate::agent::{self, SessionKind, claude_signal, codex_signal};
use crate::detect::Detection;
use crate::detect::tracker::{Look, ScreenTracker};
use crate::pty::{PtySession, SessionEvent};
use crate::report::Report;
use crate::status::{AgentStatus, Audience, Signal, Snapshot, StatusSource, StatusView};
use crate::vt::TextSource;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;

impl Hub {
    /// Watch an agent session's screen for what its reports miss (see
    /// [`crate::detect::tracker`]): Codex's turns and approval prompts,
    /// Claude Code's interrupts (Esc fires no hook) and folder-trust
    /// dialog (shown before any hook). The kind's detection manifest reads
    /// the server's emulator after output, at most every [`SCREEN_LOOK`].
    /// Runs until the process exits or the PTY is dropped.
    pub(super) fn monitor_agent(
        self: &Arc<Self>,
        kind: SessionKind,
        id: &str,
        generation: u64,
        session: &Arc<PtySession>,
    ) {
        // Screen-only snapshot: the monitor wants the live stream.
        let attachment = session.attach_with(0);
        if attachment.exit.is_some() {
            // Exited before the monitor subscribed: its Exit event is gone.
            return;
        }
        let mut events = attachment.events;
        let weak = Arc::downgrade(self);
        let pty = Arc::downgrade(session);
        let id = id.to_owned();
        let thread_id = id.clone();
        self.monitored().insert(id.clone(), generation);
        let spawned = std::thread::Builder::new()
            .name("hub-agent-monitor".into())
            .spawn(move || {
                let id = thread_id;
                let mut tracker = ScreenTracker::new(kind);
                let mut dirty = true;
                let mut looked: Option<Instant> = None;
                let mut last_output: Option<Instant> = Some(Instant::now());
                let mut last_spell: Option<u64> = None;
                loop {
                    match events.try_recv() {
                        Ok(SessionEvent::Output(_)) => {
                            dirty = true;
                            last_output = Some(Instant::now());
                        }
                        // A resize redraws: look again.
                        Ok(SessionEvent::Resize { .. }) => dirty = true,
                        Ok(SessionEvent::Exit(_)) | Err(broadcast::error::TryRecvError::Closed) => {
                            break;
                        }
                        Err(broadcast::error::TryRecvError::Lagged(_)) => {
                            dirty = true;
                            last_output = Some(Instant::now());
                        }
                        Err(broadcast::error::TryRecvError::Empty) => {
                            std::thread::sleep(MONITOR_POLL);
                        }
                    }
                    let Some(hub) = weak.upgrade() else {
                        break;
                    };
                    let now = Instant::now();
                    let due = looked.is_none_or(|at| now.duration_since(at) >= SCREEN_LOOK);
                    let Some((status, spell, reported)) = hub.board().raw_status(&id, generation)
                    else {
                        break;
                    };
                    // A report started a new spell: look again even if the
                    // screen did not change (its markers may be up).
                    dirty |= last_spell != Some(spell);
                    last_spell = Some(spell);
                    if !((dirty || tracker.pending()) && due) {
                        continue;
                    }
                    let Some(pty) = pty.upgrade() else {
                        break;
                    };
                    looked = Some(now);
                    dirty = false;
                    let screen = pty.screen_text(TextSource::Visible);
                    drop(pty);
                    let manifest = hub.detection.manifest(kind);
                    let verdict = manifest.evaluate(&screen.lines);
                    let file = manifest.source_file();
                    let decision = tracker.observe(Look {
                        status,
                        reported,
                        spell,
                        verdict: &verdict,
                        file: &file,
                        now,
                        last_output,
                    });
                    if let Some(decision) = decision {
                        hub.apply_screen(&id, generation, spell, decision);
                    }
                }
                if let Some(hub) = weak.upgrade() {
                    let mut monitored = hub.monitored();
                    if monitored.get(&id) == Some(&generation) {
                        monitored.remove(&id);
                    }
                }
            });
        if spawned.is_err() {
            self.monitored().remove(&id);
        }
    }

    /// Apply what the screen calls for, unless a report landed since it
    /// was read; push the change if there was one.
    fn apply_screen(
        &self,
        id: &str,
        generation: u64,
        spell: u64,
        decision: crate::detect::tracker::Decision,
    ) {
        let changed = self.board().apply_screen(
            id,
            generation,
            spell,
            decision.status,
            decision.detail.as_deref(),
            decision.source,
        );
        if changed {
            self.status_changed(id);
        }
    }

    /// The detection manifests in force.
    pub fn detection(&self) -> &Arc<Detection> {
        &self.detection
    }

    /// Why session `id` shows its status: what set it, and a fresh read
    /// of its live screen with its kind's manifest (`fw hub explain`).
    pub fn explain(&self, id: &str) -> Result<crate::detect::explain::Explanation, HubError> {
        let record = self.store().session(id)?.clone();
        let screen = self
            .live_session(id)
            .map(|pty| pty.screen_text(TextSource::Visible).lines);
        let mut explanation =
            crate::detect::explain::explain_screen(&self.detection, record.kind, screen.as_deref());
        explanation.session = Some(crate::detect::explain::SessionInfo {
            id: record.id.clone(),
            name: record.name.clone(),
        });
        if record.kind.is_agent() {
            let board = self.board();
            explanation.status = board.status(id, Audience::Anyone);
            explanation.source = board.source(id).cloned();
            explanation.source_label = explanation.source.as_ref().map(StatusSource::label);
        }
        Ok(explanation)
    }

    /// Apply a status signal; push the change if there was one.
    pub(super) fn apply_signal(
        &self,
        id: &str,
        generation: u64,
        signal: Signal,
        detail: Option<&str>,
        source: StatusSource,
    ) {
        let changed = self
            .board()
            .apply_report(id, generation, signal, detail, source);
        if changed {
            self.status_changed(id);
        }
    }

    /// A session's status changed: refresh its badge, tell clients.
    fn status_changed(&self, id: &str) {
        // A turn started or ended: its files may have changed.
        let key = self.store().session(id).ok().map(badge_key);
        if let Some(key) = key {
            self.git.nudge(key);
        }
        self.emit(HubEvent::Status {
            session_id: id.to_owned(),
        });
    }

    /// Handle one agent report from the report socket: authenticate it
    /// against the spawn's secret, then apply what it means.
    pub fn handle_report(&self, report: Report) -> Result<(), String> {
        let (kind, conversation) = {
            let store = self.store();
            let record = store.session(&report.session).map_err(|e| e.to_string())?;
            (record.kind, record.agent_session_id.clone())
        };
        if !kind.is_agent() || report.source != kind.as_str() {
            return Err(format!(
                "session {} is a {} session, not {}",
                report.session,
                kind.as_str(),
                report.source
            ));
        }
        let Some(generation) = self.board().authenticate(&report.session, &report.secret) else {
            return Err(format!(
                "report secret does not match session {}",
                report.session
            ));
        };
        let payload = report.payload.clone().unwrap_or(serde_json::Value::Null);
        let (signal, detail) = match kind {
            SessionKind::Claude => (
                claude_signal(&report.event, report.detail.as_deref()),
                agent::claude_detail(&report.event, report.detail.as_deref(), &payload),
            ),
            SessionKind::Codex if report.event == "notify" => (codex_signal(&payload), None),
            _ => (None, None),
        };
        if let Some(signal) = signal {
            let source = StatusSource::Hook {
                agent: report.source.clone(),
                event: match &report.detail {
                    Some(qualifier) => format!("{} {qualifier}", report.event),
                    None => report.event.clone(),
                },
            };
            self.apply_signal(
                &report.session,
                generation,
                signal,
                detail.as_deref(),
                source,
            );
        }
        if let Some(reported) = agent::reported_conversation(kind, &payload)
            && conversation.as_deref() != Some(reported.as_str())
        {
            let changed = self
                .store()
                .set_agent_session_id(&report.session, &reported)
                .map_err(|e| e.to_string())?;
            if changed {
                self.emit(HubEvent::Sessions);
            }
        }
        self.commands
            .note_report(&report.session, kind, &report.event, &payload);
        Ok(())
    }

    /// Register an events client (a browser, by its persistent client id).
    pub fn connect_viewer(&self, client: &str) -> u64 {
        self.board().connect(client)
    }

    pub fn disconnect_viewer(&self, viewer: u64) {
        let left = self.board().disconnect(viewer);
        for session_id in left {
            self.emit(HubEvent::Unwatched { session_id });
        }
    }

    /// Record every session a client connection shows (each pane of a
    /// split layout; empty: none, or the page is hidden).
    pub fn set_viewing_many(&self, viewer: u64, session_ids: &[&str]) {
        let (seen, left) = {
            let mut board = self.board();
            let before = board.viewing(viewer).to_vec();
            let seen = board.view_many(viewer, session_ids);
            let left: Vec<String> = before
                .into_iter()
                .filter(|b| !board.is_watched(b))
                .collect();
            (seen, left)
        };
        for session_id in left {
            self.emit(HubEvent::Unwatched { session_id });
        }
        for session_id in seen {
            self.emit(HubEvent::Status { session_id });
        }
    }

    /// What a push notification about session `id` would say, and the
    /// facts deciding whether to send it (`None` once it is not tracked).
    pub fn alert_subject(&self, id: &str) -> Option<crate::notify::Subject> {
        let alert = self.board().alert(id)?;
        let store = self.store();
        let session = store.session(id).ok()?;
        let workspace = store
            .workspace(&session.workspace_id)
            .map(|w| w.name.clone())
            .unwrap_or_default();
        let session = session.clone();
        drop(store);
        // A worktree session's branch: the badge cache's (it may have been
        // switched), else the one it was cut on.
        let branch = session.worktree.as_ref().map(|worktree| {
            self.git
                .get(&badge_key(&session))
                .and_then(|git| git.branch)
                .unwrap_or_else(|| worktree.branch.clone())
        });
        Some(crate::notify::Subject {
            session_id: session.id,
            session: session.name,
            workspace,
            branch,
            kind: session.kind,
            alert,
        })
    }

    /// An agent session's status before seen tracking, and its spell (a
    /// new spell of `done` is a new finished turn); `None` once untracked.
    pub fn turn_state(&self, id: &str) -> Option<(AgentStatus, u64)> {
        self.board().alert(id).map(|a| (a.status, a.spell))
    }

    /// Every agent session's status and workspace rollup, for one client.
    pub fn status_snapshot(&self, client: &str) -> Snapshot {
        self.board().snapshot(Audience::Client(client))
    }

    /// One session's status and its workspace's rollup, for one client
    /// (`None` once it is no longer tracked).
    pub fn status_for(
        &self,
        client: &str,
        session_id: &str,
    ) -> Option<(StatusView, String, Option<AgentStatus>)> {
        let board = self.board();
        let view = board.status(session_id, Audience::Client(client))?;
        let workspace = board.workspace_of(session_id)?.to_owned();
        let rollup = board.workspace_status(&workspace, Audience::Client(client));
        Some((view, workspace, rollup))
    }
}
