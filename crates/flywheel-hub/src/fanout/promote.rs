//! A fan-out and a flywheel task, both ways.
//!
//! **Promote** ("Promote to flywheel task", `fw hub fanout promote`):
//! a fan-out becomes a durable task definition in its repo's directory
//! work source, `<tasks dir>/active/<phase>/<id>.json`, for `fw worker`
//! to re-verify through the loop. The hub drafts it ([`draft`]) and the
//! embedding binary validates and writes it through flywheel-core's task
//! loader (the provider's `plan_task` / `write_task`, see
//! `crate::flywheel::FlywheelProvider`), never as hand-rolled JSON here:
//!
//! * the goal: the request's, else the seeding task's, else the prompt's
//!   first line; the whole prompt goes into `context.notes` -- unless the
//!   fan-out was seeded from a task with its prompt unedited: then that
//!   task's context, field for field (a seeded fan-out promotes only under
//!   an explicitly named new id: the task it came from is the durable one,
//!   which `fw worker` re-verifies as it is);
//! * the fan-out's graders: command graders, in order (held-out ones stay
//!   in the held-out registry: a task's graders are shown to its agent);
//! * protected paths: `context.constraints` lines; what enforces them in
//!   the loop is the repo-wide `[submit] protected_paths` (a branch
//!   touching one is parked, never landed), so the plan says which are not
//!   covered there (the hub never edits `flywheel.toml`, itself protected);
//! * the winner's commit (or a named passing candidate's): one
//!   `context.references` line, offered as prior work to consult, not the
//!   solution.
//!
//! The file is written as an uncommitted change in the fan-out's main
//! checkout, as `/fw-plan` and `fw init`'s "drop one JSON file per task"
//! do: the worker reads the working tree, and the hub never commits to
//! the operator's branch. Dry run first: the exact path and bytes, and a
//! fingerprint the write must confirm; the write refuses an existing file.
//!
//! **Seed** ("Fan out this task", `fw hub fanout new --from-task`): a
//! fan-out from a task's goal and context plus its command graders and
//! its held-out graders ([`seeded`]), held to the repo's protected paths.
//! Held-out graders run like the others but are kept out of what the hub
//! tells an agent: the prompt must not name them, and "ask to fix" says
//! only that a held-out check failed (an agent running as the operator's
//! user could still read them on disk, as with the worker's gate).

use super::{CreateFanout, Fanout, GraderInput, GraderSpec};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The longest goal a promotion writes (a goal is one or two sentences).
pub const MAX_GOAL_CHARS: usize = 600;
/// The tag every promoted task carries.
pub const PROMOTED_TAG: &str = "hub-fanout";

/// A task's `context`, field for field (what a promotion of a fan-out
/// seeded from it, prompt unedited, writes back).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedContext {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relevant: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub non_goals: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edge_cases: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

/// The flywheel task a fan-out was seeded from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOrigin {
    pub folder: PathBuf,
    pub task_id: String,
    /// Its `active/<phase>` directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub goal: String,
    /// The task definition's content digest when seeded.
    pub digest: String,
    /// How many of the fan-out's graders are the task's held-out ones.
    #[serde(default)]
    pub held_out: usize,
    /// The task's graders the hub does not run (rubric, manual,
    /// transcript): the worker still applies them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_run: Vec<String>,
    /// The task's context and tags as defined.
    #[serde(default)]
    pub context: SeedContext,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The fan-out's prompt is the task's own, unedited: a promotion
    /// writes the task's context back instead of the prompt.
    #[serde(default)]
    pub prompt_seeded: bool,
    /// The protected paths that came from the repo's `[submit]
    /// protected_paths` (the loop enforces those already).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_protect: Vec<String>,
}

/// A task file written from a fan-out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Promotion {
    pub task_id: String,
    pub phase: String,
    /// Absolute.
    pub path: PathBuf,
    /// The commit offered as a reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    pub at: DateTime<Utc>,
}

