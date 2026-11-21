//! Agents ending agents: `end_session`, `replace_session`, and
//! `give_session` ([`Hub::agent_end`], [`Hub::agent_replace`],
//! [`Hub::give_session`]), the operator's approvals of them, and the end
//! itself, driven from the queue's tick ([`Hub::endings_tick`]).
//!
//! An end never acts mid-flight. A graceful end sets the session's other
//! queued messages aside and queues the caller's final prompt (only it may
//! go); once that prompt's turn is over, the session rests (the queue's
//! own rest rules), and telemetry shows no open tool call and no running
//! subagent, the hub hangs it up. An `at_rest` end skips the final prompt.
//! A replacement starts its successor before the old session ends. An end
//! not done by its deadline is called off: the set-aside messages go back,
//! the session keeps running, and the caller is told where it stuck. The
//! end in progress is kept on the session's record, so a hub restart (a
//! live `fw hub upgrade` too) carries it on.
//!
//! Who may end what: the session's effective owner ([`effective_owner`]),
//! at once; anyone else only with the operator's approval (a Needs-you
//! row). Nobody ends itself; force is the operator's alone. These rules
//! bind the tools, not an agent with a shell (it can read the hub's token).

use super::starts::{refused, running_agent, span, trimmed};
use super::{Hub, HubError, HubEvent, SessionView};
use crate::git;
use crate::queue::{ItemState, QueueItem, SETTLE, SessionQueue, When};
use crate::state::{
    Actor, ApprovalWhy, DirtyNote, DroppedItem, EndMode, EndPhase, EndRecord, Ending, PendingEnd,
    PendingStart, ProcessNote, SessionRecord, SessionState, StartPolicy, StateError, Successor,
    WorktreeRequest, new_id, validate_name,
};
use crate::status::{AgentStatus, State};
use crate::telemetry::reduce::{WorkSource, WorkStatus};
use crate::telemetry::{Draft, What, now_ms};
use chrono::Utc;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

/// How long an end may take, unless the caller says (minutes).
pub const DEFAULT_WAIT_MINUTES: u32 = 20;
/// The longest an end may be given (minutes).
pub const MAX_WAIT_MINUTES: u32 = 120;
/// Processes named in an answer before the rest are counted.
const PROCESSES_NAMED: usize = 20;
/// Characters of a process's command line kept.
const COMMAND_CHARS: usize = 120;
/// Characters of a set-aside message's first line kept.
const PREVIEW_CHARS: usize = 100;
/// How long after a background subagent finished its agent may still
/// take its result up (a turn of its own): not at rest meanwhile.
const WAKE_WINDOW_MS: i64 = 120_000;
/// How long a turn that started and never ended (no Stop) must have been
/// quiet before it no longer holds its session busy. In telemetry.db
/// (2026-11-20, 41,700 gaps between events inside a turn with no tool
/// call open) the longest silence of a live turn was 232 s, 99.99% under
/// 151 s; the model thinking makes no events.
const OPEN_TURN_QUIET_MS: i64 = 300_000;
/// What a refused force says.
pub const FORCE_REFUSED: &str = "force is operator only (pane menu or fw hub kill)";

/// What an agent asks `end_session` for.
#[derive(Debug, Clone)]
pub struct AgentEnd {
    /// The session's name or id.
    pub to: String,
    pub mode: EndMode,
    /// A graceful end's last prompt to it.
    pub final_prompt: Option<String>,
    pub wait_minutes: Option<u32>,
    /// What to do with its directory once it ended (see
    /// [`crate::close`]).
    pub close: Option<crate::close::CloseChoices>,
    /// Only answer the close checklist; end nothing.
    pub dry_run: bool,
}

/// What an agent asks `replace_session` for.
#[derive(Debug, Clone, Default)]
pub struct AgentReplace {
    pub to: String,
    /// The successor's name and whole first prompt.
    pub name: String,
    pub prompt: String,
    /// The old session's last prompt.
    pub final_prompt: String,
    /// The successor's model; default: the old session's.
    pub model: Option<String>,
    /// One of the workspace's folders; default: the old session's.
    pub folder: Option<PathBuf>,
    pub worktree: Option<WorktreeRequest>,
    pub wait_minutes: Option<u32>,
}

/// Whether `record` lives (or will, once the hub restores it).
fn alive(record: &SessionRecord) -> bool {
    record.state == SessionState::Running || record.state.is_restorable()
}

/// Who controls `record`: its `owner` if given, else the agent that
/// opened it; an owner that has ended hands control on to its newest
/// successor (its replacement, a handoff's continuation), and so on.
/// `None`: the operator started it, and nobody was given it.
pub(crate) fn effective_owner(
    record: &SessionRecord,
    sessions: &[SessionRecord],
) -> Option<String> {
    let start = record.owner.clone().or_else(|| record.opened_by.clone())?;
    Some(controller(start, sessions))
}

/// The session `record` hangs under (the tree, `mine`, `children`
/// watches, `fw.parent`): its [`effective_owner`] while that session is
/// still on the hub; once it was removed, its opener's chain instead;
/// that gone too, `None` (a root). `opened_by` stays the history.
pub(crate) fn effective_parent(
    record: &SessionRecord,
    sessions: &[SessionRecord],
) -> Option<String> {
    let present = |id: &String| *id != record.id && sessions.iter().any(|s| s.id == *id);
    record
        .owner
        .clone()
        .map(|owner| controller(owner, sessions))
        .filter(present)
        .or_else(|| {
            record
                .opened_by
                .clone()
                .map(|opener| controller(opener, sessions))
                .filter(present)
        })
}

/// Who controls what `start` controlled: `start` while it lives, else
/// its newest successor's controller; the last one reached when the chain
/// ends (or loops).
fn controller(start: String, sessions: &[SessionRecord]) -> String {
    let mut at = start;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(at.clone()) {
            return at;
        }
        let current = sessions.iter().find(|s| s.id == at);
        if current.is_some_and(alive) {
            return at;
        }
        let next = current
            .and_then(|s| s.ended.as_ref())
            .and_then(|e| e.successor.clone())
            .or_else(|| {
                sessions
                    .iter()
                    .filter(|s| s.continues.as_deref() == Some(at.as_str()))
                    .max_by_key(|s| s.created_at)
                    .map(|s| s.id.clone())
            });
        match next {
            Some(next) => at = next,
            None => return at,
        }
    }
}

/// `minutes` as asked, within 1..=[`MAX_WAIT_MINUTES`].
fn wait_of(minutes: Option<u32>) -> Result<u32, HubError> {
    match minutes {
        None => Ok(DEFAULT_WAIT_MINUTES),
        Some(m) if (1..=MAX_WAIT_MINUTES).contains(&m) => Ok(m),
        Some(m) => Err(refused(format!(
            "wait_minutes {m} is outside 1..={MAX_WAIT_MINUTES}"
        ))),
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((cut, _)) => format!("{}...", &flat[..cut]),
        None => flat,
    }
}

