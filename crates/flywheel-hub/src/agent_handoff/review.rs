//! "Ask for a review": one action launches (or reuses) a reviewer agent of
//! the other kind -- Claude Code work gets a Codex reviewer and back
//! (`review.reviewer` in hub.toml; the same kind when the other is not
//! installed) -- in the author's directory, read-only (Claude
//! `--permission-mode plan`; Codex `-s read-only -a never`), with a prompt
//! carrying `git diff <base>...HEAD` (capped; the agent is told how to read
//! the rest itself). The pairing is kept on both records (`reviewer`,
//! `review_of`), so it survives restarts and the sidebar and `fw hub ls`
//! show it. The reviewer's answer comes back as its findings: its latest
//! reply and the code blocks in it, which "send findings" pastes into the
//! author under a provenance header.

use super::{DEFAULT_CONTENT_BYTES, Delivered, Delivery, Provenance, assemble, branch_of, header};
use crate::agent::{self, AgentLaunch, SessionKind};
use crate::agentlog::Role;
use crate::hub::{Hub, HubError, SessionView};
use crate::review::Tools;
use crate::search::MessageKind;
use crate::state::{SessionRecord, SessionState, StateError, new_id};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Diff bytes put in a review prompt (the agent's first turn is at most
/// [`agent::MAX_PROMPT_BYTES`] with the instructions around it).
pub const REVIEW_DIFF_BYTES: usize = 24 * 1024;
/// Transcript messages read back for the reviewer's latest reply.
const FINDINGS_MESSAGES: usize = 200;

/// A review request.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReviewRequest {
    /// The reviewer's kind (default: `review.reviewer` relative to the
    /// author's kind).
    #[serde(default)]
    pub kind: Option<SessionKind>,
    /// Compare against this instead of the session's base.
    #[serde(default)]
    pub base: Option<String>,
    /// The reviewer's model (default: the author's when both are the same
    /// kind of agent, else hub.toml `agents.<kind>.model`, else the CLI's
    /// own).
    #[serde(default)]
    pub model: Option<String>,
    /// Say what would happen; launch nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// One side of a pairing.
#[derive(Debug, Clone, Serialize)]
pub struct PairRef {
    pub id: String,
    pub name: String,
    pub kind: SessionKind,
    /// Its agent CLI runs now.
    pub running: bool,
    /// Its agent status (`working`, `done`, ...), when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
}

/// What asking for a review did (or, dry, would do).
#[derive(Debug, Clone, Serialize)]
pub struct ReviewLaunch {
    pub author: PairRef,
    /// The reviewer session (absent on a dry run that would launch one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<SessionView>,
    pub kind: SessionKind,
    /// The running reviewer was asked again instead of a new one started.
    pub reused: bool,
    /// Why the reviewer is not the kind asked for (the other is not
    /// installed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    /// The earlier reviewer this one replaces (stopped, or of another
    /// kind): closed, so one reviewer is paired with the author.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaced: Option<PairRef>,
    /// The model alias it was given and what it runs instead: Claude
    /// Code's plan mode runs a bare alias (`haiku`) on a larger model
    /// (`claude-sonnet-5-5`, ~13x the cost), so the alias is pinned to the
    /// id the author ran, or this says it could not be.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_note: Option<String>,
    /// The read-only flags it runs with.
    pub flags: Vec<String>,
    pub base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub prompt: String,
    pub diff_bytes: usize,
    pub diff_truncated: bool,
    pub dry_run: bool,
}

/// A code block of the reviewer's reply.
#[derive(Debug, Clone, Serialize)]
pub struct FindingBlock {
    pub lang: Option<String>,
    pub shell: bool,
    /// Exactly as written.
    pub text: String,
    /// What Copy / Send paste (`$ ` prompts stripped, see
    /// [`crate::commands::copyable`]).
    pub command: String,
}

/// The reviewer's findings so far.
#[derive(Debug, Clone, Serialize)]
pub struct Findings {
    pub author: PairRef,
    pub reviewer: PairRef,
    /// Its latest reply (the text after its last prompt), when written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub blocks: Vec<FindingBlock>,
    /// What "send findings" pastes into the author (absent: nothing to
    /// send yet).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub truncated: bool,
}

/// The read-only launch of a reviewer of `kind`.
pub fn read_only_launch(kind: SessionKind) -> AgentLaunch {
    match kind {
        SessionKind::Claude => AgentLaunch {
            permission_mode: Some("plan".into()),
            ..AgentLaunch::default()
        },
        SessionKind::Codex => AgentLaunch {
            approval: Some("never".into()),
            sandbox: Some("read-only".into()),
            ..AgentLaunch::default()
        },
        SessionKind::Shell => AgentLaunch::default(),
    }
}

/// The model a `kind` reviewer of `author` is asked to run: the one named,
/// else the author's own when it is the same kind of agent (a haiku
/// session's reviewer is not launched on the user's default model, which
/// may cost many times more); `None` leaves it to hub.toml and the CLI.
fn reviewer_model(
    request: &ReviewRequest,
    author: &SessionRecord,
    kind: SessionKind,
) -> Option<String> {
    let named = request
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    match named {
        Some(model) => Some(model.to_owned()),
        None if author.kind == kind => author
            .launch
            .as_ref()
            .and_then(|l| l.model.clone())
            .filter(|m| !m.trim().is_empty()),
        None => None,
    }
}

/// A Claude model alias (`haiku`, `sonnet[1m]`, `opus`): not a full id.
fn claude_alias(model: &str) -> Option<&str> {
    let family = model.split('[').next().unwrap_or(model).trim();
    (!family.is_empty() && !family.starts_with("claude-") && !family.contains('-'))
        .then_some(family)
}

/// A Claude plan-mode `launch` whose model is a bare alias: Claude Code
/// (2.1.306, verified) runs plan mode on a larger model than the alias
/// names unless the model is a full id. The alias is pinned to the one
/// model of its family the author's transcript shows (`models`); else the
/// launch keeps it and the note says what it may cost.
fn pin_plan_model(
    kind: SessionKind,
    launch: &mut AgentLaunch,
    models: &[crate::usage::ModelUsage],
) -> Option<String> {
    if kind != SessionKind::Claude || launch.permission_mode.as_deref() != Some("plan") {
        return None;
    }
    let alias = launch.model.clone()?;
    let family = claude_alias(&alias)?;
    let mut ids: Vec<&str> = models
        .iter()
        .map(|m| m.model.as_str())
        .filter(|id| id.split('-').any(|part| part == family))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if let [id] = ids[..] {
        launch.model = Some(id.to_owned());
        return Some(format!(
            "--model {alias} is pinned to {id} (the author's): plan mode runs a bare alias on a \
             larger model"
        ));
    }
    Some(format!(
        "--model {alias} may not hold: Claude Code's plan mode runs a bare alias on a larger \
         model (haiku ran as sonnet, ~13x the cost); name a full id (--model claude-{family}-...) \
         to keep it"
    ))
}

fn other(kind: SessionKind) -> SessionKind {
    match kind {
        SessionKind::Codex => SessionKind::Claude,
        _ => SessionKind::Codex,
    }
}

/// The review prompt: what to review and how, then the diff (cut to
/// `max` bytes, with where to read the rest).
pub fn review_prompt(
    author: &SessionRecord,
    branch: Option<&str>,
    base: &str,
    diff: &str,
    dirty: bool,
    max: usize,
) -> (String, usize, bool) {
    let what = match branch {
        Some(branch) => format!("branch {branch}"),
        None => "this work tree's HEAD".to_owned(),
    };
    let mut intro = format!(
        "Review the work on {what} against {base}: `git diff {base}...HEAD`, written by \
         another coding agent ({} session \"{}\").\n\
         This is a read-only review: do not edit, create, or delete files, and do not stage, \
         commit, push, or run anything that changes the repository. Read-only commands \
         (git diff, git log, git show, reading files, running the tests) are fine.\n\
         Report, most severe first: correctness bugs, missed edge cases, security problems, \
         and missing tests -- each with file:line and why it matters. Put every command the \
         author should run, and every concrete code change, in its own fenced code block. \
         End with a one-line verdict.",
        author.kind.as_str(),
        author.name
    );
    if dirty {
        intro.push_str(
            "\nThe work tree also has uncommitted changes: review them too (`git status`, \
             `git diff HEAD`).",
        );
    }
    // A NUL would make the prompt no argument at all.
    let diff = diff.replace('\0', "");
    let diff = diff.trim_end_matches(['\n', '\r']);
    let (kept, truncated) = super::cut(diff, max);
    let rest = truncated.then(|| {
        format!(
            "The diff above is cut at {} of {} bytes: read the rest with `git diff {base}...HEAD`.",
            kept.len(),
            diff.len()
        )
    });
    if diff.is_empty() {
        return (
            format!("{intro}\n\nNothing is committed beyond {base} yet."),
            0,
            false,
        );
    }
    let header = format!("{intro}\n\nThe diff (`git diff {base}...HEAD`):");
    let (mut prompt, kept, _) = assemble(&header, "diff", kept, usize::MAX, rest.as_deref());
    if prompt.ends_with('\n') {
        prompt.pop();
    }
    (prompt, kept, truncated)
}

/// `reply` as the prompt "send findings" pastes into the author.
fn findings_prompt(reviewer: &SessionView, author: &SessionRecord, reply: &str) -> (String, bool) {
    let from = Provenance {
        session_id: reviewer.record.id.clone(),
        session: reviewer.record.name.clone(),
        kind: reviewer.record.kind,
        branch: branch_of(reviewer),
        machine: None,
    };
    let what = format!(
        "review findings ({} reviewed {}'s work, read-only)",
        reviewer.record.kind.as_str(),
        author.name
    );
    let header = header(&from, None, &what);
    let (prompt, _, truncated) = assemble(
        &header,
        "markdown",
        reply,
        DEFAULT_CONTENT_BYTES,
        Some(
            "Address these findings: fix what is right, and say briefly why for any you \
             disagree with.",
        ),
    );
    (prompt, truncated)
}

impl Hub {
    fn pair_ref(&self, view: &SessionView) -> PairRef {
        PairRef {
            id: view.record.id.clone(),
            name: view.record.name.clone(),
            kind: view.record.kind,
            running: view.record.runs_agent()
                && view.record.state.is_running()
                && self
                    .live_session(&view.record.id)
                    .is_some_and(|s| s.exit_info().is_none()),
            status: view.agent.as_ref().map(|a| a.status.as_str()),
        }
    }

    /// Ask an agent of the other kind to review session `id`'s work (its
    /// agent's, for a companion shell; the reviewed session's, for a
    /// reviewer). Blocking (runs git, may spawn).
    pub fn request_review(
        self: &Arc<Self>,
        tools: &Tools,
        id: &str,
        request: &ReviewRequest,
    ) -> Result<ReviewLaunch, HubError> {
        // One request at a time: a second one (a double click, two CLIs)
        // sees the reviewer the first paired and asks it again instead of
        // starting another.
        let _one = self
            .review_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let asked = self.session(id)?.record;
        let id = asked.review_of.clone().unwrap_or_else(|| asked.id.clone());
        let (record, target) = self.review_target(tools, &id, request.base.as_deref())?;
        let author = match self.review_agent(&record, &target.dir) {
            Some(agent) => self.session(&agent.id)?,
            None => self.session(&record.id)?,
        };
        // Neither the author's nor its reviewer's prompt queue types in
        // while the review is set up and asked.
        let mut held = vec![author.record.id.clone()];
        held.extend(author.record.reviewer.clone());
        let _held = self.hold_sessions(held, "asking for a review");
        if let Some(kind) = request.kind
            && !kind.is_agent()
        {
            return Err(StateError::Invalid("a reviewer is claude or codex".into()).into());
        }
        let review = crate::review::read(tools, &target).map_err(StateError::Conflict)?;
        let diff =
            crate::review::branch_diff(tools, &target, None, None).map_err(StateError::Conflict)?;
        if diff.text.trim().is_empty() && review.clean() {
            return Err(StateError::Conflict(format!(
                "nothing to review: {} has no commits beyond {} and no uncommitted changes",
                review.branch.as_deref().unwrap_or("HEAD"),
                diff.base
            ))
            .into());
        }
        let installed = |kind: SessionKind| self.agent_program(kind).is_ok();
        let (kind, fallback) = match request.kind {
            Some(kind) if installed(kind) => (kind, None),
            Some(kind) => {
                return Err(StateError::Invalid(format!(
                    "{} is not installed on this hub's PATH",
                    kind.as_str()
                ))
                .into());
            }
            None => {
                let preferred = self.tunables().reviewer.preferred(author.record.kind);
                if installed(preferred) {
                    (preferred, None)
                } else if installed(other(preferred)) {
                    (
                        other(preferred),
                        Some(format!(
                            "{} is not installed here: the reviewer is {}",
                            preferred.as_str(),
                            other(preferred).as_str()
                        )),
                    )
                } else {
                    return Err(StateError::Invalid(
                        "neither claude nor codex is installed on this hub's PATH".into(),
                    )
                    .into());
                }
            }
        };
        let branch = review.branch.clone().or_else(|| branch_of(&author));
        let (prompt, diff_bytes, diff_truncated) = review_prompt(
            &author.record,
            branch.as_deref(),
            &diff.base,
            &diff.text,
            !review.clean(),
            REVIEW_DIFF_BYTES,
        );
        let tunables = self.tunables();
        let mut read_only = read_only_launch(kind);
        read_only.model = reviewer_model(request, &author.record, kind);
        let mut launch = tunables
            .agent_defaults
            .fill(kind, &read_only)
            .validated(kind)
            .map_err(StateError::Invalid)?;
        let model_note = pin_plan_model(
            kind,
            &mut launch,
            author.usage.as_ref().map_or(&[][..], |u| &u.models[..]),
        );
        let flags = launch.args(kind);
        // The paired reviewer, running as the kind wanted: asked again.
        let reusable = author
            .record
            .reviewer
            .as_deref()
            .and_then(|r| self.session(r).ok())
            .filter(|r| {
                r.record.kind == kind
                    && r.record.review_of.as_deref() == Some(author.record.id.as_str())
                    && self.pair_ref(r).running
            });
        // A paired reviewer not asked again is replaced (and closed).
        let replaced = match &reusable {
            Some(_) => None,
            None => author
                .record
                .reviewer
                .as_deref()
                .and_then(|r| self.session(r).ok())
                .filter(|r| r.record.review_of.as_deref() == Some(author.record.id.as_str()))
                .map(|r| self.pair_ref(&r)),
        };
        let mut outcome = ReviewLaunch {
            author: self.pair_ref(&author),
            reviewer: reusable.clone(),
            kind,
            reused: reusable.is_some(),
            fallback,
            replaced,
            model_note,
            flags,
            base: diff.base.clone(),
            branch: branch.clone(),
            prompt,
            diff_bytes,
            diff_truncated,
            dry_run: request.dry_run,
        };
        if request.dry_run {
            return Ok(outcome);
        }
        let reviewer = match reusable {
            Some(reviewer) => {
                self.paste(&reviewer.record.id, &outcome.prompt, true)?;
                self.session(&reviewer.record.id)?
            }
            None => self.launch_reviewer(&author.record, kind, launch, &outcome.prompt)?,
        };
        eprintln!(
            "fw hub: review {}: {} reviewer {} ({}, {})",
            author.record.label(),
            if outcome.reused { "asked" } else { "launched" },
            reviewer.record.label(),
            kind.as_str(),
            outcome.flags.join(" ")
        );
        if let (Some(author_event), Some(reviewer_event)) = (
            self.event_session(&author.record.id),
            self.event_session(&reviewer.record.id),
        ) {
            self.emit_lifecycle(
                "review.requested",
                serde_json::json!({
                    "author": author_event,
                    "reviewer": reviewer_event,
                    "reused": outcome.reused,
                    "kind": kind,
                    "base": diff.base,
                    "branch": branch,
                    "dir": target.dir,
                }),
            );
        }
        outcome.author = self.pair_ref(&self.session(&author.record.id)?);
        outcome.reviewer = Some(reviewer);
        Ok(outcome)
    }

    /// Start a reviewer agent in `author`'s directory (and worktree), paired
    /// with it.
    fn launch_reviewer(
        self: &Arc<Self>,
        author: &SessionRecord,
        kind: SessionKind,
        launch: AgentLaunch,
        prompt: &str,
    ) -> Result<SessionView, HubError> {
        let program = self.agent_program(kind)?;
        if !author.cwd.is_dir() {
            return Err(StateError::Conflict(format!(
                "{}'s directory {} no longer exists",
                author.label(),
                author.cwd.display()
            ))
            .into());
        }
        let prompt = agent::validate_prompt(Some(prompt)).map_err(StateError::Invalid)?;
        let record = SessionRecord {
            id: new_id(),
            workspace_id: author.workspace_id.clone(),
            name: self.unique_session_name(&format!("{} review", author.name)),
            kind,
            cwd: author.cwd.clone(),
            command: vec![program.to_string_lossy().into_owned()],
            agent_session_id: (kind == SessionKind::Claude)
                .then(|| uuid::Uuid::new_v4().hyphenated().to_string()),
            created_at: Utc::now(),
            state: SessionState::Running,
            fallback_command: None,
            worktree: author.worktree.clone(),
            companion_of: None,
            launch: Some(launch),
            reviewer: None,
            review_of: Some(author.id.clone()),
        };
        let view = self.launch(record, None, None, prompt.as_deref())?;
        let paired = self.store().set_review_pair(&author.id, &view.record.id);
        let replaced = match paired {
            Ok(replaced) => replaced,
            Err(e) => {
                // Not paired: the reviewer just started goes again, so a
                // retry does not leave a second agent running.
                if let Err(undo) = self.remove_session(&view.record.id) {
                    eprintln!(
                        "fw hub: review {}: closing unpaired reviewer {}: {undo}",
                        author.label(),
                        view.record.label()
                    );
                }
                return Err(e.into());
            }
        };
        if let Some(old) = replaced {
            // One reviewer per author: the one replaced is closed (its
            // transcript stays where its CLI wrote it).
            match self.remove_session(&old) {
                Ok(()) | Err(HubError::State(StateError::NotFound(_))) => {}
                Err(e) => eprintln!(
                    "fw hub: review {}: closing replaced reviewer {old}: {e}",
                    author.label()
                ),
            }
        }
        self.emit(crate::hub::HubEvent::Sessions);
        self.session(&view.record.id)
    }

    /// The (author, reviewer) pairing session `id` is part of: as either
    /// side, or as the author's companion shell.
    fn review_pair(&self, id: &str) -> Result<(SessionView, SessionView), HubError> {
        let view = self.session(id)?;
        let record = &view.record;
        if let Some(author) = &record.review_of {
            let author = self.session(author)?;
            return Ok((author, view));
        }
        let author = match (&record.reviewer, &record.companion_of) {
            (Some(_), _) => view.clone(),
            (None, Some(agent)) => self.session(agent)?,
            (None, None) => view.clone(),
        };
        let reviewer = author.record.reviewer.clone().ok_or_else(|| {
            StateError::Conflict(format!(
                "no reviewer has looked at {}'s work: ask for a review first",
                author.record.label()
            ))
        })?;
        Ok((author, self.session(&reviewer)?))
    }

    /// The reviewer's findings so far (its latest reply and the code
    /// blocks in it). Blocking (reads its transcript).
    pub fn review_findings(&self, id: &str) -> Result<Findings, HubError> {
        let (author, reviewer) = self.review_pair(id)?;
        let page = self.transcript_page(&reviewer.record.id, None, FINDINGS_MESSAGES, 0)?;
        // The reply: every text it wrote after the last prompt.
        let last_prompt = page
            .messages
            .iter()
            .rposition(|m| m.meta.role == Role::User && m.meta.kind == MessageKind::Text);
        let replies: Vec<_> = page.messages[last_prompt.map_or(0, |i| i + 1)..]
            .iter()
            .filter(|m| m.meta.role == Role::Assistant && m.meta.kind == MessageKind::Text)
            .collect();
        let mut reply = (!replies.is_empty()).then(|| {
            replies
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        });
        let mut timestamp = replies.last().and_then(|m| m.meta.timestamp.clone());
        if reply.is_none() && page.transcript.is_none() {
            // No transcript (a Codex turn known from its notify only):
            // the blocks of its newest reply.
            let list = self.commands(
                &reviewer.record.id,
                crate::transcript::CommandFilter {
                    all: true,
                    limit: 200,
                },
            )?;
            if let Some(newest) = list.commands.first() {
                let mut blocks: Vec<_> = list
                    .commands
                    .iter()
                    .filter(|c| c.message_id == newest.message_id)
                    .collect();
                blocks.sort_by_key(|c| c.block_index);
                timestamp.clone_from(&newest.timestamp);
                reply = Some(
                    blocks
                        .iter()
                        .map(|c| {
                            let fence = super::fence_for(&c.text);
                            format!(
                                "{fence}{}\n{}\n{fence}",
                                c.lang.as_deref().unwrap_or(""),
                                c.text
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                );
            }
        }
        let blocks = reply
            .as_deref()
            .map(|text| {
                crate::commands::fenced_blocks(text)
                    .into_iter()
                    .filter(|b| !b.text.trim().is_empty())
                    .map(|b| FindingBlock {
                        shell: crate::commands::is_shell(b.lang.as_deref(), &b.text),
                        command: crate::commands::copyable(b.lang.as_deref(), &b.text),
                        lang: b.lang,
                        text: b.text,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (prompt, truncated) = match reply.as_deref() {
            Some(text) => {
                let (prompt, truncated) = findings_prompt(&reviewer, &author.record, text);
                (Some(prompt), truncated)
            }
            None => (None, false),
        };
        Ok(Findings {
            author: self.pair_ref(&author),
            reviewer: self.pair_ref(&reviewer),
            reply,
            timestamp,
            blocks,
            prompt,
            truncated,
        })
    }

    /// Paste the reviewer's findings (or `edited`, the user's version of
    /// them) into the author agent, then Enter.
    pub fn send_findings(&self, id: &str, edited: Option<&str>) -> Result<Delivered, HubError> {
        let findings = self.review_findings(id)?;
        let prompt = match edited.filter(|p| !p.trim().is_empty()) {
            Some(edited) => edited.to_owned(),
            None => findings.prompt.clone().ok_or_else(|| {
                StateError::Conflict(format!(
                    "{} has not answered yet: no findings to send",
                    findings.reviewer.name
                ))
            })?,
        };
        let reviewer = self.session(&findings.reviewer.id)?;
        self.handoff_deliver(
            &findings.author.id,
            &Delivery {
                prompt,
                from: Some(Provenance {
                    session_id: reviewer.record.id.clone(),
                    session: reviewer.record.name.clone(),
                    kind: reviewer.record.kind,
                    branch: branch_of(&reviewer),
                    machine: None,
                }),
                source: Some("review_findings".into()),
                truncated: findings.truncated,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ReviewerChoice;

    #[test]
    fn the_reviewer_is_the_other_kind_unless_configured() {
        use SessionKind::{Claude, Codex, Shell};
        assert_eq!(ReviewerChoice::Other.preferred(Claude), Codex);
        assert_eq!(ReviewerChoice::Other.preferred(Codex), Claude);
        assert_eq!(ReviewerChoice::Other.preferred(Shell), Claude);
        assert_eq!(ReviewerChoice::Same.preferred(Codex), Codex);
        assert_eq!(ReviewerChoice::Same.preferred(Claude), Claude);
        assert_eq!(ReviewerChoice::Codex.preferred(Codex), Codex);
        assert_eq!(ReviewerChoice::Claude.preferred(Claude), Claude);
    }

    fn used(model: &str) -> crate::usage::ModelUsage {
        crate::usage::ModelUsage {
            model: model.to_owned(),
            label: model.to_owned(),
            tokens: crate::usage::Tokens::default(),
            cost_usd: Some(0.01),
        }
    }

    #[test]
    fn a_plan_mode_reviewer_s_model_alias_is_pinned_to_the_author_s_id() {
        // Claude Code 2.1.306: `--model haiku --permission-mode plan` ran
        // every turn on claude-sonnet-5-5; `--model claude-haiku-5-5` kept
        // haiku.
        let plan = |model: &str| AgentLaunch {
            model: Some(model.to_owned()),
            ..read_only_launch(SessionKind::Claude)
        };
        let mut launch = plan("haiku");
        let note = pin_plan_model(
            SessionKind::Claude,
            &mut launch,
            &[used("claude-haiku-5-5")],
        );
        assert_eq!(launch.model.as_deref(), Some("claude-haiku-5-5"));
        assert!(note.unwrap().contains("pinned to claude-haiku-5-5"));
        assert_eq!(
            launch.args(SessionKind::Claude),
            ["--model", "claude-haiku-5-5", "--permission-mode", "plan"]
        );

        // Nothing to pin it to: kept, and said.
        let mut launch = plan("haiku");
        let note = pin_plan_model(
            SessionKind::Claude,
            &mut launch,
            &[used("claude-sonnet-5-5")],
        );
        assert_eq!(launch.model.as_deref(), Some("haiku"));
        assert!(note.unwrap().contains("name a full id"));

        // A full id, no model, or not plan mode: untouched.
        let mut launch = plan("claude-haiku-5-5");
        assert_eq!(pin_plan_model(SessionKind::Claude, &mut launch, &[]), None);
        let mut launch = read_only_launch(SessionKind::Claude);
        assert_eq!(pin_plan_model(SessionKind::Claude, &mut launch, &[]), None);
        let mut launch = AgentLaunch {
            model: Some("haiku".into()),
            ..AgentLaunch::default()
        };
        assert_eq!(pin_plan_model(SessionKind::Claude, &mut launch, &[]), None);
        assert_eq!(launch.model.as_deref(), Some("haiku"));
    }

    #[test]
    fn reviewers_launch_read_only() {
        assert_eq!(
            read_only_launch(SessionKind::Claude).args(SessionKind::Claude),
            ["--permission-mode", "plan"]
        );
        assert_eq!(
            read_only_launch(SessionKind::Codex).args(SessionKind::Codex),
            ["-a", "never", "-s", "read-only"]
        );
        for kind in [SessionKind::Claude, SessionKind::Codex] {
            assert!(!read_only_launch(kind).bypasses_permissions());
            read_only_launch(kind).validated(kind).unwrap();
        }
    }
}