/// Where a promoted task file is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotedState {
    /// Still under `active/<phase>/`: the loop has not archived it.
    Active,
    /// Its phase was archived (`archive/<phase>[-N]/`): done and landed.
    Archived,
    /// Neither: removed or moved by hand.
    Gone,
}

/// A [`Promotion`] as a board shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromotedView {
    #[serde(flatten)]
    pub promotion: Promotion,
    pub state: PromotedState,
    /// Where the file is now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<PathBuf>,
}

impl Promotion {
    /// Where its file is now: where it was written, else where archiving
    /// its phase moved it (`<tasks>/archive/<phase>` or `<phase>-N`).
    pub fn view(&self) -> PromotedView {
        let (state, current) = if self.path.is_file() {
            (PromotedState::Active, Some(self.path.clone()))
        } else {
            match self.archived() {
                Some(path) => (PromotedState::Archived, Some(path)),
                None => (PromotedState::Gone, None),
            }
        };
        PromotedView {
            promotion: self.clone(),
            state,
            current,
        }
    }

    fn archived(&self) -> Option<PathBuf> {
        let file = self.path.file_name()?;
        let phase_dir = self.path.parent()?;
        let active = phase_dir.parent()?;
        if active.file_name()? != "active" {
            return None;
        }
        let archive = active.parent()?.join("archive");
        let mut found: Option<(u64, PathBuf)> = None;
        for entry in std::fs::read_dir(&archive).ok()?.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rank = if name == self.phase {
                1
            } else {
                match name
                    .strip_prefix(&self.phase)
                    .and_then(|rest| rest.strip_prefix('-'))
                    .and_then(|n| n.parse::<u64>().ok())
                {
                    Some(n) => n,
                    None => continue,
                }
            };
            let path = entry.path().join(file);
            if path.is_file() && found.as_ref().is_none_or(|(best, _)| rank > *best) {
                found = Some((rank, path));
            }
        }
        found.map(|(_, path)| path)
    }
}

// --- the provider seam's plain data ------------------------------------------

/// A task definition to write, as the hub drafts it. The provider maps it
/// onto flywheel's task shape and validates it with the real loader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDraft {
    pub task_id: String,
    /// The `active/<phase>` directory; `None`: a new phase after the
    /// existing ones.
    pub phase: Option<String>,
    pub goal: String,
    pub notes: String,
    #[serde(default)]
    pub relevant: Vec<String>,
    pub constraints: Vec<String>,
    pub references: Vec<String>,
    #[serde(default)]
    pub non_goals: Vec<String>,
    #[serde(default)]
    pub edge_cases: Vec<String>,
    pub tags: Vec<String>,
    /// Command graders, in order.
    pub graders: Vec<GraderSpec>,
    /// The fan-out's protected paths (relative to the repo): the provider
    /// says which the repo's `[submit] protected_paths` already refuses at
    /// landing.
    #[serde(default)]
    pub protect: Vec<String>,
}

/// Exactly what a promotion writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFilePlan {
    pub folder: PathBuf,
    /// Absolute.
    pub path: PathBuf,
    /// Relative to `folder`.
    pub relative: String,
    pub task_id: String,
    pub phase: String,
    /// The phase directory does not exist yet.
    pub new_phase: bool,
    /// The active phases there are now (to pick from).
    pub phases: Vec<String>,
    /// The file's bytes, word for word.
    pub contents: String,
    /// What else to know (the file stays uncommitted, ...).
    pub notes: Vec<String>,
}

/// A task file written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskWritten {
    pub path: PathBuf,
    pub relative: String,
    pub task_id: String,
    pub phase: String,
    /// The directory source lists it now (what `fw worker` and `fw status`
    /// read).
    pub listed: bool,
    /// The other active tasks with no done run: `fw worker` drains them
    /// too (in phase order), not only this one.
    #[serde(default)]
    pub queue: Vec<String>,
}

