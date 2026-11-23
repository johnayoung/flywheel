//! The hub's holds (see [`crate::holds`]): what its sessions wait on the
//! operator for, answering them one at a time or a group at once, and
//! recording every answer where it is given.
//!
//! Where each kind's answer is recorded:
//! * an agent's start or end request: when the operator approves or
//!   declines it (`hub/starts.rs`, `hub/endings.rs`);
//! * an ask (`ask_operator`, or a turn that ended on a question): when
//!   the session's next prompt arrives, whoever typed or sent it (the
//!   prompt is the answer; an agent's signed message is `agent:<name>`);
//! * a permission prompt: when the hub answers it "Yes" (here, or after
//!   writing a rule), or when a Claude Code request the hub was told of
//!   moves on (answered in the terminal);
//! * a manual gate: when its decision is enqueued from the hub
//!   (`server/autopilot_api.rs`, `server/holds_api.rs`).

use super::starts::running_agent;
use super::{Hub, HubError};
use crate::holds::{self, Decision, Hold, HoldKind, NewDecision};
use crate::perms::Folder;
use crate::perms::rule::Target;
use crate::perms::synth::Request;
use crate::state::{PendingEnd, PendingStart, SessionRecord, StateError};
use crate::status::AgentStatus;
use crate::telemetry::event::AskKind;
use std::sync::{Arc, PoisonError};