/// A queued message's first line, past the line naming its sender.
pub(crate) fn preview(item: &QueueItem) -> String {
    let body = match &item.from {
        Some(_) => item
            .text
            .split_once("\n\n")
            .map_or(item.text.as_str(), |(_, rest)| rest),
        None => item.text.as_str(),
    };
    let first = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    one_line(first, PREVIEW_CHARS)
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// Process `pid`'s command line, cut short (Linux; else its pid only).
fn command_of(pid: u32) -> String {
    let argv = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|raw| {
            raw.split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|c| !c.is_empty())
        .or_else(|| {
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .ok()
                .map(|c| c.trim().to_owned())
        })
        .unwrap_or_else(|| "?".to_owned());
    one_line(&argv, COMMAND_CHARS)
}

/// How deep a process's parents are walked looking for the agent.
const ANCESTRY_MAX: usize = 64;

/// Whether `command` is a shell an agent runs its commands in (Claude
/// Code's Bash tool sources a shell snapshot; Codex runs `bash -lc`).
fn tool_shell(command: &str, codex: bool) -> bool {
    command.contains("/shell-snapshots/")
        || (codex
            && command
                .split_whitespace()
                .nth(1)
                .is_some_and(|a| a == "-lc"))
}

/// Whether process `pid` is the agent's own helper (an MCP server and
/// what it runs): reached from `leader` (the agent) through its parents
/// with no shell the agent runs commands in on the way. Work it started
/// through such a shell, or that left it (a `nohup` job, reparented), is
/// not. `parent` and `command` read a process's parent pid and command.
fn is_helper(
    pid: u32,
    leader: u32,
    codex: bool,
    parent: &dyn Fn(u32) -> Option<u32>,
    command: &dyn Fn(u32) -> String,
) -> bool {
    let mut at = pid;
    for _ in 0..ANCESTRY_MAX {
        if at == leader {
            return true;
        }
        if tool_shell(&command(at), codex) {
            return false;
        }
        match parent(at) {
            Some(up) if up > 1 => at = up,
            _ => return false,
        }
    }
    false
}

/// Process `pid`'s parent (Linux `/proc`; the field after the state).
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Every process whose parents lead to `leader` (Linux `/proc`; empty
/// elsewhere). Claude Code runs each Bash-tool command in a session of
/// its own, so what an agent starts that way is outside its terminal's
/// session: only its parents tie it to the agent.
fn descendants(leader: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let parents: std::collections::HashMap<u32, u32> = entries
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| Some((pid, parent_of(pid)?)))
        .collect();
    let mut found = Vec::new();
    for &pid in parents.keys() {
        let mut at = pid;
        for _ in 0..ANCESTRY_MAX {
            match parents.get(&at) {
                Some(&up) if up == leader => {
                    found.push(pid);
                    break;
                }
                Some(&up) if up > 1 => at = up,
                _ => break,
            }
        }
    }
    found.sort_unstable();
    found
}

/// Hang up the processes an end found outside the terminal's session
/// (`pids`, with the command each had): SIGHUP, then SIGKILL after
/// [`super::KILL_GRACE`] to any still running the same command (a pid
/// reused meanwhile is left alone).
fn hang_up_outside(pids: Vec<(u32, String)>) {
    if pids.is_empty() {
        return;
    }
    let same = |pid: u32, command: &str| command_of(pid) == command;
    let signal = |pid: u32, sig: libc::c_int| {
        if let Ok(pid) = libc::pid_t::try_from(pid) {
            // SAFETY: kill(2) has no memory-safety preconditions.
            unsafe { libc::kill(pid, sig) };
        }
    };
    for (pid, command) in &pids {
        if same(*pid, command) {
            signal(*pid, libc::SIGHUP);
        }
    }
    let spawned = std::thread::Builder::new()
        .name("hub-end-reap".into())
        .spawn(move || {
            std::thread::sleep(super::KILL_GRACE);
            for (pid, command) in &pids {
                if same(*pid, command) {
                    signal(*pid, libc::SIGKILL);
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("fw hub: ending: cannot wait to kill what outlived its hangup: {e}");
    }
}

/// The processes worth naming: work, not the agent's own helpers.
fn work_of(killed: &[ProcessNote]) -> Vec<&ProcessNote> {
    killed.iter().filter(|p| !p.helper).collect()
}

fn helpers_text(killed: &[ProcessNote]) -> Option<String> {
    let helpers = killed.iter().filter(|p| p.helper).count();
    (helpers > 0).then(|| {
        format!(
            "{} of the agent's own (MCP servers and their tooling, which end with any session)",
            if helpers == 1 {
                "1 helper process".to_owned()
            } else {
                format!("{helpers} helper processes")
            }
        )
    })
}

fn processes_text(killed: &[&ProcessNote]) -> String {
    let mut named: Vec<String> = killed
        .iter()
        .take(PROCESSES_NAMED)
        .map(|p| format!("{} {}", p.pid, p.command))
        .collect();
    if killed.len() > PROCESSES_NAMED {
        named.push(format!("and {} more", killed.len() - PROCESSES_NAMED));
    }
    named.join("; ")
}

fn dirty_text(note: &DirtyNote) -> String {
    let mut what = Vec::new();
    if note.dirty > 0 {
        what.push(plural(note.dirty as usize, "uncommitted change"));
    }
    if note.untracked > 0 {
        what.push(plural(note.untracked as usize, "untracked file"));
    }
    if note.unmerged > 0 {
        what.push(plural(note.unmerged as usize, "commit") + " not merged");
    }
    format!(
        "{}{} has {}",
        note.path.display(),
        note.branch
            .as_deref()
            .map_or_else(String::new, |b| format!(" (branch {b})")),
        what.join(", ")
    )
}

/// What ending a session leaves behind, in words: the processes that go
/// with it, and the work left in its directory (never touched).
fn loss_text(killed: &[ProcessNote], dirty: Option<&DirtyNote>) -> String {
    let mut said = Vec::new();
    let work = work_of(killed);
    if !work.is_empty() {
        said.push(format!(
            "processes in its terminal that end with it: {}",
            processes_text(&work)
        ));
    }
    if let Some(note) = dirty {
        said.push(format!(
            "{}: ending never touches it, so a successor can pick it up",
            dirty_text(note)
        ));
    }
    let mut text = if said.is_empty() {
        "nothing I can see (a process that left the agent, reparented away from it, cannot be seen)"
            .to_owned()
    } else {
        said.join("; ")
    };
    if let Some(helpers) = helpers_text(killed) {
        text.push_str(&format!("; besides, {helpers}"));
    }
    text
}

impl Hub {
    /// Session `to` (name, id, or id prefix), as the tools find it.
    pub(super) fn session_named(&self, to: &str) -> Result<SessionRecord, HubError> {
        let store = self.store();
        crate::script::find_by(
            store.sessions(),
            |s| (s.id.as_str(), s.name.as_str()),
            to,
            "session",
        )
        .map_err(StateError::Invalid)?
        .cloned()
        .ok_or_else(|| {
            StateError::Invalid(format!(
                "no session matches {:?} (list_sessions names them)",
                to.trim()
            ))
            .into()
        })
    }

    /// Who controls session `id` (see [`effective_owner`]).
    pub fn session_owner(&self, id: &str) -> Result<Option<String>, HubError> {
        let store = self.store();
        let record = store.session(id)?;
        Ok(effective_owner(record, store.sessions()))
    }

    /// `owner` in words, for `asker`.
    fn owner_words(&self, owner: Option<&str>, asker: &str) -> String {
        match owner {
            None => "the operator started it".to_owned(),
            Some(id) if id == asker => "it is yours".to_owned(),
            Some(id) => match self.store().session(id) {
                Ok(s) => format!("its owner is {} (id {})", s.name, s.id),
                Err(_) => format!("its owner was a removed session (id {id})"),
            },
        }
    }

    fn session_name(&self, id: &str) -> String {
        self.store()
            .session(id)
            .map_or_else(|_| format!("a removed session ({id})"), |s| s.name.clone())
    }

    /// Tell session `id`'s agent `text` (now, or once its turn is done);
    /// one no longer running is not told.
    pub(super) fn tell(&self, id: &str, text: &str) {
        let record = self.store().session(id).cloned();
        if let Ok(record) = record {
            self.tell_starter(&record, text);
        }
    }

    fn record_both(&self, target: &str, by: Option<&str>, what: What, on_caller: What) {
        let now = now_ms();
        self.telemetry().record(target, vec![Draft::new(now, what)]);
        if let Some(by) = by.filter(|b| *b != target) {
            self.telemetry()
                .record(by, vec![Draft::new(now, on_caller)]);
        }
    }

    /// The processes other than its agent in session `record`'s terminal,
    /// and the work in its directory: what ending it would take or leave.
    fn end_losses(&self, record: &SessionRecord) -> (Vec<ProcessNote>, Option<DirtyNote>) {
        let killed = self.end_processes(record);
        let dirty = match &record.worktree {
            Some(worktree) => git::removal_check(worktree).ok().map(|c| DirtyNote {
                path: c.path,
                branch: Some(c.branch),
                dirty: c.dirty,
                untracked: c.untracked,
                unmerged: c.unmerged,
            }),
            None if record.cwd.is_dir() => git::status(&record.cwd, None).ok().map(|s| DirtyNote {
                path: record.cwd.clone(),
                branch: s.branch,
                dirty: s.dirty,
                untracked: s.untracked,
                unmerged: 0,
            }),
            None => None,
        }
        .filter(|n| n.dirty + n.untracked + n.unmerged > 0);
        (killed, dirty)
    }

    /// The processes other than its agent in session `record`'s terminal
    /// or started by it (its Bash tool runs commands in sessions of their
    /// own), each marked when it is the agent's own helper.
    fn end_processes(&self, record: &SessionRecord) -> Vec<ProcessNote> {
        self.live_session(&record.id)
            .map(|pty| {
                let leader = pty.pid();
                let codex = record.kind == crate::agent::SessionKind::Codex;
                let full = |pid: u32| {
                    std::fs::read(format!("/proc/{pid}/cmdline"))
                        .map(|raw| String::from_utf8_lossy(&raw).replace('\0', " "))
                        .unwrap_or_default()
                };
                let mut pids = pty.session_processes();
                if let Some(leader) = leader {
                    pids.extend(descendants(leader));
                }
                pids.sort_unstable();
                pids.dedup();
                pids.into_iter()
                    .filter(|p| Some(*p) != leader)
                    .map(|pid| ProcessNote {
                        pid,
                        command: command_of(pid),
                        helper: leader.is_some_and(|l| is_helper(pid, l, codex, &parent_of, &full)),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Work session `record`'s agent started that still runs (not its
    /// own helpers): what it is, else `None`.
    pub(super) fn work_running(&self, record: &SessionRecord) -> Option<String> {
        let processes = self.end_processes(record);
        let work = work_of(&processes);
        let first = work.first()?;
        let named = format!("{} {}", first.pid, one_line(&first.command, 60));
        Some(match work.len() {
            1 => format!("a process it started still runs: {named}"),
            n => format!("{n} processes it started still run: {named}, ..."),
        })
    }

    /// Why agent session `from` may not end `target` at all.
    fn check_end(from: &SessionRecord, target: &SessionRecord) -> Result<(), HubError> {
        if !from.runs_agent() {
            return Err(refused(format!(
                "{} is not an agent session: only an agent ends sessions this way",
                from.name
            )));
        }
        if target.id == from.id {
            return Err(refused(
                "you cannot end your own session: finish your turn, or hand off",
            ));
        }
        if !target.runs_agent() || target.state != SessionState::Running {
            return Err(refused(format!(
                "{} is not a running agent session",
                target.name
            )));
        }
        if let Some(ending) = &target.ending {
            return Err(StateError::Conflict(format!(
                "{} is ending already (request {})",
                target.name, ending.id
            ))
            .into());
        }
        Ok(())
    }

    /// The final prompt as `target` reads it: under a line naming the
    /// sender, and one saying it is the last.
    fn final_text(by: &SessionRecord, end: &str, prompt: &str) -> Result<String, HubError> {
        let from = crate::agent_handoff::header(
            &crate::agent_handoff::Provenance {
                session_id: by.id.clone(),
                session: by.name.clone(),
                kind: by.kind,
                branch: by.worktree.as_ref().map(|w| w.branch.clone()),
                machine: None,
            },
            by.cwd.file_name().and_then(|n| n.to_str()),
            "a message from that agent session",
        );
        Ok(crate::queue::validate_text(&format!(
            "{from}\n\nThis is your final prompt: once this turn is over and nothing runs, the \
             flywheel hub ends this session (end request {end}). Wrap up as it asks; do not start \
             new work.\n\n{}",
            prompt.trim()
        ))?)
    }

    /// Agent session `caller` ends another (the `end_session` tool): at
    /// once when it owns it, else kept for the operator's approval. What
    /// to tell the agent.
    pub fn agent_end(self: &Arc<Self>, caller: &str, end: AgentEnd) -> Result<String, HubError> {
        let from = self.store().session(caller)?.clone();
        let target = self.session_named(&end.to)?;
        Self::check_end(&from, &target)?;
        if end.dry_run {
            let list = self.close_checklist(&target.id)?;
            return Ok(format!(
                "Close checklist for {} (nothing ended):\n{}\nTo end it with cleanups, call \
                 end_session again with close (worktree remove, build clean, report).",
                target.name,
                list.text()
            ));
        }
        let close = self.close_choices(&target, end.close)?;
        let final_prompt = trimmed(end.final_prompt.as_deref());
        match end.mode {
            EndMode::Force => return Err(refused(FORCE_REFUSED)),
            EndMode::Graceful if final_prompt.is_none() => {
                return Err(refused(
                    "a graceful end needs final_prompt: its last prompt (wrap up, commit, \
                     report); or use mode at_rest",
                ));
            }
            EndMode::AtRest if final_prompt.is_some() => {
                return Err(refused(
                    "at_rest takes no final_prompt: use mode graceful to send one",
                ));
            }
            _ => {}
        }
        let wait = wait_of(end.wait_minutes)?;
        let owner = self.session_owner(&target.id)?;
        if owner.as_deref() != Some(caller) {
            return self.ask_end(
                &from,
                &target,
                PendingEnd {
                    id: new_id(),
                    target: target.id.clone(),
                    mode: end.mode,
                    final_prompt,
                    wait_minutes: Some(wait),
                    replace: None,
                    why: ApprovalWhy::NotOwner,
                    created_at: Utc::now(),
                    close,
                },
                owner.as_deref(),
            );
        }
        self.begin_end(
            &target,
            &from,
            end.mode,
            final_prompt.as_deref(),
            wait,
            None,
            close,
        )
    }

    /// Agent session `caller` replaces another (the `replace_session`
    /// tool): the old one winds down, its successor starts, then it ends.
    pub fn agent_replace(
        self: &Arc<Self>,
        caller: &str,
        replace: AgentReplace,
    ) -> Result<String, HubError> {
        let from = self.store().session(caller)?.clone();
        let target = self.session_named(&replace.to)?;
        Self::check_end(&from, &target)?;
        let final_prompt = trimmed(Some(&replace.final_prompt)).ok_or_else(|| {
            refused("give final_prompt: the old session's last prompt (wrap up, commit, report)")
        })?;
        let name = trimmed(Some(&replace.name))
            .ok_or_else(|| refused("give the new session a short name"))?;
        let name = validate_name(&name)?;
        let prompt = crate::agent::validate_prompt(Some(&replace.prompt))
            .map_err(StateError::Invalid)?
            .ok_or_else(|| refused("give the new session's whole first prompt"))?;
        let workspace = self.workspace(&target.workspace_id)?;
        let folder = replace
            .folder
            .as_deref()
            .map(|f| self.workspace_folder(&workspace, f))
            .transpose()?;
        let worktree = match replace.worktree {
            Some(w) => Some(WorktreeRequest {
                task: trimmed(Some(&w.task))
                    .ok_or_else(|| refused("a worktree needs a task name"))?,
                base: trimmed(w.base.as_deref()),
            }),
            None => None,
        };
        let wait = wait_of(replace.wait_minutes)?;
        let policy = self.start_policy(&from);
        if policy == StartPolicy::Off {
            return Err(refused(format!(
                "starting sessions is off for {} (its start-sessions setting, or hub.toml \
                 agent_starts.policy): a replacement starts one; ask the operator",
                from.name
            )));
        }
        // A start that could not run now is refused now.
        self.check_start_limits(&from.id)?;
        let successor = Successor {
            name,
            model: trimmed(replace.model.as_deref()),
            prompt,
            folder,
            worktree,
        };
        // Resolved now, so a bad directory is refused now.
        self.start_request(&target, &successor_start(&target, &successor))?;
        let owner = self.session_owner(&target.id)?;
        let owns = owner.as_deref() == Some(caller);
        let why = match (owns, policy == StartPolicy::Ask) {
            (true, false) => None,
            (false, false) => Some(ApprovalWhy::NotOwner),
            (true, true) => Some(ApprovalWhy::StartPolicyAsk),
            (false, true) => Some(ApprovalWhy::Both),
        };
        if let Some(why) = why {
            return self.ask_end(
                &from,
                &target,
                PendingEnd {
                    id: new_id(),
                    target: target.id.clone(),
                    mode: EndMode::Graceful,
                    final_prompt: Some(final_prompt),
                    wait_minutes: Some(wait),
                    replace: Some(successor),
                    why,
                    created_at: Utc::now(),
                    close: None,
                },
                owner.as_deref(),
            );
        }
        self.begin_end(
            &target,
            &from,
            EndMode::Graceful,
            Some(&final_prompt),
            wait,
            Some(successor),
            None,
        )
    }

    /// Keep `pending` (asked by `from`) for the operator.
    fn ask_end(
        &self,
        from: &SessionRecord,
        target: &SessionRecord,
        pending: PendingEnd,
        owner: Option<&str>,
    ) -> Result<String, HubError> {
        let why = match pending.why {
            ApprovalWhy::NotOwner => format!(
                "{} is not yours to end ({})",
                target.name,
                self.owner_words(owner, &from.id)
            ),
            ApprovalWhy::StartPolicyAsk => {
                "your start-sessions setting asks the operator before you start a session"
                    .to_owned()
            }
            ApprovalWhy::Both => format!(
                "{} is not yours to end ({}), and your start-sessions setting asks the operator \
                 before you start a session",
                target.name,
                self.owner_words(owner, &from.id)
            ),
        };
        let id = pending.id.clone();
        let kind = request_kind(&pending);
        let (killed, dirty) = self.end_losses(target);
        self.store().add_end_request(&from.id, pending)?;
        self.emit(HubEvent::Sessions);
        Ok(format!(
            "Waiting for the operator's approval ({kind} request {id}): {why}. You get a message \
             when they approve or decline it. Keep working. If it goes ahead, what I can see \
             would be lost: {}.",
            loss_text(&killed, dirty.as_ref())
        ))
    }

    /// Start ending `target` for `by` (an agent; its request approved, or
    /// it owns the target): set its queue aside, queue the final prompt,
    /// and keep the end on its record. What to tell the agent.
    #[allow(clippy::too_many_arguments)]
    fn begin_end(
        &self,
        target: &SessionRecord,
        by: &SessionRecord,
        mode: EndMode,
        final_prompt: Option<&str>,
        wait: u32,
        replace: Option<Successor>,
        close: Option<crate::close::CloseChoices>,
    ) -> Result<String, HubError> {
        let id = new_id();
        let now = Utc::now();
        let final_text = final_prompt
            .map(|p| Self::final_text(by, &id, p))
            .transpose()?;
        let (killed, dirty) = self.end_losses(target);
        let replacing = replace.as_ref().map(|s| s.name.clone());
        let (dropped, paused) = {
            let mut store = self.store();
            let current = store.session(&target.id)?.clone();
            if let Some(ending) = &current.ending {
                return Err(StateError::Conflict(format!(
                    "{} is ending already (request {})",
                    current.name, ending.id
                ))
                .into());
            }
            let by_id = by.id.clone();
            let (held, final_item, paused) = store.update_queue(&target.id, |q| {
                let held: Vec<QueueItem> = q
                    .items
                    .iter()
                    .filter(|i| matches!(i.state, ItemState::Pending))
                    .cloned()
                    .collect();
                q.items.retain(|i| !matches!(i.state, ItemState::Pending));
                let final_item = final_text.map(|text| {
                    let item = QueueItem {
                        id: new_id(),
                        text,
                        when: When::Done,
                        added_at: now,
                        blocked_at: None,
                        from: Some(by_id),
                        priority: true,
                        watch: None,
                        topic: None,
                        state: ItemState::Pending,
                    };
                    let id = item.id.clone();
                    q.insert_pending(item);
                    id
                });
                (held, final_item, q.paused)
            })?;
            let dropped: Vec<DroppedItem> = held
                .iter()
                .map(|i| DroppedItem {
                    id: i.id.clone(),
                    from: i.from.clone(),
                    preview: preview(i),
                })
                .collect();
            let ending = Ending {
                id: id.clone(),
                by: Actor::Session(by.id.clone()),
                mode,
                final_item: final_item.clone(),
                final_at: None,
                phase: if final_item.is_some() {
                    EndPhase::AwaitFinal
                } else {
                    EndPhase::Settling
                },
                deadline: now + chrono::Duration::minutes(i64::from(wait)),
                dropped: dropped.clone(),
                held: held.clone(),
                replace,
                successor: None,
                requested_at: now,
                close,
            };
            if let Err(e) = store.update_session(&target.id, |r| r.ending = Some(ending)) {
                // Put the queue back as it was.
                let _ = store.update_queue(&target.id, |q| {
                    q.items.retain(|i| Some(&i.id) != final_item.as_ref());
                    for item in held {
                        q.insert_pending(item);
                    }
                });
                return Err(e.into());
            }
            (dropped, paused)
        };
        eprintln!(
            "fw hub: session {}: ending ({}, request {id}, by {})",
            target.id,
            mode.as_str(),
            by.name
        );
        self.queue_changed();
        let requested = |target_id: Option<String>| What::EndRequested {
            by: by.id.clone(),
            mode: mode.as_str().to_owned(),
            dropped: u32::try_from(dropped.len()).unwrap_or(u32::MAX),
            target: target_id,
        };
        self.record_both(
            &target.id,
            Some(&by.id),
            requested(None),
            requested(Some(target.id.clone())),
        );
        for item in &dropped {
            if let Some(sender) = item.from.as_deref().filter(|s| *s != by.id) {
                self.tell(
                    sender,
                    &format!(
                        "From the flywheel hub: your message {} queued for {} was set aside: {} is \
                         ending (asked by {}). It is delivered after all if the end is called off; \
                         else it is dropped.",
                        item.id, target.name, target.name, by.name
                    ),
                );
            }
        }
        let mut said = vec![match &replacing {
            Some(name) => format!(
                "Replacing {} (id {}) with {name}: scheduled, request {id}. Its final prompt goes \
                 at its next turn boundary; once that turn is over and nothing runs, {name} \
                 starts, then {} ends.",
                target.name, target.id, target.name
            ),
            None if mode == EndMode::AtRest => format!(
                "Ending {} (id {}): scheduled, request {id}. It ends at its next rest, once no \
                 tool call or subagent runs.",
                target.name, target.id
            ),
            None => format!(
                "Ending {} (id {}): scheduled, request {id}. Its final prompt goes at its next \
                 turn boundary; once that turn is over and nothing runs, it ends.",
                target.name, target.id
            ),
        }];
        said.push(format!(
            "If that has not happened in {wait} minutes, nothing changes. The outcome arrives as \
             a message."
        ));
        if paused && final_prompt.is_some() {
            said.push(
                "Its queue is paused: the final prompt goes when the operator resumes it."
                    .to_owned(),
            );
        }
        if !dropped.is_empty() {
            let listed: Vec<String> = dropped
                .iter()
                .map(|d| {
                    let sender = match &d.from {
                        Some(f) if *f == by.id => "you".to_owned(),
                        Some(f) => self.session_name(f),
                        None => "the operator".to_owned(),
                    };
                    format!("{} from {sender}: {}", d.id, d.preview)
                })
                .collect();
            said.push(format!(
                "Set aside {} it had queued (back if the end is called off): {}.",
                plural(dropped.len(), "message"),
                listed.join("; ")
            ));
        }
        said.push(format!(
            "What I can see would be lost: {}.",
            loss_text(&killed, dirty.as_ref())
        ));
        Ok(said.join(" "))
    }

    /// Agent session `caller` gives control of session `to` to session
    /// `new_owner` (the `give_session` tool): only its owner may.
    pub fn give_session(
        &self,
        caller: &str,
        to: &str,
        new_owner: &str,
    ) -> Result<String, HubError> {
        let from = self.store().session(caller)?.clone();
        if !from.runs_agent() {
            return Err(refused(format!("{} is not an agent session", from.name)));
        }
        let target = self.session_named(to)?;
        let next = self.session_named(new_owner)?;
        let owner = self.session_owner(&target.id)?;
        if owner.as_deref() != Some(caller) {
            return Err(refused(format!(
                "only its owner gives {} away ({})",
                target.name,
                self.owner_words(owner.as_deref(), caller)
            )));
        }
        if next.id == caller {
            return Err(refused(format!("{} is yours already", target.name)));
        }
        if next.id == target.id {
            return Err(refused("a session cannot own itself"));
        }
        if !running_agent(&next) {
            return Err(refused(format!(
                "{} is not a running agent session",
                next.name
            )));
        }
        let previous = target.owner.clone();
        let next_id = next.id.clone();
        self.store()
            .update_session(&target.id, |r| r.owner = Some(next_id))?;
        self.refresh_parents();
        self.emit(HubEvent::Sessions);
        self.telemetry().record(
            &target.id,
            vec![Draft::new(
                now_ms(),
                What::OwnerChanged {
                    from: previous.or(Some(caller.to_owned())),
                    to: next.id.clone(),
                    by: caller.to_owned(),
                },
            )],
        );
        self.tell(
            &next.id,
            &format!(
                "From the flywheel hub: {} (id {}) gave you control of {} (id {}). You may end or \
                 replace it (end_session, replace_session), or give it on (give_session); check \
                 on it with session_status.",
                from.name, from.id, target.name, target.id
            ),
        );
        Ok(format!(
            "{} (id {}) is now {}'s: it may end, replace, or give it; you no longer may.",
            target.name, target.id, next.name
        ))
    }

    /// The operator puts session `id` under agent session `owner` (the
    /// sidebar's drag onto a lead), or back under whoever opened it
    /// (`None`). Refused: a session under itself or under one of its own
    /// (a loop), or an owner that is not a running agent. The new owner is
    /// told and watches its children. The session, as it now is.
    pub fn set_owner(&self, id: &str, owner: Option<&str>) -> Result<SessionView, HubError> {
        let target = self.store().session(id)?.clone();
        let next = match owner.map(str::trim).filter(|o| !o.is_empty()) {
            None => None,
            Some(wanted) => {
                let next = self.session_named(wanted)?;
                if next.id == target.id {
                    return Err(refused("a session cannot own itself"));
                }
                if !running_agent(&next) {
                    return Err(refused(format!(
                        "{} is not a running agent session",
                        next.name
                    )));
                }
                // Not under one of its own: walk up from the new owner.
                let store = self.store();
                let sessions = store.sessions();
                let mut at = Some(next.id.clone());
                let mut seen = HashSet::new();
                while let Some(here) = at {
                    if here == target.id {
                        return Err(refused(format!(
                            "{} is under {} already: that would be a loop",
                            next.name, target.name
                        )));
                    }
                    if !seen.insert(here.clone()) {
                        break;
                    }
                    at = sessions
                        .iter()
                        .find(|s| s.id == here)
                        .and_then(|s| effective_parent(s, sessions));
                }
                Some(next)
            }
        };
        let previous = target.owner.clone();
        let next_id = next.as_ref().map(|n| n.id.clone());
        self.store()
            .update_session(&target.id, |r| r.owner = next_id)?;
        self.refresh_parents();
        self.emit(HubEvent::Sessions);
        let now_under = {
            let store = self.store();
            store
                .session(&target.id)
                .ok()
                .and_then(|r| effective_owner(r, store.sessions()))
        };
        self.telemetry().record(
            &target.id,
            vec![Draft::new(
                now_ms(),
                What::OwnerChanged {
                    from: previous,
                    to: now_under.clone().unwrap_or_default(),
                    by: "operator".to_owned(),
                },
            )],
        );
        if let Some(next) = &next {
            self.tell(
                &next.id,
                &format!(
                    "From the flywheel hub: the operator put {} (id {}) under you. Its report \
                     (its closing message) reaches you through your children watch; you may \
                     end, replace, or give it; check on it with session_status.",
                    target.name, target.id
                ),
            );
            self.ensure_children_watch(&next.id);
        }
        self.session(&target.id)
    }

    /// The operator approves session `id`'s pending end `end`: it starts
    /// as if the asker owned the target; the asker is told.
    pub fn approve_end(self: &Arc<Self>, id: &str, end: &str) -> Result<SessionView, HubError> {
        let from = self.store().session(id)?.clone();
        let pending = from
            .end_requests
            .iter()
            .find(|e| e.id == end)
            .cloned()
            .ok_or_else(|| {
                StateError::NotFound(format!("{} has no pending end {end}", from.name))
            })?;
        // Taken before it starts, so two clicks start one end; put back
        // when it cannot start for a reason that may pass.
        let taken = self.store().remove_end_request(id, end)?;
        let target = match self.store().session(&pending.target).cloned() {
            Ok(target) => target,
            Err(_) => {
                self.emit(HubEvent::Sessions);
                self.tell(
                    &from.id,
                    &format!(
                        "From the flywheel hub: your {} request {end} is dropped: its session \
                         was removed.",
                        request_kind(&pending)
                    ),
                );
                return Err(StateError::NotFound(format!(
                    "the session end request {end} names (it was removed)"
                ))
                .into());
            }
        };
        let started = (|| {
            if target.state != SessionState::Running {
                return Err(refused(format!("{} is not running", target.name)));
            }
            if let Some(ending) = &target.ending {
                return Err(StateError::Conflict(format!(
                    "{} is ending already (request {})",
                    target.name, ending.id
                ))
                .into());
            }
            if pending.replace.is_some() {
                self.check_start_limits(&from.id)?;
            }
            self.begin_end(
                &target,
                &from,
                pending.mode,
                pending.final_prompt.as_deref(),
                pending.wait_minutes.unwrap_or(DEFAULT_WAIT_MINUTES),
                pending.replace.clone(),
                pending.close.clone(),
            )
        })();
        match started {
            Ok(said) => {
                // As it is now: a session at rest may be gone by the time
                // the decision is kept and the asker told.
                let record = self.store().session(&target.id)?.clone();
                self.emit(HubEvent::Sessions);
                self.record_decision(super::holds::approval_decision(
                    super::holds::end_hold(&from, &taken, &target.name),
                    "approved",
                ));
                self.tell(
                    &from.id,
                    &format!(
                        "From the flywheel hub: the operator approved your {} request {end}. {said}",
                        request_kind(&pending)
                    ),
                );
                Ok(self.view(record))
            }
            Err(failed) => {
                let _ = self.store().add_end_request(id, taken);
                self.emit(HubEvent::Sessions);
                Err(failed)
            }
        }
    }

    /// The operator declines session `id`'s pending end `end`; the asker
    /// is told.
    pub fn decline_end(&self, id: &str, end: &str) -> Result<(), HubError> {
        let removed = self.store().remove_end_request(id, end)?;
        self.emit(HubEvent::Sessions);
        let name = self.session_name(&removed.target);
        // Read, then let go of the store: recording takes it again.
        let from = self.store().session(id).cloned();
        if let Ok(from) = from {
            self.record_decision(super::holds::approval_decision(
                super::holds::end_hold(&from, &removed, &name),
                "declined",
            ));
        }
        self.tell(
            id,
            &format!(
                "From the flywheel hub: the operator declined your {} request {end}; {name} keeps \
                 running{}.",
                request_kind(&removed),
                if removed.replace.is_some() {
                    " and its replacement was not started"
                } else {
                    ""
                }
            ),
        );
        Ok(())
    }

    /// The operator calls off session `id`'s end in progress.
    pub fn cancel_ending(&self, id: &str) -> Result<SessionView, HubError> {
        let record = self.store().session(id)?.clone();
        if record.ending.is_none() {
            return Err(
                StateError::NotFound(format!("an end of {} in progress", record.name)).into(),
            );
        }
        self.call_off(&record, "the operator called it off");
        let record = self.store().session(id)?.clone();
        Ok(self.view(record))
    }

    /// The operator hangs session `id` up at once (`fw hub kill`, the pane
    /// menu): recorded as ended by the operator, with what went with it.
    /// An end in progress is over; its asker is told.
    pub fn operator_kill(&self, id: &str) -> Result<(), HubError> {
        let record = self.store().session(id)?.clone();
        let (killed, dirty) = self.end_losses(&record);
        let inside: Vec<u32> = self
            .live_session(id)
            .map(|pty| pty.session_processes())
            .unwrap_or_default();
        let outside: Vec<(u32, String)> = killed
            .iter()
            .filter(|p| !inside.contains(&p.pid))
            .map(|p| (p.pid, p.command.clone()))
            .collect();
        self.kill_session(id)?;
        hang_up_outside(outside);
        let end = EndRecord {
            by: Actor::Operator,
            at: Utc::now(),
            mode: EndMode::Force,
            killed,
            dirty,
            successor: None,
            by_name: None,
            successor_name: None,
            close: None,
        };
        let ending = record.ending.clone();
        let saved = end.clone();
        if let Err(e) = self.store().update_session(id, |r| {
            r.ending = None;
            r.ended = Some(saved);
        }) {
            eprintln!("fw hub: session {id}: killed, but how is not recorded: {e}");
        }
        self.emit(HubEvent::Sessions);
        self.telemetry()
            .record(id, vec![Draft::new(now_ms(), ended_event(&end, None))]);
        if let Some(ending) = ending
            && let Some(by) = ending.by.session()
        {
            self.tell(
                by,
                &format!(
                    "From the flywheel hub: the operator killed {} (id {}) while your end request \
                     {} was in progress{}.",
                    record.name,
                    record.id,
                    ending.id,
                    if ending.replace.is_some() && ending.successor.is_none() {
                        "; its replacement was not started"
                    } else {
                        ""
                    }
                ),
            );
        }
        Ok(())
    }

    /// One look at every end in progress; whether there is any (look
    /// again in a tick). Runs on the queue's thread, after its tick.
    pub(crate) fn endings_tick(self: &Arc<Self>) -> bool {
        let ending: Vec<SessionRecord> = self
            .store()
            .sessions()
            .iter()
            .filter(|s| s.ending.is_some())
            .cloned()
            .collect();
        for record in &ending {
            self.drive_end(record);
        }
        !ending.is_empty()
    }

    fn set_ending(&self, id: &str, edit: impl FnOnce(&mut Ending)) {
        let saved = self.store().update_session(id, |r| {
            if let Some(ending) = r.ending.as_mut() {
                edit(ending);
            }
        });
        if let Err(e) = saved {
            eprintln!("fw hub: session {id}: ending: not recorded: {e}");
        }
    }

    /// Queue item `item` of session `id` was just delivered, its paste
    /// begun at `at`: when it is the session's final prompt, its turn is
    /// the one that starts from then on.
    pub(crate) fn final_prompt_sent(&self, id: &str, item: &str, at: chrono::DateTime<Utc>) {
        let ending = self.store().session(id).ok().and_then(|r| r.ending.clone());
        if ending.is_some_and(|e| {
            e.phase == EndPhase::AwaitFinal && e.final_item.as_deref() == Some(item)
        }) {
            self.set_ending(id, |e| {
                e.phase = EndPhase::FinalTurn;
                e.final_at = Some(at);
            });
        }
    }

    fn drive_end(self: &Arc<Self>, record: &SessionRecord) {
        let Some(ending) = record.ending.clone() else {
            return;
        };
        let now = Utc::now();
        match &record.state {
            SessionState::Exited { .. } => {
                // It went on its own (or was hung up some other way).
                let mut ending = ending;
                if ending.replace.is_some() && ending.successor.is_none() {
                    match self.start_successor(record, &ending) {
                        Ok(id) => ending.successor = Some(id),
                        Err(e) => {
                            if let Some(by) = ending.by.session() {
                                self.tell(
                                    by,
                                    &format!(
                                        "From the flywheel hub: {} exited on its own during your \
                                         end request {}, and its replacement did not start: {e}",
                                        record.name, ending.id
                                    ),
                                );
                            }
                        }
                    }
                }
                self.finish_end(record, &ending, false);
                return;
            }
            SessionState::Stopped { .. } if now < ending.deadline => return,
            _ => {}
        }
        if now >= ending.deadline {
            let minutes = (ending.deadline - ending.requested_at).num_minutes();
            self.call_off(
                record,
                &format!(
                    "it was still {} when its {minutes} minutes ran out",
                    ending.phase.words()
                ),
            );
            return;
        }
        match ending.phase {
            EndPhase::AwaitFinal => {
                let state = self.store().queue(&record.id).and_then(|q| {
                    q.items
                        .iter()
                        .find(|i| Some(&i.id) == ending.final_item.as_ref())
                        .map(|i| i.state.clone())
                });
                match state {
                    Some(ItemState::Delivered { delivered_at, .. }) => {
                        self.set_ending(&record.id, |e| {
                            e.phase = EndPhase::FinalTurn;
                            e.final_at = Some(delivered_at);
                        });
                        self.emit(HubEvent::Sessions);
                    }
                    Some(_) => {}
                    None => self.call_off(record, "its final prompt was taken out of its queue"),
                }
            }
            EndPhase::FinalTurn => {
                let done = self.agent_status(&record.id).is_some_and(|s| {
                    matches!(s.status, AgentStatus::Done | AgentStatus::Idle)
                        && ending.final_at.is_some_and(|at| s.since >= at)
                });
                if done {
                    self.set_ending(&record.id, |e| e.phase = EndPhase::Settling);
                    self.emit(HubEvent::Sessions);
                }
            }
            EndPhase::Settling => {
                if self.end_ready(record).is_err() {
                    return;
                }
                let mut ending = ending;
                if ending.replace.is_some() && ending.successor.is_none() {
                    match self.start_successor(record, &ending) {
                        Ok(id) => {
                            let started = id.clone();
                            self.set_ending(&record.id, |e| e.successor = Some(started));
                            ending.successor = Some(id);
                        }
                        Err(e) => {
                            self.call_off(record, &format!("its successor did not start: {e}"));
                            return;
                        }
                    }
                }
                // Looked at again just before acting, under the paste turn
                // (nothing types into it meanwhile).
                let runtime = self.queue_runtime();
                let turn = runtime.paste_turn();
                let Ok(fresh) = self.store().session(&record.id).cloned() else {
                    return;
                };
                if self.end_ready(&fresh).is_err() {
                    return;
                }
                let (killed, dirty) = self.end_losses(&fresh);
                // What the agent started outside its terminal's session
                // (its Bash tool's commands) goes with it too.
                let inside: Vec<u32> = self
                    .live_session(&fresh.id)
                    .map(|pty| pty.session_processes())
                    .unwrap_or_default();
                let outside: Vec<(u32, String)> = killed
                    .iter()
                    .filter(|p| !inside.contains(&p.pid))
                    .map(|p| (p.pid, p.command.clone()))
                    .collect();
                let end = self.record_end(&fresh, &ending, killed, dirty);
                if let Err(e) = self.kill_session(&fresh.id) {
                    eprintln!("fw hub: session {}: ending: {e}", fresh.id);
                }
                hang_up_outside(outside);
                drop(turn);
                self.announce_end(&fresh, &ending, &end, true);
            }
        }
    }

    /// Why session `record` is not at rest to be ended now: the queue's
    /// rest rules, nothing queued, and no tool call or subagent open.
    fn end_ready(&self, record: &SessionRecord) -> Result<(), String> {
        self.at_rest(record, false)?;
        if self
            .store()
            .queue(&record.id)
            .is_some_and(|q| q.pending_count() > 0)
        {
            return Err("a queued prompt waits".to_owned());
        }
        Ok(())
    }

    /// Why agent session `record` is not at rest: the queue's rest rules
    /// (its status settled at done or idle, nobody using or typing into
    /// it, nothing unsent at its prompt), no tool call open, and no
    /// subagent running or about to wake it. `shown`: the state it shows
    /// must be done, idle, or failed too (not asking, background, or
    /// compacting).
    pub(super) fn at_rest(&self, record: &SessionRecord, shown: bool) -> Result<(), String> {
        let facts = self.rest_facts(record, &SessionQueue::default());
        match facts.presence {
            crate::queue::Presence::Ready => {}
            crate::queue::Presence::Starting(why) => return Err(why.to_owned()),
            crate::queue::Presence::Gone(why) => return Err(why),
        }
        let status = facts
            .status
            .ok_or_else(|| "no status reported yet".to_owned())?;
        if !matches!(status.status, AgentStatus::Done | AgentStatus::Idle) {
            return Err(format!("{:?}", status.status).to_lowercase());
        }
        if shown && !matches!(status.state, State::Done | State::Idle | State::Failed) {
            return Err(status.state.as_str().to_owned());
        }
        if (facts.now - status.since).to_std().unwrap_or_default() < SETTLE {
            return Err("just finished: settling".to_owned());
        }
        if let Some(owner) = facts.owner {
            return Err(format!("{owner} is using it"));
        }
        if facts.typed_ago.is_some_and(|ago| ago < facts.quiet) {
            return Err("someone is typing into it".to_owned());
        }
        if facts.draft > 0 {
            return Err("something typed into it is unsent".to_owned());
        }
        if self
            .store()
            .queue(&record.id)
            .and_then(|q| q.last_delivered_at)
            .is_some_and(|at| at >= status.since)
        {
            return Err("a prompt sent to it has not started yet".to_owned());
        }
        if let Some(snapshot) = self.telemetry().snapshot(&record.id) {
            if snapshot.open_tools > 0 {
                return Err("a tool call is open".to_owned());
            }
            // Claude reports `idle_prompt` (and the screen can read idle)
            // while a turn still waits on the model: only the turn's own
            // end says it is over. A turn quiet past [`OPEN_TURN_QUIET_MS`]
            // lost its end (a missed Stop) and no longer holds.
            if snapshot.status == "working" && now_ms() - snapshot.last_ts < OPEN_TURN_QUIET_MS {
                return Err("its turn is still open".to_owned());
            }
        }
        if let Some(detail) = self.telemetry().detail(None, &record.id) {
            let work = &detail.work;
            if work
                .iter()
                .any(|w| w.source == WorkSource::Subagent && w.status == WorkStatus::Active)
            {
                return Err("a subagent runs".to_owned());
            }
            // A subagent that ended after the last turn did (one run in
            // the background) wakes its agent for one more turn: Claude
            // submits its result as a prompt. Not at rest until then.
            let turn_end = work
                .iter()
                .filter(|w| w.source == WorkSource::Turn)
                .filter_map(|w| w.finished)
                .max();
            // A subagent cut short (its session ended, an Esc) wakes
            // nothing, and one not taken up within [`WAKE_WINDOW_MS`] never
            // will be.
            let now = now_ms();
            if work.iter().any(|w| {
                w.source == WorkSource::Subagent
                    && matches!(w.status, WorkStatus::Done | WorkStatus::Failed)
                    && w.finished.is_some_and(|f| {
                        turn_end.is_none_or(|end| f > end) && now - f < WAKE_WINDOW_MS
                    })
            }) {
                return Err("a background subagent finished: its agent takes it up".to_owned());
            }
        }
        Ok(())
    }

    /// Start the successor `ending` asks for, in `record`'s place: its id.
    fn start_successor(
        self: &Arc<Self>,
        record: &SessionRecord,
        ending: &Ending,
    ) -> Result<String, HubError> {
        let successor = ending
            .replace
            .as_ref()
            .ok_or_else(|| refused("no successor asked for"))?;
        let by = ending
            .by
            .session()
            .ok_or_else(|| refused("only an agent's end starts a successor"))?
            .to_owned();
        let mut request = self.start_request(record, &successor_start(record, successor))?;
        // What it takes over, said by the hub (its children pass to it as
        // the old one ends).
        if let (Some(prompt), Some(brief)) = (&request.prompt, self.handover_brief(&record.id)) {
            request.prompt = Some(format!("{}\n\n{brief}", prompt.trim_end()));
        }
        let view = self.start_for(&by, request)?;
        let new_id = view.record.id.clone();
        let (owner, continues) = (by.clone(), record.id.clone());
        self.store().update_session(&new_id, |r| {
            r.owner = Some(owner);
            r.continues = Some(continues);
        })?;
        self.refresh_parents();
        self.emit(HubEvent::Sessions);
        Ok(new_id)
    }

    /// Record how `record` ended (its end in progress is over); the set-
    /// aside messages are dropped for good.
    fn record_end(
        &self,
        record: &SessionRecord,
        ending: &Ending,
        killed: Vec<ProcessNote>,
        dirty: Option<DirtyNote>,
    ) -> EndRecord {
        let end = EndRecord {
            by: ending.by.clone(),
            at: Utc::now(),
            mode: ending.mode,
            killed,
            dirty,
            successor: ending.successor.clone(),
            by_name: ending.by.session().map(|id| self.session_name(id)),
            successor_name: ending.successor.as_deref().map(|id| self.session_name(id)),
            close: None,
        };
        let saved = end.clone();
        let final_item = ending.final_item.clone();
        let mut store = self.store();
        if let Err(e) = store.update_session(&record.id, |r| {
            r.ending = None;
            r.ended = Some(saved);
        }) {
            eprintln!(
                "fw hub: session {}: ended, but not recorded: {e}",
                record.id
            );
        }
        let _ = store.update_queue(&record.id, |q| {
            q.items.retain(|i| {
                !(matches!(i.state, ItemState::Pending) && Some(&i.id) == final_item.as_ref())
            });
        });
        drop(store);
        self.refresh_parents();
        end
    }

    /// Record the end of a session that exited before it was hung up.
    fn finish_end(self: &Arc<Self>, record: &SessionRecord, ending: &Ending, killed: bool) {
        let (_, dirty) = self.end_losses(record);
        let end = self.record_end(record, ending, Vec::new(), dirty);
        self.announce_end(record, ending, &end, killed);
    }

    /// Tell everyone `record` ended: its log, the asker's, clients, and
    /// the asker's agent.
    fn announce_end(
        self: &Arc<Self>,
        record: &SessionRecord,
        ending: &Ending,
        end: &EndRecord,
        killed: bool,
    ) {
        if let Some(close) = ending.close.clone().filter(|c| !c.is_empty()) {
            self.close_after_end(record, close, &ending.id, ending.by.session());
        }
        eprintln!(
            "fw hub: session {}: ended (request {}){}",
            record.id,
            ending.id,
            end.successor
                .as_deref()
                .map_or_else(String::new, |s| format!(", replaced by {s}"))
        );
        self.emit(HubEvent::Sessions);
        let by = ending.by.session();
        self.record_both(
            &record.id,
            by,
            ended_event(end, None),
            ended_event(end, Some(record.id.clone())),
        );
        if let Some(successor) = &end.successor {
            self.telemetry().record(
                &record.id,
                vec![Draft::new(
                    now_ms(),
                    What::Replaced {
                        successor: successor.clone(),
                    },
                )],
            );
        }
        let Some(by) = by else {
            return;
        };
        let head = match &end.successor {
            Some(successor) => format!(
                "{} (id {}) is replaced by {} (id {}): it started (opened by you, continuing {}'s \
                 work), then {} ended",
                record.name,
                record.id,
                self.session_name(successor),
                successor,
                record.name,
                record.name
            ),
            None if killed => format!("{} (id {}) ended", record.name, record.id),
            None => format!(
                "{} (id {}) exited on its own before the end was done",
                record.name, record.id
            ),
        };
        let mut said = format!("From the flywheel hub: {head} (end request {}).", ending.id);
        let work = work_of(&end.killed);
        if !work.is_empty() {
            said.push_str(&format!(
                " Processes that ended with it: {}.",
                processes_text(&work)
            ));
        }
        if let Some(dirty) = &end.dirty {
            said.push_str(&format!(" Left as it was: {}.", dirty_text(dirty)));
        }
        self.tell(by, &said);
    }

    /// Call off `record`'s end in progress: its set-aside messages go
    /// back, the final prompt (if not sent) is dropped, and the asker is
    /// told why.
    fn call_off(&self, record: &SessionRecord, why: &str) {
        let Some(ending) = record.ending.clone() else {
            return;
        };
        {
            let mut store = self.store();
            let final_item = ending.final_item.clone();
            let held = ending.held.clone();
            if let Err(e) = store.update_queue(&record.id, |q| {
                q.items.retain(|i| {
                    !(matches!(i.state, ItemState::Pending) && Some(&i.id) == final_item.as_ref())
                });
                for item in held {
                    q.insert_pending(item);
                }
            }) {
                eprintln!(
                    "fw hub: session {}: ending: queue not restored: {e}",
                    record.id
                );
            }
            if let Err(e) = store.update_session(&record.id, |r| r.ending = None) {
                eprintln!("fw hub: session {}: ending: not called off: {e}", record.id);
                return;
            }
        }
        eprintln!(
            "fw hub: session {}: end request {} called off: {why}",
            record.id, ending.id
        );
        self.queue_changed();
        let by = ending.by.session();
        self.record_both(
            &record.id,
            by,
            What::EndCancelled {
                why: why.to_owned(),
                target: None,
            },
            What::EndCancelled {
                why: why.to_owned(),
                target: Some(record.id.clone()),
            },
        );
        let Some(by) = by else {
            return;
        };
        let mut said = format!(
            "From the flywheel hub: the end of {} (id {}, request {}) was called off: {why}. It \
             keeps running",
            record.name, record.id, ending.id
        );
        if !ending.held.is_empty() {
            said.push_str(&format!(
                ", and the {} set aside are back in its queue",
                plural(ending.held.len(), "message")
            ));
        }
        said.push('.');
        match (&ending.replace, &ending.successor) {
            (Some(_), Some(started)) => said.push_str(&format!(
                " Its successor {} (id {started}) had started and keeps running.",
                self.session_name(started)
            )),
            (Some(s), None) => said.push_str(&format!(" {} was not started.", s.name)),
            _ => {}
        }
        if let Ok(snap) = self.agent_session_status(by, &record.id) {
            said.push_str(&format!("\n{snap}"));
        }
        self.tell(by, &said);
    }

    /// A session's end in progress, for its asker when it is removed.
    pub(super) fn ending_removed(&self, record: &SessionRecord) {
        let Some(ending) = &record.ending else {
            return;
        };
        if let Some(by) = ending.by.session() {
            self.tell(
                by,
                &format!(
                    "From the flywheel hub: the operator removed {} (id {}) while your end \
                     request {} was in progress.",
                    record.name, record.id, ending.id
                ),
            );
        }
    }

    /// Agent session `caller`'s view of the queued messages it sent to
    /// session `to` that wait (the `queued_messages` tool).
    pub fn agent_queued(&self, caller: &str, to: Option<&str>) -> Result<String, HubError> {
        self.store().session(caller)?;
        let Some(to) = to.map(str::trim).filter(|t| !t.is_empty()) else {
            return self.agent_queued_everywhere(caller);
        };
        let target = self.session_named(to)?;
        let queue = self.store().queue(&target.id).cloned().unwrap_or_default();
        let total = queue.pending_count();
        let now = now_ms();
        let mine: Vec<String> = queue
            .waiting()
            .enumerate()
            .filter(|(_, i)| {
                i.from.as_deref() == Some(caller) && matches!(i.state, ItemState::Pending)
            })
            .map(|(at, i)| {
                format!(
                    "  {} #{} {}{}sent {} ago: {}",
                    i.id,
                    at + 1,
                    if i.priority { "priority, " } else { "" },
                    i.topic
                        .as_deref()
                        .map(|t| format!("topic {t}, "))
                        .unwrap_or_default(),
                    span(now - i.added_at.timestamp_millis()),
                    preview(i)
                )
            })
            .collect();
        let mut said = if mine.is_empty() {
            format!(
                "None of your messages wait for {} ({} queued in all).",
                target.name,
                plural(total, "message")
            )
        } else {
            format!(
                "{} of yours wait{} for {} (id {}); {} queued in all (#: place in its queue):\n{}",
                plural(mine.len(), "message"),
                if mine.len() == 1 { "s" } else { "" },
                target.name,
                target.id,
                plural(total, "message"),
                mine.join("\n")
            )
        };
        if let Some(ending) = &target.ending {
            let held = ending
                .held
                .iter()
                .filter(|i| i.from.as_deref() == Some(caller))
                .count();
            said.push_str(&format!(
                "\n{} is ending (request {}){}.",
                target.name,
                ending.id,
                if held > 0 {
                    format!(
                        "; {} of yours set aside come back only if the end is called off",
                        plural(held, "message")
                    )
                } else {
                    String::new()
                }
            ));
        }
        Ok(said)
    }

    /// `caller`'s waiting messages in every session's queue, by session
    /// (the `queued_messages` tool without `to`).
    fn agent_queued_everywhere(&self, caller: &str) -> Result<String, HubError> {
        let now = now_ms();
        let mut lines = Vec::new();
        let mut count = 0;
        {
            let store = self.store();
            for (id, queue) in store.queues() {
                let mine: Vec<String> = queue
                    .waiting()
                    .enumerate()
                    .filter(|(_, i)| {
                        i.from.as_deref() == Some(caller) && matches!(i.state, ItemState::Pending)
                    })
                    .map(|(at, i)| {
                        format!(
                            "  {} #{} {}{}sent {} ago: {}",
                            i.id,
                            at + 1,
                            if i.priority { "priority, " } else { "" },
                            i.topic
                                .as_deref()
                                .map(|t| format!("topic {t}, "))
                                .unwrap_or_default(),
                            span(now - i.added_at.timestamp_millis()),
                            preview(i)
                        )
                    })
                    .collect();
                if mine.is_empty() {
                    continue;
                }
                count += mine.len();
                let name = store
                    .session(id)
                    .map_or_else(|_| id.clone(), |s| s.name.clone());
                lines.push(format!(
                    "{name} (id {id}), {} queued in all:",
                    plural(queue.pending_count(), "message")
                ));
                lines.extend(mine);
            }
        }
        Ok(if count == 0 {
            "None of your messages wait in any session's queue.".to_owned()
        } else {
            format!(
                "{} of yours wait{} (#: place in that queue):\n{}",
                plural(count, "message"),
                if count == 1 { "s" } else { "" },
                lines.join("\n")
            )
        })
    }

    /// Agent session `caller` takes back its waiting messages to session
    /// `to`: `item` (an id or id prefix), else all of them (the
    /// `cancel_message` tool). A message being sent stays.
    pub fn agent_cancel(
        &self,
        caller: &str,
        to: &str,
        item: Option<&str>,
    ) -> Result<String, HubError> {
        self.store().session(caller)?;
        let target = self.session_named(to)?;
        let wanted = trimmed(item);
        if let Some(r) = &wanted
            && r.len() < 4
        {
            return Err(refused(
                "name a message by its id (4+ characters) from queued_messages",
            ));
        }
        let mine = |i: &QueueItem| i.from.as_deref() == Some(caller);
        let (removed, sending, left) = self.store().update_queue(&target.id, |q| {
            let picked: Vec<String> = match &wanted {
                Some(r) => {
                    let found: Vec<&QueueItem> = q
                        .items
                        .iter()
                        .filter(|i| mine(i) && i.id.starts_with(r.as_str()))
                        .collect();
                    match found.as_slice() {
                        [] => {
                            return Err(StateError::NotFound(format!(
                                "a message of yours {r} in {}'s queue",
                                target.name
                            )));
                        }
                        [one] => match one.state {
                            ItemState::Pending => vec![one.id.clone()],
                            ItemState::Sending { .. } => {
                                return Err(StateError::Conflict(format!(
                                    "message {} is being delivered right now: it cannot be \
                                     cancelled",
                                    one.id
                                )));
                            }
                            ItemState::Delivered { .. } => {
                                return Err(StateError::Conflict(format!(
                                    "message {} was delivered already",
                                    one.id
                                )));
                            }
                        },
                        _ => {
                            return Err(StateError::Invalid(format!(
                                "{r} matches more than one of your messages: give more of its id"
                            )));
                        }
                    }
                }
                None => q
                    .items
                    .iter()
                    .filter(|i| mine(i) && matches!(i.state, ItemState::Pending))
                    .map(|i| i.id.clone())
                    .collect(),
            };
            let sending = q
                .items
                .iter()
                .filter(|i| mine(i) && matches!(i.state, ItemState::Sending { .. }))
                .count();
            q.items.retain(|i| !picked.contains(&i.id));
            if q.stalled
                .as_ref()
                .is_some_and(|s| s.item.as_ref().is_some_and(|i| picked.contains(i)))
                && q.head().is_none()
            {
                q.stalled = None;
            }
            Ok((picked.len(), sending, q.pending_count()))
        })??;
        if removed > 0 {
            self.queue_changed();
            self.telemetry().record(
                &target.id,
                vec![Draft::new(
                    now_ms(),
                    What::QueueCancelled {
                        by: caller.to_owned(),
                        items: u32::try_from(removed).unwrap_or(u32::MAX),
                    },
                )],
            );
        }
        let mut said = format!(
            "Cancelled {} of yours to {}; {} still queued there.",
            plural(removed, "message"),
            target.name,
            plural(left, "message")
        );
        if sending > 0 && wanted.is_none() {
            said.push_str(" One of yours is being delivered right now: it cannot be cancelled.");
        }
        Ok(said)
    }
}

/// What an asked-for end is, in a word: `end` or `replace`.
fn request_kind(pending: &PendingEnd) -> &'static str {
    if pending.replace.is_some() {
        "replace"
    } else {
        "end"
    }
}

/// The start request a replacement makes, as `record`'s agent would ask
/// it: its kind, account, and launch options, only the model changed.
fn successor_start(record: &SessionRecord, successor: &Successor) -> PendingStart {
    let mut launch = record.launch.clone().unwrap_or_default();
    if let Some(model) = &successor.model {
        launch.model = Some(model.clone());
    }
    PendingStart {
        id: new_id(),
        workspace_id: record.workspace_id.clone(),
        name: Some(successor.name.clone()),
        kind: record.kind,
        prompt: Some(successor.prompt.clone()),
        folder: successor.folder.clone(),
        worktree: successor.worktree.clone(),
        launch: (!launch.is_empty()).then_some(launch),
        account: record.account.clone(),
        created_at: Utc::now(),
        // The work goes on under the same labels (the hub sets its own).
        labels: record
            .labels
            .iter()
            .filter(|(k, _)| !k.starts_with(crate::labels::HUB_PREFIX))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

fn ended_event(end: &EndRecord, target: Option<String>) -> What {
    What::Ended {
        by: end.by.tag(),
        killed: work_of(&end.killed)
            .iter()
            .map(|p| format!("{} {}", p.pid, p.command))
            .collect(),
        dirty: end.dirty.as_ref().map(dirty_text),
        successor: end.successor.clone(),
        target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::SessionKind;
    use chrono::{DateTime, TimeZone};

    fn at(minute: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + minute * 60, 0).unwrap()
    }

    fn record(id: &str, opened_by: Option<&str>, running: bool) -> SessionRecord {
        SessionRecord {
            id: id.to_owned(),
            workspace_id: "w".to_owned(),
            name: format!("s-{id}"),
            kind: SessionKind::Claude,
            cwd: PathBuf::from("/r"),
            command: vec!["claude".to_owned()],
            agent_session_id: None,
            created_at: at(0),
            state: if running {
                SessionState::Running
            } else {
                SessionState::Exited {
                    exit: crate::pty::ExitInfo {
                        code: Some(0),
                        signal: None,
                    },
                    at: at(1),
                }
            },
            fallback_command: None,
            worktree: None,
            companion_of: None,
            launch: None,
            reviewer: None,
            review_of: None,
            account: None,
            continues: None,
            opened_by: opened_by.map(str::to_owned),
            handoffs: Vec::new(),
            start_sessions: None,
            start_requests: Vec::new(),
            starts_made: 0,
            owner: None,
            ending: None,
            ended: None,
            end_requests: Vec::new(),
            stale: None,
            never_relaunch: false,
            labels: Default::default(),
        }
    }

    #[test]
    fn the_owner_is_the_opener_unless_given_and_passes_to_successors() {
        let worker = record("worker", Some("lead1"), true);
        // Its opener runs: it owns it.
        let lead1 = record("lead1", None, true);
        assert_eq!(
            effective_owner(&worker, &[lead1.clone(), worker.clone()]).as_deref(),
            Some("lead1")
        );
        // The operator started it, and nobody was given it: nobody owns it.
        assert_eq!(effective_owner(&lead1, std::slice::from_ref(&lead1)), None);
        // The opener ended; its handoff successor (the newest) owns it.
        let gone = record("lead1", None, false);
        let mut lead2 = record("lead2", None, true);
        lead2.continues = Some("lead1".into());
        lead2.created_at = at(5);
        let mut older = record("lead2-old", None, false);
        older.continues = Some("lead1".into());
        older.created_at = at(3);
        let all = vec![gone.clone(), older, lead2.clone(), worker.clone()];
        assert_eq!(effective_owner(&worker, &all).as_deref(), Some("lead2"));
        // A replacement named on the ended record wins, and the chain goes
        // on past an ended successor.
        let mut replaced = gone.clone();
        replaced.ended = Some(EndRecord {
            by: Actor::Session("x".into()),
            at: at(4),
            mode: EndMode::Graceful,
            killed: Vec::new(),
            dirty: None,
            successor: Some("lead3".into()),
            by_name: Some("s-x".into()),
            successor_name: Some("s-lead3".into()),
            close: None,
        });
        let mut lead3 = record("lead3", Some("x"), false);
        lead3.continues = Some("lead1".into());
        let mut lead4 = record("lead4", None, true);
        lead4.continues = Some("lead3".into());
        let all = vec![replaced, lead3, lead4, worker.clone()];
        assert_eq!(effective_owner(&worker, &all).as_deref(), Some("lead4"));
        // Given away: the owner, not the opener.
        let mut given = worker.clone();
        given.owner = Some("other".into());
        let other = record("other", None, true);
        assert_eq!(
            effective_owner(&given, &[lead1, other, given.clone()]).as_deref(),
            Some("other")
        );
        // An owner gone with no successor stays named (nobody running owns it).
        assert_eq!(effective_owner(&worker, &[gone]).as_deref(), Some("lead1"));
    }

    #[test]
    fn the_parent_is_the_owner_while_on_the_hub_else_the_opener_else_none() {
        let lead1 = record("lead1", None, false);
        let lead2 = record("lead2", None, true);
        let mut worker = record("worker", Some("lead1"), true);
        // Never given: its opener, ended or not.
        assert_eq!(
            effective_parent(&worker, &[lead1.clone(), worker.clone()]).as_deref(),
            Some("lead1")
        );
        // Given: the new owner, the opener still on the hub or not.
        worker.owner = Some("lead2".into());
        let all = [lead1.clone(), lead2.clone(), worker.clone()];
        assert_eq!(effective_parent(&worker, &all).as_deref(), Some("lead2"));
        let gone = [lead2.clone(), worker.clone()];
        assert_eq!(effective_parent(&worker, &gone).as_deref(), Some("lead2"));
        // Its owner removed with no successor: back to the opener.
        assert_eq!(
            effective_parent(&worker, &[lead1.clone(), worker.clone()]).as_deref(),
            Some("lead1")
        );
        // Its owner's successor leads it.
        let mut lead3 = record("lead3", None, true);
        lead3.continues = Some("lead2".into());
        let mut ended = lead2;
        ended.state = lead1.state.clone();
        assert_eq!(
            effective_parent(&worker, &[ended, lead3, worker.clone()]).as_deref(),
            Some("lead3")
        );
        // Both gone: a root.
        assert_eq!(
            effective_parent(&worker, std::slice::from_ref(&worker)),
            None
        );
    }

    #[test]
    fn a_cycle_of_successors_ends_the_walk() {
        let mut a = record("a", None, false);
        a.continues = Some("b".into());
        let mut b = record("b", None, false);
        b.continues = Some("a".into());
        let w = record("w", Some("a"), true);
        assert!(effective_owner(&w, &[a, b, w.clone()]).is_some());
    }

    #[test]
    fn waits_are_bounded() {
        assert_eq!(wait_of(None).unwrap(), DEFAULT_WAIT_MINUTES);
        assert_eq!(wait_of(Some(1)).unwrap(), 1);
        assert_eq!(wait_of(Some(MAX_WAIT_MINUTES)).unwrap(), MAX_WAIT_MINUTES);
        assert!(wait_of(Some(0)).is_err());
        assert!(wait_of(Some(MAX_WAIT_MINUTES + 1)).is_err());
    }

    #[test]
    fn previews_skip_the_sender_line() {
        let item = QueueItem {
            id: "i".into(),
            text: "From lead in repo: a message\n\nstop the build\nmore".into(),
            when: When::Done,
            added_at: at(0),
            blocked_at: None,
            from: Some("lead".into()),
            priority: false,
            watch: None,
            topic: None,
            state: ItemState::Pending,
        };
        assert_eq!(preview(&item), "stop the build");
        let operator = QueueItem {
            from: None,
            text: "first\n\nsecond".into(),
            ..item
        };
        assert_eq!(preview(&operator), "first");
    }

    #[test]
    fn losses_read_plainly() {
        assert!(loss_text(&[], None).starts_with("nothing I can see"));
        let said = loss_text(
            &[
                ProcessNote {
                    pid: 7,
                    command: "npm run dev".into(),
                    helper: false,
                },
                ProcessNote {
                    pid: 8,
                    command: "npx @playwright/mcp".into(),
                    helper: true,
                },
            ],
            Some(&DirtyNote {
                path: PathBuf::from("/wt/x"),
                branch: Some("feat/x".into()),
                dirty: 2,
                untracked: 1,
                unmerged: 3,
            }),
        );
        assert!(said.contains("7 npm run dev"), "{said}");
        assert!(
            said.contains("/wt/x (branch feat/x) has 2 uncommitted changes, 1 untracked file, 3 commits not merged"),
            "{said}"
        );
        assert!(
            !said.contains("playwright"),
            "helpers are counted, not named: {said}"
        );
        assert!(
            said.ends_with(
                "besides, 1 helper process of the agent's own (MCP servers and their tooling, \
                 which end with any session)"
            ),
            "{said}"
        );
        let only_helpers = loss_text(
            &[ProcessNote {
                pid: 8,
                command: "serena".into(),
                helper: true,
            }],
            None,
        );
        assert!(
            only_helpers.starts_with("nothing I can see"),
            "{only_helpers}"
        );
    }

    #[test]
    fn the_agents_own_helpers_are_told_from_work_it_started() {
        // 10 claude
        //   11 serena (an MCP server), 12 its python
        //   13 sh -c playwright-mcp (an MCP config's own shell)
        //   20 the Bash tool's shell, 21 the dev server it started
        //   40 bash -lc (Codex's shell tool), 41 what it runs
        // 30 a nohup job left behind (reparented to 1)
        let tree: &[(u32, u32, &str)] = &[
            (11, 10, "uvx serena start-mcp-server"),
            (12, 11, "python serena"),
            (13, 10, "sh -c playwright-mcp"),
            (
                20,
                10,
                "/bin/bash -c source /home/u/.claude/shell-snapshots/x.sh && eval 'npm run dev'",
            ),
            (21, 20, "node dev-server"),
            (30, 1, "node worker.js"),
            (40, 10, "bash -lc cargo watch"),
            (41, 40, "cargo watch"),
        ];
        let parent = |pid: u32| tree.iter().find(|t| t.0 == pid).map(|t| t.1);
        let command = |pid: u32| {
            tree.iter()
                .find(|t| t.0 == pid)
                .map_or(String::new(), |t| t.2.to_owned())
        };
        let helper = |pid, codex| is_helper(pid, 10, codex, &parent, &command);
        assert!(helper(11, false));
        assert!(helper(12, false));
        assert!(
            helper(13, false),
            "a shell an MCP config runs is not the Bash tool's"
        );
        assert!(!helper(20, false));
        assert!(!helper(21, false), "started through the Bash tool");
        assert!(!helper(30, false), "left the agent: work");
        assert!(helper(41, false));
        assert!(!helper(41, true), "Codex's shell tool");
    }
}