/// A flywheel task as a fan-out seed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSeed {
    pub folder: PathBuf,
    pub task_id: String,
    pub phase: Option<String>,
    pub goal: String,
    /// The task's goal, context, and visible graders as the agents' prompt.
    pub prompt: String,
    pub digest: String,
    /// Its command graders, in order (what its agent sees too).
    pub graders: Vec<GraderSpec>,
    /// Its held-out command graders (`[held_out] root`): registered for the
    /// task id, plus the standing oracles matching it.
    pub held_out: Vec<GraderSpec>,
    /// Its graders the hub does not run (rubric, manual, transcript).
    pub not_run: Vec<String>,
    pub context: SeedContext,
    pub tags: Vec<String>,
    /// What a candidate must leave unchanged: the repo's `[submit]
    /// protected_paths` the hub can hold to (literal paths and `dir/**`),
    /// then the paths its constraints name ([`protected_in_constraint`]).
    pub protect: Vec<String>,
    /// The part of `protect` from `[submit] protected_paths`.
    pub repo_protect: Vec<String>,
    /// `[submit] protected_paths` patterns the hub cannot hold to (globs
    /// inside a name): the loop still refuses a landing touching them.
    pub protect_skipped: Vec<String>,
}

/// A protected path as a task constraint line (what a promotion writes).
pub fn constraint_line(path: &str) -> String {
    format!(
        "Leave `{path}` unchanged: the fan-out this task came from failed any candidate that \
         changed it."
    )
}

/// The path a [`constraint_line`] protects (`None`: any other constraint).
pub fn protected_in_constraint(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("Leave `")?;
    let (path, tail) = rest.split_once('`')?;
    (tail.starts_with(" unchanged") && !path.trim().is_empty()).then_some(path)
}

impl TaskSeed {
    /// What a browser is shown: the held-out graders by name only (it
    /// never needs their commands; the server re-reads them at launch).
    pub fn redacted(&self) -> serde_json::Value {
        serde_json::json!({
            "folder": self.folder,
            "task_id": self.task_id,
            "phase": self.phase,
            "goal": self.goal,
            "prompt": self.prompt,
            "digest": self.digest,
            "graders": self.graders,
            "held_out": self
                .held_out
                .iter()
                .enumerate()
                .map(|(i, g)| g.name.clone().unwrap_or_else(|| format!("held-out {}", i + 1)))
                .collect::<Vec<_>>(),
            "not_run": self.not_run,
            "protect": self.protect,
            "protect_skipped": self.protect_skipped,
        })
    }
}

// --- promote --------------------------------------------------------------------

/// `POST /api/fanouts/{id}/promote`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PromoteRequest {
    /// Default: the fan-out's name as a slug plus its tag.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Default: a new phase after the existing ones.
    #[serde(default)]
    pub phase: Option<String>,
    /// Default: the seeding task's goal, else the prompt's first line.
    #[serde(default)]
    pub goal: Option<String>,
    /// A candidate label whose passing commit to reference; `""` / `none`
    /// for none. Default: the winner, if any.
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub confirm: Option<String>,
}

/// A drafted promotion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Drafted {
    pub draft: TaskDraft,
    /// The referenced commit.
    pub reference: Option<String>,
    pub warnings: Vec<String>,
}

/// `text` as a slug of `[a-z0-9-]`, at most `max` bytes.
pub fn slug(text: &str, max: usize) -> String {
    let mut slug = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= max {
            break;
        }
    }
    slug.trim_matches('-').to_owned()
}