/// A start request as a hold.
pub(super) fn start_hold(from: &SessionRecord, pending: &PendingStart) -> Hold {
    let first = pending
        .prompt
        .as_deref()
        .and_then(|p| p.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("")
        .trim();
    let named = pending
        .name
        .as_deref()
        .map_or_else(String::new, |n| format!(" {n:?}"));
    Hold {
        id: format!("start:{}:{}", from.id, pending.id),
        kind: HoldKind::AwaitingApproval,
        cause: format!("start_session from {}", from.name),
        session: Some(from.id.clone()),
        session_name: Some(from.name.clone()),
        folder: None,
        question: holds::cap(
            &format!(
                "{} asks to start a {} session{named}: {first}",
                from.name,
                pending.kind.as_str()
            ),
            holds::TEXT_CAP,
        ),
        recommendation: None,
        answers: vec!["approve".into(), "decline".into()],
        since: pending.created_at.timestamp_millis(),
    }
}

/// An end (or replace) request as a hold; `target` its target's name.
pub(super) fn end_hold(from: &SessionRecord, pending: &PendingEnd, target: &str) -> Hold {
    let verb = if pending.replace.is_some() {
        "replace"
    } else {
        "end"
    };
    Hold {
        id: format!("end:{}:{}", from.id, pending.id),
        kind: HoldKind::AwaitingApproval,
        cause: format!("{verb}_session from {}", from.name),
        session: Some(from.id.clone()),
        session_name: Some(from.name.clone()),
        folder: None,
        question: format!("{} asks to {verb} {target}", from.name),
        recommendation: None,
        answers: vec!["approve".into(), "decline".into()],
        since: pending.created_at.timestamp_millis(),
    }
}

/// A stuck lease as a hold: `lease:<lease>:<granted>`, held by the first
/// session that waits (its owner hears it too).
pub(super) fn stuck_hold(stuck: &super::leases::Stuck, name_of: &dyn Fn(&str) -> String) -> Hold {
    let span = super::starts::span;
    let typical = stuck.typical_ms.map_or_else(
        || "no history".to_owned(),
        |t| format!("typical {}", span(t)),
    );
    let doing = match stuck.doing.as_slice() {
        [] => String::new(),
        lines => format!(" Its process: {}.", lines.join(" / ")),
    };
    let session = stuck.waiting_sessions.first().cloned();
    Hold {
        id: format!("lease:{}:{}", stuck.lease.id, stuck.lease.granted_at),
        kind: HoldKind::AwaitingApproval,
        cause: format!("lease {} stuck", stuck.lease.resource),
        session_name: session.as_deref().map(name_of),
        session,
        folder: None,
        question: holds::cap(
            &format!(
                "{} held {} by {} ({typical}; stuck after {}) while {} wait{}.{doing} \
                 Release breaks the lease and sends SIGTERM to its process group.",
                stuck.lease.resource,
                span(stuck.held_ms),
                stuck.holder,
                span(stuck.threshold_ms),
                stuck.waiting.join(", "),
                if stuck.waiting.len() == 1 { "s" } else { "" },
            ),
            holds::TEXT_CAP,
        ),
        recommendation: None,
        answers: vec!["release".into(), "keep waiting".into()],
        since: stuck.lease.granted_at + stuck.threshold_ms,
    }
}

/// The decision on an approval hold.
pub(super) fn approval_decision(hold: Hold, answer: &str) -> NewDecision {
    NewDecision {
        hold: hold.id,
        kind: Some(hold.kind),
        cause: hold.cause,
        session: hold.session,
        session_name: hold.session_name,
        question: hold.question,
        recommendation: hold.recommendation,
        answer: answer.to_owned(),
        by: "operator".into(),
        batch: None,
    }
}

/// What a permission request asks, in one line, and its cause (the tool
/// and, for a command, its program).
fn request_text(request: &Request) -> (String, String) {
    match &request.target {
        Target::Command { command } => {
            let program = command
                .split_whitespace()
                .find(|w| !w.contains('='))
                .unwrap_or("");
            (
                format!("{} {command}", request.tool),
                format!("{} {program}", request.tool).trim().to_owned(),
            )
        }
        Target::Edit { path } | Target::Read { path } => (
            format!("{} {}", request.tool, path.display()),
            request.tool.clone(),
        ),
        Target::Fetch { url } => {
            let host = crate::perms::rule::url_host(url).unwrap_or_default();
            (
                format!("{} {url}", request.tool),
                format!("{} {host}", request.tool).trim().to_owned(),
            )
        }
        Target::Tool { name } => (name.clone(), name.clone()),
    }
}

/// A permission prompt's decision.
pub(crate) fn permission_decision(
    record: &SessionRecord,
    request: &Request,
    answer: &str,
) -> NewDecision {
    let (question, cause) = request_text(request);
    NewDecision {
        hold: format!("perm:{}:{}", record.id, request.key),
        kind: Some(HoldKind::Permission),
        cause,
        session: Some(record.id.clone()),
        session_name: Some(record.name.clone()),
        question: holds::cap(&question, holds::TEXT_CAP),
        recommendation: None,
        answer: answer.to_owned(),
        by: "operator".into(),
        batch: None,
    }
}

/// The most of an answer the bus carries (the record keeps it whole).
const EVENT_ANSWER_CAP: usize = 2000;

impl Hub {
    /// The decision record.
    pub fn decisions(&self) -> &holds::DecisionStore {
        &self.decisions
    }

    /// Keep `new` (its batch filled in when one was registered for its
    /// hold) and tell the bus (`decision.made`: the answer, capped,
    /// never the question). A store that cannot be written is logged: the
    /// answer itself has gone through.
    pub fn record_decision(&self, mut new: NewDecision) -> Option<Decision> {
        if new.batch.is_none() {
            new.batch = self.take_batch(&new.hold);
        }
        let at = chrono::Utc::now().timestamp_millis();
        match self.decisions.record(&new, at) {
            Ok(decision) => {
                let session = decision
                    .session
                    .as_deref()
                    .and_then(|id| self.event_session(id))
                    .unwrap_or(serde_json::Value::Null);
                self.emit_lifecycle(
                    "decision.made",
                    serde_json::json!({
                        "session": session,
                        "id": decision.id,
                        "hold": decision.hold,
                        "kind": decision.kind,
                        "cause": decision.cause,
                        "answer": holds::cap(&decision.answer, EVENT_ANSWER_CAP),
                        "by": decision.by,
                        "at": decision.at,
                        "batch": decision.batch,
                    }),
                );
                Some(decision)
            }
            Err(e) => {
                eprintln!("fw hub: could not record the decision on {}: {e}", new.hold);
                None
            }
        }
    }

    fn batches(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, String>> {
        self.batches.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The batch registered for `hold`, taken.
    fn take_batch(&self, hold: &str) -> Option<String> {
        self.batches().remove(hold)
    }

    /// Everything this hub's sessions wait on the operator for (manual
    /// gates are the server's to add).
    pub fn session_holds(&self) -> Vec<Hold> {
        let records: Vec<SessionRecord> = self.store().sessions().to_vec();
        let name_of = |id: &str| {
            records
                .iter()
                .find(|r| r.id == id)
                .map_or_else(|| format!("a removed session ({id})"), |r| r.name.clone())
        };
        let mut out = Vec::new();
        for record in &records {
            for pending in &record.start_requests {
                out.push(start_hold(record, pending));
            }
            for pending in &record.end_requests {
                out.push(end_hold(record, pending, &name_of(&pending.target)));
            }
            if !running_agent(record) {
                continue;
            }
            if let Some(hold) = self.blocked_hold(record) {
                out.push(hold);
            } else if let Some(hold) = self.ask_hold(record) {
                out.push(hold);
            }
        }
        out.extend(self.stuck_leases().iter().map(|s| stuck_hold(s, &name_of)));
        out
    }

    /// A blocked session's hold: its permission prompt, or the question
    /// dialog it shows (answered there).
    fn blocked_hold(&self, record: &SessionRecord) -> Option<Hold> {
        let status = self.agent_status(&record.id)?;
        if status.status != AgentStatus::Blocked {
            return None;
        }
        let since = status.since.timestamp_millis();
        let base = |id: String, kind, cause: String, question: String, answers| Hold {
            id,
            kind,
            cause,
            session: Some(record.id.clone()),
            session_name: Some(record.name.clone()),
            folder: None,
            question: holds::cap(&question, holds::TEXT_CAP),
            recommendation: None,
            answers,
            since,
        };
        if let Some(first) = status.questions.first() {
            let question = first.question.clone();
            return Some(base(
                format!("dialog:{}:{since}", record.id),
                HoldKind::NeedsInput,
                holds::cause_of(&question),
                question,
                Vec::new(),
            ));
        }
        if let Some(request) = self.perms().pending_for(&record.id) {
            let (question, cause) = request_text(&request);
            return Some(base(
                format!("perm:{}:{}", record.id, request.key),
                HoldKind::Permission,
                cause,
                question,
                vec!["yes".into()],
            ));
        }
        let snap = self.telemetry().snapshot(&record.id);
        let tool = snap.as_ref().and_then(|s| s.blocked_tool.clone());
        let summary = snap
            .as_ref()
            .and_then(|s| s.tool.as_ref())
            .filter(|t| Some(&t.name) == tool.as_ref())
            .and_then(|t| t.summary.clone());
        let (question, cause) = match (&tool, &summary) {
            (Some(tool), Some(summary)) => {
                let program = summary.split_whitespace().next().unwrap_or("");
                (
                    format!("{tool} {summary}"),
                    format!("{tool} {program}").trim().to_owned(),
                )
            }
            (Some(tool), None) => (tool.clone(), tool.clone()),
            _ => {
                let detail = status.detail.unwrap_or_else(|| "waiting on you".into());
                (detail.clone(), holds::cause_of(&detail))
            }
        };
        Some(base(
            format!("perm:{}:{since}", record.id),
            HoldKind::Permission,
            cause,
            question,
            vec!["yes".into()],
        ))
    }

    /// An asking session's hold: what its finished turn asks.
    fn ask_hold(&self, record: &SessionRecord) -> Option<Hold> {
        let snap = self.telemetry().snapshot(&record.id)?;
        if snap.status != "asking" || snap.continued_by.is_some() {
            return None;
        }
        let ask = snap.ask?;
        if ask.kind == AskKind::Handoff {
            return None;
        }
        let question = match &ask.command {
            Some(command) => format!("{} (run: {command})", ask.text),
            None => ask.text.clone(),
        };
        let recommendation = ask.recommendation.clone().or_else(|| {
            (ask.kind == AskKind::Decide)
                .then(|| ask.options.first().cloned())
                .flatten()
        });
        Some(Hold {
            id: format!("ask:{}:{}", record.id, ask.since),
            kind: ask.hold.unwrap_or(HoldKind::NeedsInput),
            cause: ask
                .cause
                .clone()
                .unwrap_or_else(|| holds::cause_of(&ask.text)),
            session: Some(record.id.clone()),
            session_name: Some(record.name.clone()),
            folder: None,
            question: holds::cap(&question, holds::TEXT_CAP),
            recommendation,
            answers: ask.options.clone(),
            since: ask.since,
        })
    }

    /// Answer the session hold `id` with `answer` (`batch`: the group
    /// answer it is part of): what became of it.
    pub fn answer_session_hold(
        self: &Arc<Self>,
        id: &str,
        answer: &str,
        batch: Option<&str>,
    ) -> Result<String, HubError> {
        let answer = answer.trim();
        if answer.is_empty() {
            return Err(StateError::Invalid("give an answer".into()).into());
        }
        let mut parts = id.splitn(3, ':');
        let (prefix, session, rest) = (
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
        );
        let not_found = || -> HubError {
            StateError::NotFound(format!("the hold {id} (answered already, or gone)")).into()
        };
        if session.is_empty() || rest.is_empty() {
            return Err(not_found());
        }
        let current = self
            .session_holds()
            .into_iter()
            .find(|h| h.id == id)
            .ok_or_else(not_found)?;
        let yes_no = |answer: &str| match answer.to_ascii_lowercase().as_str() {
            "approve" | "approved" | "yes" | "start" => Ok(true),
            "decline" | "declined" | "no" | "deny" => Ok(false),
            other => Err(HubError::from(StateError::Invalid(format!(
                "{other:?}: answer approve or decline"
            )))),
        };
        if let Some(batch) = batch {
            self.batches().insert(id.to_owned(), batch.to_owned());
        }
        let done = match prefix {
            "lease" => self.answer_stuck_lease(session, answer).inspect(|_| {
                self.record_decision(approval_decision(current.clone(), answer));
            }),
            "start" => {
                if yes_no(answer)? {
                    self.approve_start(session, rest, None, None)
                        .map(|view| format!("started {}", view.record.name))
                } else {
                    self.decline_start(session, rest)
                        .map(|()| "declined".to_owned())
                }
            }
            "end" => {
                if yes_no(answer)? {
                    self.approve_end(session, rest)
                        .map(|_| "approved".to_owned())
                } else {
                    self.decline_end(session, rest)
                        .map(|()| "declined".to_owned())
                }
            }
            "perm" => {
                if !answer.eq_ignore_ascii_case("yes") {
                    Err(StateError::Invalid(
                        "a permission prompt takes yes here; answer anything else in the session"
                            .into(),
                    )
                    .into())
                } else {
                    let record = self.store().session(session)?.clone();
                    let folder = Folder::resolve(&record.cwd).map_err(StateError::Invalid)?;
                    // perms_answer records it, under the request's own id.
                    if let (Some(batch), Some(request)) =
                        (batch, self.perms_request(&record, &folder))
                    {
                        self.batches().insert(
                            format!("perm:{}:{}", record.id, request.key),
                            batch.to_owned(),
                        );
                    }
                    let (answered, note) = self.perms_answer(&record, &folder, None);
                    if answered {
                        Ok(note)
                    } else {
                        Err(StateError::Conflict(note).into())
                    }
                }
            }
            "ask" => {
                // Its next prompt is the answer, recorded when it arrives
                // (the batch waits under this ask's id: a later ask never
                // takes it).
                self.send_or_queue(session, answer)
                    .map(|delivery| match delivery {
                        crate::queue::Delivery::Pasted { .. } => "sent".to_owned(),
                        crate::queue::Delivery::Queued { .. } => "queued".to_owned(),
                    })
            }
            _ => Err(StateError::Invalid(format!(
                "{} shows a question dialog: answer it in the session",
                current.session_name.as_deref().unwrap_or("the session")
            ))
            .into()),
        };
        // Whatever a recorded answer did not take (an ask's waits for its
        // prompt).
        if prefix != "ask" || done.is_err() {
            self.take_batch(id);
        }
        self.batches()
            .retain(|key, _| !key.starts_with(&format!("perm:{session}:")));
        done
    }

    /// A prompt arrived in session `session` (call before the prompt
    /// reaches telemetry): whether it answers what the session asks, the
    /// operator's or its lead's message, recorded as the ask's answer when
    /// it was asking. A relayed one (a watch, a hub notice, a background
    /// task's result, another session's message) answers nothing: the ask
    /// stays the operator's.
    pub(crate) fn note_prompt(&self, session: &str, prompt: &str) -> bool {
        let (record, by) = {
            let store = self.store();
            let sessions = store.sessions();
            let Some(record) = sessions.iter().find(|r| r.id == session).cloned() else {
                return true;
            };
            let sender =
                holds::sender_of_prompt(prompt, |n| sessions.iter().any(|known| known.name == n));
            let by = match sender {
                holds::Sender::Operator => "operator".to_owned(),
                holds::Sender::Agent(name) => {
                    let lead = super::endings::effective_owner(&record, sessions);
                    let leads = sessions
                        .iter()
                        .any(|s| s.name == name && lead.as_deref() == Some(s.id.as_str()));
                    if !leads {
                        return false;
                    }
                    format!("agent:{name}")
                }
                holds::Sender::Relay => return false,
            };
            (record, by)
        };
        // Asking, or a relayed prompt's turn holds the ask aside.
        let Some(ask) = self.telemetry().snapshot(session).and_then(|snap| snap.ask) else {
            return true;
        };
        let hold = self.ask_hold(&record);
        let id = format!("ask:{session}:{}", ask.since);
        let batch = self.take_batch(&id);
        // Batches of this session's earlier asks, answered otherwise.
        self.batches()
            .retain(|key, _| !key.starts_with(&format!("ask:{session}:")));
        self.record_decision(NewDecision {
            hold: id,
            kind: Some(hold.as_ref().map_or(HoldKind::NeedsInput, |h| h.kind)),
            cause: hold
                .as_ref()
                .map_or_else(|| holds::cause_of(&ask.text), |h| h.cause.clone()),
            session: Some(record.id.clone()),
            session_name: Some(record.name.clone()),
            question: hold
                .as_ref()
                .map_or_else(|| ask.text.clone(), |h| h.question.clone()),
            recommendation: hold.and_then(|h| h.recommendation),
            answer: prompt.trim().to_owned(),
            by,
            batch,
        });
        true
    }

    /// A Claude Code permission request the hub was told of moved on
    /// without the hub answering it: answered in the terminal.
    pub(crate) fn note_permission_moved_on(&self, session: &str, request: &Request) {
        let Ok(record) = self.store().session(session).cloned() else {
            return;
        };
        self.record_decision(permission_decision(
            &record,
            request,
            "answered in the terminal",
        ));
    }
}
