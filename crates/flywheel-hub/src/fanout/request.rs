//! Requests: validating a new fan-out, grader pins and tamper checks,
//! names, and the "ask to fix" prompt.

use super::promote;
use super::promote::TaskOrigin;
use super::{
    DEFAULT_TIMEOUT_SECS, Grade, GraderInput, GraderRun, GraderSpec, MAX_CANDIDATES,
    MAX_GRADER_BYTES, MAX_GRADER_NAME_CHARS, MAX_GRADERS, MAX_PIN_BYTES, MAX_PROTECT,
    MAX_TIMEOUT_SECS, MIN_CANDIDATES, Pin,
};
use crate::agent::{AgentLaunch, SessionKind};
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

/// One agent to launch.
#[derive(Debug, Clone, Deserialize)]
pub struct CandidateSpec {
    pub kind: SessionKind,
    #[serde(flatten)]
    pub launch: AgentLaunch,
}

/// `POST /api/fanouts`.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateFanout {
    pub workspace_id: String,
    /// One of the workspace's git folders (default: the first git one).
    #[serde(default, alias = "repo")]
    pub folder: Option<PathBuf>,
    /// The base branch (default: the folder's current branch).
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub prompt: String,
    pub candidates: Vec<CandidateSpec>,
    pub graders: Vec<GraderInput>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub protect: Vec<String>,
    #[serde(default = "default_true")]
    pub commit_instruction: bool,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    /// Seed from this flywheel task of `folder` (its directory source):
    /// the server re-reads the task and its held-out graders at launch
    /// (see `promote::seeded`); `prompt` and `graders` may then be empty.
    #[serde(default)]
    pub from_task: Option<String>,
    /// Set by the server from `from_task`, never by a request.
    #[serde(skip)]
    pub origin: Option<TaskOrigin>,
}

pub(crate) fn default_true() -> bool {
    true
}

/// A [`CreateFanout`] checked: what the engine launches.
#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    pub name: String,
    pub prompt: String,
    pub candidates: Vec<(SessionKind, AgentLaunch)>,
    pub graders: Vec<GraderSpec>,
    pub timeout_secs: u64,
    pub protect: Vec<String>,
}

