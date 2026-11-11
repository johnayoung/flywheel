//! The hub's side of telemetry capture: what reaches
//! [`crate::telemetry`] and from where.
//!
//! * Claude Code: every hook is a report ([`Hub::handle_report`]), POSTed
//!   by the agent to this hub ([`Hub::hook_report`]) or sent by the report
//!   command; the status line's input arrives the same way.
//! * Codex: the rollout's lines, as the transcript watcher reads them
//!   ([`Hub::record_rollout`]).
//! * A Claude Code session whose status line never reported (an older
//!   CLI): its context and totals are estimated from the transcript's
//!   usage instead ([`Hub::record_usage_fallback`]).

use super::Hub;
use crate::agent::{self, SessionKind};
use crate::report::{self, Report};
use crate::state::SessionRecord;
use crate::telemetry::{Draft, Telemetry, What, claude, now_ms};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

/// What capture knows beside the store.
#[derive(Default)]
pub(super) struct Capture {
    /// Where agent sessions reach this hub over http, once it listens.
    url: RwLock<Option<String>>,
    /// Sessions whose status line has reported: their context window and
    /// totals come from it, not from the transcript.
    status_lines: Mutex<HashSet<String>>,
    /// The Claude Code version a session's status line last showed (kept
    /// in [`CLAUDE_VERSION_FILE`]): what decides http hooks. Never asked
    /// of the CLI itself: the hub runs no agent program on its own.
    claude_version: Mutex<Option<(u32, u32, u32)>>,
    /// The capture token, once read or made (see [`super::capture`]).
    pub(super) token: Mutex<Option<String>>,
}

/// Where the last seen Claude Code version is kept in the state dir.
pub const CLAUDE_VERSION_FILE: &str = "claude-version";

impl Capture {
    /// What a previous run of the hub saw.
    pub(super) fn load(state_dir: &Path) -> Self {
        let seen = std::fs::read_to_string(state_dir.join(CLAUDE_VERSION_FILE))
            .ok()
            .and_then(|text| agent::parse_claude_version(&text));
        Self {
            claude_version: Mutex::new(seen),
            ..Self::default()
        }
    }
}

/// The context window assumed for a model whose size nobody reported.
const ASSUMED_WINDOW: u64 = 200_000;
/// ...and for one already past that.
const ASSUMED_LONG_WINDOW: u64 = 1_000_000;

impl Hub {
    /// Everything this hub's sessions do, recorded.
    pub fn telemetry(&self) -> &Arc<Telemetry> {
        &self.telemetry
    }

    /// Tell the hub where it listens (`http://127.0.0.1:7420`): sessions
    /// started from now on POST their hooks there and get the progress
    /// tool.
    pub fn set_agent_url(&self, url: String) {
        *self
            .capture
            .url
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(url);
    }