/// The prompt's first non-empty line (markdown heading marks dropped),
/// whitespace folded, cut at a word near [`MAX_GOAL_CHARS`].
pub fn default_goal(prompt: &str) -> String {
    let line = prompt
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    cut_words(&line.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn cut_words(text: &str) -> String {
    if text.chars().count() <= MAX_GOAL_CHARS {
        return text.to_owned();
    }
    let head: String = text.chars().take(MAX_GOAL_CHARS - 3).collect();
    let cut = head.rfind(' ').map_or(head.as_str(), |at| &head[..at]);
    format!("{}...", cut.trim_end())
}

/// The candidate whose passing commit `request` references: `(label,
/// commit)`.
fn reference_of(
    fanout: &Fanout,
    request: &PromoteRequest,
) -> Result<Option<(String, String)>, String> {
    let named = request.reference.as_deref().map(str::trim);
    let label = match named {
        Some("" | "none") => return Ok(None),
        Some(label) => label.to_owned(),
        None => match &fanout.winner {
            Some(winner) => return Ok(Some((winner.label.clone(), winner.head.clone()))),
            None => return Ok(None),
        },
    };
    if let Some(winner) = fanout.winner.as_ref().filter(|w| w.label == label) {
        return Ok(Some((label, winner.head.clone())));
    }
    let candidate = fanout.candidate(&label).ok_or_else(|| {
        format!(
            "fan-out {:?} has no candidate {label:?}; its candidates: {}",
            fanout.name,
            fanout
                .candidates
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    match candidate.grade.as_ref() {
        Some(grade) if grade.passed && !grade.no_change() => match &grade.head {
            Some(head) => Ok(Some((label, head.clone()))),
            None => Err(format!("{label}'s passing grading recorded no commit")),
        },
        _ => Err(format!(
            "{label} has no passing grading with a commit: only a passing candidate's work is a \
             reference"
        )),
    }
}

/// The task a promotion of `fanout` writes, before the provider validates
/// it (see the module doc).
pub fn draft(fanout: &Fanout, request: &PromoteRequest) -> Result<Drafted, String> {
    let origin = fanout.origin.as_ref();
    let named_id = request
        .task_id
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    if let (Some(origin), None) = (origin, named_id) {
        // The task it came from is the durable asset already: a second
        // task for the same intent is a deliberate choice, never a default.
        return Err(format!(
            "fan-out {:?} came from flywheel task {}, which stays the durable task: to verify \
             it through the loop, run `fw worker` in {} (the Flywheel panel's Start worker); to \
             write a separate task anyway, name its id (--task-id)",
            fanout.name,
            origin.task_id,
            origin.folder.display()
        ));
    }
    let goal = request
        .goal
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(cut_words)
        .or_else(|| origin.map(|o| o.goal.clone()))
        .unwrap_or_else(|| default_goal(&fanout.prompt));
    if goal.trim().is_empty() {
        return Err("the task needs a goal: name one (the prompt has no text line)".into());
    }
    let tag = &fanout.id[..fanout.id.len().min(4)];
    let task_id = match named_id {
        Some(id) => id.to_owned(),
        None => {
            let base = slug(&fanout.name, 40);
            let base = if base.is_empty() {
                "fanout".to_owned()
            } else {
                base
            };
            format!("{base}-{tag}")
        }
    };
    let phase = request
        .phase
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned);
    let graders: Vec<GraderSpec> = fanout
        .graders
        .iter()
        .filter(|g| !g.held_out)
        .map(|g| GraderSpec {
            run: g.run.clone(),
            name: g.name.clone(),
            held_out: false,
        })
        .collect();
    if graders.is_empty() {
        return Err(
            "every grader of this fan-out is held out: a task needs a command grader its \
             worker runs (held-out graders stay in the held-out registry)"
                .into(),
        );
    }
    let mut warnings = Vec::new();
    let held = fanout.graders.len() - graders.len();
    if held > 0 {
        let (s, v) = if held == 1 { ("", "s") } else { ("s", "") };
        warnings.push(match origin.map(|o| o.task_id.as_str()) {
            Some(origin) if origin != task_id => format!(
                "{held} held-out grader{s} stay{v} out of the task file: registered for task \
                 {origin}, so they gate {origin}, not {task_id} (register them for {task_id} \
                 with /fw-verify to gate it too)"
            ),
            _ => format!(
                "{held} held-out grader{s} stay{v} out of the task file: the held-out gate runs \
                 only what is registered for {task_id} under [held_out] root (/fw-verify)"
            ),
        });
    }
    if let Some(origin) = origin
        && !origin.not_run.is_empty()
    {
        warnings.push(format!(
            "task {}'s other graders ({}) are not copied: the hub never ran them",
            origin.task_id,
            origin.not_run.join(", ")
        ));
    }
    let reference = reference_of(fanout, request)?;
    if reference.is_none() && request.reference.is_none() {
        let passing: Vec<&str> = fanout
            .candidates
            .iter()
            .filter(|c| {
                c.grade
                    .as_ref()
                    .is_some_and(|g| g.passed && !g.no_change() && g.head.is_some())
            })
            .map(|c| c.label.as_str())
            .collect();
        if let Some(first) = passing.first() {
            warnings.push(format!(
                "no winner picked, so no reference commit: {} passed every grader (name one \
                 with --reference {first}, or the Reference commit choice)",
                passing.join(", ")
            ));
        }
    }
    // Seeded from a task, prompt unedited: that task's context, field for
    // field, instead of the prompt drawn from it.
    let carried = origin.filter(|o| o.prompt_seeded).map(|o| &o.context);
    let mut context = carried.cloned().unwrap_or_default();
    if carried.is_none() {
        if let Some(o) = origin {
            context.relevant.clone_from(&o.context.relevant);
        }
        context.notes = format!(
            "Promoted from the hub's verified fan-out \"{name}\" ({id}, base {base} at \
             {base_sha}). The prompt its agents got:\n\n{prompt}",
            name = fanout.name,
            id = &fanout.id[..fanout.id.len().min(8)],
            base = fanout.base,
            base_sha = &fanout.base_sha[..fanout.base_sha.len().min(12)],
            prompt = fanout.prompt.trim_end(),
        );
    }
    if let Some((label, head)) = &reference {
        let candidate = fanout.candidate(label);
        let branch = candidate.map_or("", |c| c.worktree.branch.as_str());
        let agent = candidate.map_or_else(String::new, |c| match &c.launch.model {
            Some(model) => format!(", {}:{model}", c.kind.as_str()),
            None => format!(", {}", c.kind.as_str()),
        });
        let seeded = origin.map_or_else(String::new, |o| {
            format!(" (seeded from flywheel task {})", o.task_id)
        });
        context.references.push(format!(
            "Commit {head} (branch {branch}, fan-out candidate {label}{agent}) passed every \
             grader of the hub fan-out \"{name}\"{seeded} from {base} at {base_sha}. It is \
             prior work to consult (`git show {short}`), not the solution to copy: this task \
             must pass its graders on its own.",
            name = fanout.name,
            base = fanout.base,
            base_sha = &fanout.base_sha[..fanout.base_sha.len().min(12)],
            short = &head[..head.len().min(12)],
        ));
    }
    let repo_protect = origin.map_or(&[][..], |o| o.repo_protect.as_slice());
    for path in &fanout.protect {
        let line = constraint_line(path);
        let stated = context
            .constraints
            .iter()
            .any(|c| protected_in_constraint(c) == Some(path.as_str()));
        if !repo_protect.contains(path) && !stated {
            context.constraints.push(line);
        }
    }
    let mut tags = origin.map(|o| o.tags.clone()).unwrap_or_default();
    if !tags.iter().any(|t| t == PROMOTED_TAG) {
        tags.push(PROMOTED_TAG.to_owned());
    }
    Ok(Drafted {
        draft: TaskDraft {
            task_id,
            phase,
            goal,
            notes: context.notes,
            relevant: context.relevant,
            constraints: context.constraints,
            references: context.references,
            non_goals: context.non_goals,
            edge_cases: context.edge_cases,
            tags,
            graders,
            protect: fanout.protect.clone(),
        },
        reference: reference.map(|(_, head)| head),
        warnings,
    })
}

/// What a write must confirm: the path and the bytes, hashed.
pub fn fingerprint(plan: &TaskFilePlan) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(plan.path.to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(plan.contents.as_bytes());
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

// --- seed -------------------------------------------------------------------------

/// The words of a held-out grader command an agent must never be shown:
/// the whole command, every absolute path in it (the oracle script), and
/// those paths' file names (`exact.sh`).
pub(crate) fn secret_words(run: &str) -> Vec<&str> {
    let mut words = vec![run.trim()];
    for word in run
        .split_whitespace()
        .map(|w| w.trim_matches(|c| c == '\'' || c == '"' || c == ';' || c == '('))
        .filter(|w| w.starts_with('/') && w.len() > 1)
    {
        words.push(word);
        if let Some(name) = word.rsplit('/').next().filter(|n| n.len() >= 3) {
            words.push(name);
        }
    }
    words.retain(|w| !w.is_empty());
    words
}

/// `request` seeded from `seed` (its `from_task`): the task's prompt when
/// it brings none, its command graders when it brings none, then its
/// held-out graders (always, marked held out; a request grader running the
/// same command is replaced by it), and the task's protected paths.
/// Refused when the prompt names a held-out grader, or nothing is left to
/// grade with.
pub fn seeded(mut request: CreateFanout, seed: &TaskSeed) -> Result<CreateFanout, String> {
    let prompt_seeded =
        request.prompt.trim().is_empty() || request.prompt.trim() == seed.prompt.trim();
    if request.prompt.trim().is_empty() {
        request.prompt.clone_from(&seed.prompt);
    }
    for held in &seed.held_out {
        // A bare file name the task's own prompt already says is public
        // to the loop's agent too; a command or path never is.
        if let Some(word) = secret_words(&held.run).into_iter().find(|w| {
            request.prompt.contains(w)
                && !(!w.contains('/') && *w != held.run.trim() && seed.prompt.contains(w))
        }) {
            return Err(format!(
                "the prompt names a held-out grader of task {} ({word:?}): held-out graders \
                 run, but are never shown to the agents",
                seed.task_id
            ));
        }
    }
    if request.graders.is_empty() {
        request.graders = seed
            .graders
            .iter()
            .cloned()
            .map(GraderInput::Spec)
            .collect();
    }
    let held_runs: Vec<&str> = seed.held_out.iter().map(|g| g.run.trim()).collect();
    request.graders.retain(|input| {
        let run = match input {
            GraderInput::Run(run) => run.trim(),
            GraderInput::Spec(spec) => spec.run.trim(),
        };
        !held_runs.contains(&run)
    });
    // Held out is the registry's to say, never the request's.
    for input in &mut request.graders {
        if let GraderInput::Spec(spec) = input {
            spec.held_out = false;
        }
    }
    request.graders.extend(seed.held_out.iter().map(|g| {
        GraderInput::Spec(GraderSpec {
            run: g.run.clone(),
            name: g.name.clone(),
            held_out: true,
        })
    }));
    if request.graders.is_empty() {
        return Err(if seed.not_run.is_empty() {
            format!(
                "task {} has no command grader for the hub to run: add one (--grader)",
                seed.task_id
            )
        } else {
            format!(
                "task {}'s graders ({}) are not ones the hub runs (it runs command graders; \
                 the worker applies the others): add a command grader (--grader)",
                seed.task_id,
                seed.not_run.join(", ")
            )
        });
    }
    for path in &seed.protect {
        if !request.protect.iter().any(|p| p.trim() == path) {
            request.protect.push(path.clone());
        }
    }
    if request.name.as_deref().is_none_or(|n| n.trim().is_empty()) {
        request.name = Some(seed.task_id.clone());
    }
    request.folder = Some(seed.folder.clone());
    request.from_task = Some(seed.task_id.clone());
    request.origin = Some(TaskOrigin {
        folder: seed.folder.clone(),
        task_id: seed.task_id.clone(),
        phase: seed.phase.clone(),
        goal: seed.goal.clone(),
        digest: seed.digest.clone(),
        held_out: seed.held_out.len(),
        not_run: seed.not_run.clone(),
        context: seed.context.clone(),
        tags: seed.tags.clone(),
        prompt_seeded,
        repo_protect: seed.repo_protect.clone(),
    });
    Ok(request)
}

#[cfg(test)]
mod tests;