impl CreateFanout {
    /// Every field checked and normalized (no git or disk access).
    pub fn validated(&self) -> Result<Validated, String> {
        if self.from_task.is_some() && self.origin.is_none() {
            return Err("a fan-out from a flywheel task is seeded by the server first".into());
        }
        let prompt = crate::agent::validate_prompt(Some(&self.prompt))?
            .filter(|p| !p.trim().is_empty())
            .ok_or("a fan-out needs a prompt: the task every agent gets")?;
        if !(MIN_CANDIDATES..=MAX_CANDIDATES).contains(&self.candidates.len()) {
            return Err(format!(
                "a fan-out runs {MIN_CANDIDATES} to {MAX_CANDIDATES} agents, not {}",
                self.candidates.len()
            ));
        }
        let mut candidates = Vec::with_capacity(self.candidates.len());
        for (i, spec) in self.candidates.iter().enumerate() {
            if !spec.kind.is_agent() {
                return Err(format!(
                    "candidate {}: a fan-out runs agents (claude or codex), not a {}",
                    i + 1,
                    spec.kind.as_str()
                ));
            }
            let launch = spec
                .launch
                .validated(spec.kind)
                .map_err(|e| format!("candidate {}: {e}", i + 1))?;
            candidates.push((spec.kind, launch));
        }
        if self.graders.is_empty() {
            return Err(
                "a fan-out needs at least one grader command: it is what verifies the agents"
                    .into(),
            );
        }
        if self.graders.len() > MAX_GRADERS {
            return Err(format!("at most {MAX_GRADERS} graders"));
        }
        let graders = self
            .graders
            .iter()
            .enumerate()
            .map(|(i, input)| validate_grader(input).map_err(|e| format!("grader {}: {e}", i + 1)))
            .map(|grader| {
                // Only a task's held-out registry makes a grader held out
                // (`promote::seeded`), never a client's say-so.
                grader.map(|mut g| {
                    g.held_out &= self.origin.is_some();
                    g
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let timeout_secs = self.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        if !(1..=MAX_TIMEOUT_SECS).contains(&timeout_secs) {
            return Err(format!(
                "the grader timeout is 1 to {MAX_TIMEOUT_SECS} seconds, not {timeout_secs}"
            ));
        }
        if self.protect.len() > MAX_PROTECT {
            return Err(format!("at most {MAX_PROTECT} protected paths"));
        }
        let mut protect: Vec<String> = Vec::new();
        for raw in &self.protect {
            let path = validate_protect(raw)?;
            if !protect.contains(&path) {
                protect.push(path);
            }
        }
        let name = match self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            Some(name) => crate::state::validate_name(name).map_err(|e| e.to_string())?,
            None => default_name(&prompt),
        };
        Ok(Validated {
            name,
            prompt,
            candidates,
            graders,
            timeout_secs,
            protect,
        })
    }
}

fn validate_grader(input: &GraderInput) -> Result<GraderSpec, String> {
    let spec = match input {
        GraderInput::Run(run) => GraderSpec {
            run: run.clone(),
            name: None,
            held_out: false,
        },
        GraderInput::Spec(spec) => spec.clone(),
    };
    let run = spec.run.trim().to_owned();
    if run.is_empty() {
        return Err("the command is empty".into());
    }
    if run.len() > MAX_GRADER_BYTES {
        return Err(format!(
            "the command is longer than {MAX_GRADER_BYTES} bytes"
        ));
    }
    if run.contains('\0') {
        return Err("the command contains a NUL byte".into());
    }
    let name = match spec
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        None => None,
        Some(name) if name.chars().count() > MAX_GRADER_NAME_CHARS => {
            return Err(format!(
                "its name is longer than {MAX_GRADER_NAME_CHARS} characters"
            ));
        }
        Some(name) if name.chars().any(char::is_control) => {
            return Err("its name contains control characters".into());
        }
        Some(name) => Some(name.to_owned()),
    };
    Ok(GraderSpec {
        run,
        name,
        held_out: spec.held_out,
    })
}

/// A protected path: relative to the repo, no `..`, normalized.
fn validate_protect(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed.len() > 256 {
        return Err(format!("protected path {raw:?} is empty or too long"));
    }
    let path = Path::new(trimmed);
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "protected path {raw:?} must be relative to the repo, without `..`"
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(format!("protected path {raw:?} names the whole repo"));
    }
    Ok(parts.join("/"))
}

/// The words of a grader command that could name a file: whitespace-split,
/// quotes and a leading `NAME=` assignment stripped.
fn path_words(run: &str) -> impl Iterator<Item = &str> {
    run.split_whitespace()
        .map(|w| w.trim_matches(|c| c == '\'' || c == '"' || c == ';' || c == '('))
        .map(|w| match w.split_once('=') {
            Some((name, value))
                if !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                value
            }
            _ => w,
        })
        .filter(|w| !w.is_empty())
}