    pub(crate) fn agent_url(&self) -> Option<String> {
        self.capture
            .url
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether Claude Code sessions get http hooks: a session's status
    /// line has shown a CLI at or past
    /// [`agent::CLAUDE_HTTP_HOOKS_SINCE`]. Until one has (a hub's first
    /// session ever), hooks run the report command.
    pub(super) fn claude_takes_http_hooks(&self) -> bool {
        self.capture
            .claude_version
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some_and(|seen| seen >= agent::CLAUDE_HTTP_HOOKS_SINCE)
    }

    /// Note the CLI version a status line input names.
    fn note_claude_version(&self, payload: &serde_json::Value) {
        let Some(seen) = payload
            .get("version")
            .and_then(serde_json::Value::as_str)
            .and_then(agent::parse_claude_version)
        else {
            return;
        };
        let mut known = self
            .capture
            .claude_version
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *known != Some(seen) {
            *known = Some(seen);
            let text = format!("{}.{}.{}\n", seen.0, seen.1, seen.2);
            let path = self.options.state_dir.join(CLAUDE_VERSION_FILE);
            if let Err(e) = crate::auth::write_private(&path, text.as_bytes()) {
                eprintln!("fw hub: cannot save {}: {e}", path.display());
            }
        }
    }

    /// Whether `secret` is the running spawn's secret for agent session
    /// `session` (what the progress tool's requests carry).
    pub fn agent_authenticated(&self, session: &str, secret: &str) -> bool {
        self.board().authenticate(session, secret).is_some()
    }

    /// A Claude Code hook POSTed to the hub: the hook's own JSON, from the
    /// session that holds `secret`. It is handled as the same report the
    /// report command would have sent.
    pub fn hook_report(
        &self,
        session: &str,
        secret: &str,
        payload: serde_json::Value,
    ) -> Result<(), String> {
        let str_of = |key: &str| {
            payload
                .get(key)
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let event = str_of("hook_event_name").ok_or("the hook payload names no hook_event_name")?;
        let detail = (event == "Notification")
            .then(|| str_of("notification_type"))
            .flatten();
        self.handle_report(Report {
            session: session.to_owned(),
            secret: secret.to_owned(),
            source: SessionKind::Claude.as_str().to_owned(),
            event,
            detail,
            payload: Some(report::fit_payload(payload, report::MAX_REPORT_BYTES / 2)),
        })
    }

    /// Record what an authenticated report says. True when the report was
    /// telemetry alone (the status line): nothing else is to be done.
    pub(super) fn record_report(
        &self,
        session: &str,
        kind: SessionKind,
        event: &str,
        payload: &serde_json::Value,
    ) -> bool {
        if kind != SessionKind::Claude {
            return false;
        }
        if event == claude::STATUS_LINE_EVENT {
            self.note_claude_version(payload);
            self.capture
                .status_lines
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(session.to_owned());
            self.telemetry
                .record(session, claude::status_line(payload, now_ms()));
            return true;
        }
        let mut drafts = claude::hook(event, payload, now_ms());
        if let Ok(record) = self.store().session(session) {
            name_session(&mut drafts, SessionKind::Claude, record);
        }
        self.telemetry.record(session, drafts);
        false
    }

    /// Record a Codex rollout's lines for `record` (already mapped by
    /// [`crate::telemetry::codex`]); a total's cost is priced from the
    /// session's usage.
    pub(crate) fn record_rollout(&self, record: &SessionRecord, mut drafts: Vec<Draft>) {
        if drafts.is_empty() {
            return;
        }
        name_session(&mut drafts, SessionKind::Codex, record);
        if drafts.iter().any(|d| matches!(d.what, What::Usage { .. })) {
            let priced = self
                .commands
                .usage(record, chrono::Local::now().date_naive())
                .filter(|view| view.total.priced)
                .map(|view| view.total.cost_usd);
            for draft in &mut drafts {
                if let What::Usage { cost_usd, .. } = &mut draft.what {
                    *cost_usd = priced;
                }
            }
        }
        self.telemetry.record_log(&record.id, drafts);
    }

    /// A Claude Code session's usage grew and its status line has not
    /// reported: record the totals and an estimate of the context window
    /// from the transcript's usage.
    pub(crate) fn record_usage_fallback(&self, record: &SessionRecord) {
        if record.kind != SessionKind::Claude
            || self
                .capture
                .status_lines
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&record.id)
        {
            return;
        }
        let Some(view) = self
            .commands
            .usage(record, chrono::Local::now().date_naive())
        else {
            return;
        };
        let now = now_ms();
        let tokens = &view.total.tokens;
        let mut drafts = vec![Draft::new(
            now,
            What::Usage {
                input: tokens.input,
                output: tokens.output,
                cache_read: tokens.cache_read,
                cache_write: tokens.cache_write_5m + tokens.cache_write_1h,
                cost_usd: view.total.priced.then_some(view.total.cost_usd),
            },
        )];
        if let Some(last) = &view.last_turn {
            let tokens = &last.tokens;
            let used =
                tokens.input + tokens.cache_read + tokens.cache_write_5m + tokens.cache_write_1h;
            drafts.push(Draft::new(
                now,
                What::ContextUsage {
                    used,
                    window: assumed_window(used),
                    exact: false,
                },
            ));
        }
        self.telemetry.record(&record.id, drafts);
    }

    /// The session's process exited: its telemetry ends (when the agent
    /// did not say so itself). Sessions that never reported have none.
    pub(super) fn record_exit(&self, id: &str) {
        self.capture
            .status_lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        if self.telemetry.snapshot(id).is_some() {
            self.telemetry.record(
                id,
                vec![Draft::new(
                    now_ms(),
                    What::SessionFinished {
                        reason: Some("exited".to_owned()),
                    },
                )],
            );
        }
    }
}

/// The fleet in lanes (see [`crate::telemetry::lanes`]).
#[derive(Debug, Clone, serde::Serialize)]
pub struct LanesView {
    /// Sessions in the window (captured runs aside).
    pub total: u64,
    /// How many of them were placed (the most urgent, when there are more
    /// than one page holds).
    pub placed: u64,
    /// Captured runs in the window, placed as their batches.
    pub runs: u64,
    pub lanes: Vec<crate::telemetry::lanes::Lane>,
}

impl Hub {
    /// Name each batch for the session of this hub it ran under.
    pub fn name_batches(&self, batches: &mut [crate::telemetry::BatchRollup]) {
        let own = self.telemetry.host();
        let store = self.store();
        for batch in batches {
            if batch.host == own
                && let Some(parent) = &batch.parent
            {
                batch.parent_name = store.session(parent).ok().map(|r| r.name.clone());
            }
        }
    }

    /// The fleet in lanes: sessions active since `since` (of `host`, else
    /// every hub's this one holds), each placed by its snapshot and by
    /// whether anyone looked at its finished turn; captured runs by batch.
    pub fn lanes(
        &self,
        host: Option<String>,
        since: Option<i64>,
    ) -> Result<LanesView, crate::telemetry::StoreError> {
        use crate::telemetry::lanes::{self, Known};
        let mut page = self.telemetry.fleet(&crate::telemetry::FleetQuery {
            host,
            active_since: since,
            limit: crate::telemetry::store::FLEET_PAGE_MAX,
            rollup: true,
            ..Default::default()
        })?;
        self.name_batches(&mut page.batches);
        let own = self.telemetry.host().to_owned();
        let known: Vec<Known> = {
            let store = self.store();
            page.sessions
                .iter()
                .map(|snap| {
                    let record = (snap.host == own)
                        .then(|| store.session(&snap.session).ok())
                        .flatten();
                    Known {
                        project: record
                            .and_then(|r| store.workspace(&r.workspace_id).ok())
                            .map(|w| w.name.clone()),
                        seen: None,
                    }
                })
                .collect()
        };
        let sessions: Vec<_> = page
            .sessions
            .into_iter()
            .zip(known)
            .map(|(snap, mut known)| {
                if snap.host == own {
                    known.seen = self
                        .agent_status(&snap.session)
                        .map(|view| view.status == crate::status::AgentStatus::Idle);
                }
                (snap, known)
            })
            .collect();
        let runs: u64 = page.batches.iter().map(|b| b.sessions).sum();
        Ok(LanesView {
            total: page.total,
            placed: sessions.len() as u64,
            runs,
            lanes: lanes::lanes(sessions, page.batches, now_ms()),
        })
    }
}

/// The context window assumed for a call that used `used` tokens, when
/// nobody reported its size.
pub(super) fn assumed_window(used: u64) -> u64 {
    if used > ASSUMED_WINDOW {
        ASSUMED_LONG_WINDOW
    } else {
        ASSUMED_WINDOW
    }
}

/// How a session is tied to others, from its record.
fn links_of(record: &SessionRecord) -> crate::telemetry::SessionLinks {
    crate::telemetry::SessionLinks {
        continues: record.continues.clone(),
        opened_by: record.opened_by.clone(),
        ..Default::default()
    }
}

/// Say whose events these are: the session's name on this hub and its
/// directory, on its start, else ahead of the rest (a session whose start
/// was not seen, or that was renamed; recording drops it when it says
/// nothing new).
fn name_session(drafts: &mut Vec<Draft>, kind: SessionKind, record: &SessionRecord) {
    let Some(first) = drafts.first().map(|d| d.ts) else {
        return;
    };
    let name = Some(crate::telemetry::summary::text(&record.name));
    let cwd = Some(crate::telemetry::summary::text(
        &record.cwd.to_string_lossy(),
    ));
    let mut named = false;
    for draft in drafts.iter_mut() {
        if let What::SessionStarted {
            label, dir, links, ..
        } = &mut draft.what
        {
            label.clone_from(&name);
            dir.clone_from(&cwd);
            *links = Some(links_of(record));
            named = true;
        }
    }
    if !named {
        drafts.insert(
            0,
            Draft::new(
                first,
                What::SessionStarted {
                    harness: kind.as_str().to_owned(),
                    conversation: None,
                    model: None,
                    label: name,
                    dir: cwd,
                    links: Some(links_of(record)),
                },
            ),
        );
    }
}