fn sha256_file(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    Some(
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// What a fan-out's graders lean on that a candidate could change: every
/// absolute path a command names that is a regular file of at most
/// [`MAX_PIN_BYTES`] (a script) is pinned by its hash; every relative path
/// (with a `/`) naming a file in `repo` joins `protect` (each worktree has
/// its own copy, compared against the base like any protected path).
pub fn pin_graders(
    graders: &[GraderSpec],
    repo: &Path,
    mut protect: Vec<String>,
) -> Result<(Vec<Pin>, Vec<String>), String> {
    let mut pins: Vec<Pin> = Vec::new();
    for grader in graders {
        for word in path_words(&grader.run) {
            let path = Path::new(word);
            if path.is_absolute() {
                let Ok(meta) = std::fs::metadata(path) else {
                    continue;
                };
                if !meta.is_file() || meta.len() > MAX_PIN_BYTES {
                    continue;
                }
                let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
                if let Some(pin) = pins.iter_mut().find(|p| p.path == canonical) {
                    pin.held_out |= grader.held_out;
                    continue;
                }
                let sha256 = sha256_file(&canonical)
                    .ok_or_else(|| format!("grader script {word}: cannot be read"))?;
                pins.push(Pin {
                    path: canonical,
                    sha256,
                    held_out: grader.held_out,
                });
            } else if word.contains('/')
                && repo.join(path).is_file()
                && let Ok(relative) = validate_protect(word)
                && !protect.contains(&relative)
                && protect.len() < MAX_PROTECT
            {
                protect.push(relative);
            }
        }
    }
    Ok((pins, protect))
}

/// Why the pinned grader scripts no longer are what the fan-out started
/// with (`None`: every one hashes the same).
/// A held-out grader's script is named only as such.
pub fn pins_broken(pins: &[Pin]) -> Option<String> {
    let broken: Vec<&Pin> = pins
        .iter()
        .filter(|p| sha256_file(&p.path).as_deref() != Some(p.sha256.as_str()))
        .collect();
    if broken.is_empty() {
        return None;
    }
    let named: Vec<String> = broken
        .iter()
        .filter(|p| !p.held_out)
        .map(|p| p.path.display().to_string())
        .collect();
    let held = broken.len() - named.len();
    let mut what = Vec::new();
    if !named.is_empty() {
        what.push(format!(
            "grader script{} {}",
            if named.len() == 1 { "" } else { "s" },
            named.join(", ")
        ));
    }
    if held > 0 {
        what.push(format!(
            "{held} held-out grader script{}",
            if held == 1 { "" } else { "s" }
        ));
    }
    Some(format!(
        "{} changed (or went) since the fan-out started: the check is no longer the one every \
         candidate is held to; restore it, or start a new fan-out",
        what.join(" and ")
    ))
}

/// `text` with every held-out grader command of `graders`, and the paths
/// and script names in it, replaced: what an agent may be shown.
fn redact_held_out(text: &str, graders: &[GraderRun]) -> String {
    let mut words: Vec<&str> = graders
        .iter()
        .filter(|g| g.held_out)
        .flat_map(|g| promote::secret_words(&g.run))
        .collect();
    // Longest first: a command before the paths inside it.
    words.sort_by_key(|w| std::cmp::Reverse(w.len()));
    let mut text = text.to_owned();
    for word in words {
        text = text.replace(word, "[held-out]");
    }
    text
}

/// The grading error for tracked files flagged out of git status's sight.
pub fn hidden_error(hidden: &[String]) -> String {
    let shown: Vec<&str> = hidden.iter().take(5).map(String::as_str).collect();
    format!(
        "{} tracked file{} hidden from git status (skip-worktree or assume-unchanged): {}{}; \
         clear the flags (git update-index --no-skip-worktree --no-assume-unchanged <path>) \
         and commit",
        hidden.len(),
        if hidden.len() == 1 { " is" } else { "s are" },
        shown.join(", "),
        if hidden.len() > shown.len() {
            ", ..."
        } else {
            ""
        }
    )
}

/// The protected paths `changed` touches (a protected directory covers
/// everything under it).
pub fn tampered(protect: &[String], changed: &[String]) -> Vec<String> {
    let mut hit: Vec<String> = Vec::new();
    for path in changed {
        let path = path.trim_end_matches('/');
        if protect.iter().any(|p| {
            path == p
                || path
                    .strip_prefix(p.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        }) && !hit.iter().any(|h| h == path)
        {
            hit.push(path.to_owned());
        }
    }
    hit.sort();
    hit
}

/// `name`, or `name-2`, `name-3`, ... when `taken` has it already.
pub(crate) fn unique_name(name: &str, taken: &[&str]) -> String {
    if !taken.contains(&name) {
        return name.to_owned();
    }
    (2..)
        .map(|n| {
            let suffix = format!("-{n}");
            // Within the name limit, the number kept.
            let keep = crate::state::MAX_NAME_CHARS.saturating_sub(suffix.len());
            let stem: String = name.chars().take(keep).collect();
            format!("{stem}{suffix}")
        })
        .find(|candidate| !taken.contains(&candidate.as_str()))
        .unwrap_or_else(|| name.to_owned())
}

/// A fan-out named after its prompt's first words.
fn default_name(prompt: &str) -> String {
    let mut name = String::new();
    for word in prompt.split_whitespace().take(8) {
        let word: String = word.chars().filter(|c| !c.is_control()).collect();
        if name.chars().count() + word.chars().count() + 1 > 48 {
            break;
        }
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(&word);
    }
    if name.is_empty() {
        "fan-out".to_owned()
    } else {
        name
    }
}

/// The worktree task (directory and branch) name of a candidate:
/// `<slug>-<tag>-<label>` within [`crate::git::validate_task`]'s rules.
pub fn task_name(name: &str, tag: &str, label: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 32 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "fanout" } else { slug };
    format!("{slug}-{tag}-{label}")
}

/// The prompt "ask candidate to fix" pastes: what failed and its output
/// (`None`: it passes, or the failure is the operator's to fix).
pub fn fix_prompt(grade: &Grade) -> Option<String> {
    if grade.passed || grade.operator {
        return None;
    }
    if let Some(error) = &grade.error {
        let error = redact_held_out(error, &grade.graders);
        return Some(format!(
            "The hub could not grade your work in this worktree: {error}. Check that the \
             worktree is in a usable state (no rebase or merge left half-way), then commit \
             your changes."
        ));
    }
    if !grade.tampered.is_empty() {
        return Some(format!(
            "Your changes touch files that must stay as they are: {}. Restore them to their \
             original contents (git checkout <base> -- <path>, or revert the change), keep \
             the rest of your work, and commit.",
            grade.tampered.join(", ")
        ));
    }
    let failed = grade.first_failure()?;
    if failed.held_out {
        // A held-out check's command and output are never the agent's to
        // see (flywheel's held-out gate): only that one failed.
        return Some(format!(
            "A held-out check the hub ran on your work in this worktree failed ({outcome}). \
             Held-out checks and their output are not shown to you. Re-read the task, make \
             sure your work fully does what it asks (edge cases included), then commit your \
             changes.",
            outcome = failed.outcome(),
        ));
    }
    let mut output = String::new();
    for (stream, text) in [("stdout", &failed.stdout), ("stderr", &failed.stderr)] {
        let text = text.trim_end();
        if text.is_empty() {
            continue;
        }
        let tail = tail_from_line_start(text, 3000);
        output.push_str(&format!("--- {stream} (last lines) ---\n{tail}\n"));
    }
    if output.is_empty() {
        output.push_str("(it printed nothing)\n");
    }
    let fence = if output.contains("```") {
        "~~~~"
    } else {
        "```"
    };
    Some(format!(
        "A check the hub ran on your work in this worktree failed.\n\n\
         $ {run}\n{outcome}\n\n{fence}\n{output}{fence}\n\n\
         Fix the code so this check passes. Do not change the check itself or what it \
         reads to decide; fix the work it checks. Then commit your changes.",
        run = failed.run,
        outcome = failed.outcome(),
    ))
}

/// The last `max` characters of `text`, from a line start where one is
/// near.
pub(crate) fn tail_from_line_start(text: &str, max: usize) -> &str {
    let count = text.chars().count();
    if count <= max {
        return text;
    }
    let start = text.char_indices().nth(count - max).map_or(0, |(i, _)| i);
    let cut = &text[start..];
    match cut.find('\n') {
        Some(nl) if nl < 200 => &cut[nl + 1..],
        _ => cut,
    }
}
