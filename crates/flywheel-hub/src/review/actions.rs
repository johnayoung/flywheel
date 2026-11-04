//! Running a confirmed plan: one action per repository at a time, the
//! fingerprint re-checked, every command's output kept.

use super::integrate::{PrRequest, plan_abort, plan_merge, plan_pr, plan_rebase};
use super::model::{nul_list, operation, read};
use super::plan::{Outcome, Plan, Planned, Ran, plan_commit, plan_push, staged_as_planned};
use super::runner;
use super::runner::{Output, RunError, Spec};
use super::{
    ActionError, Git, MAX_MESSAGE_BYTES, MAX_TITLE_CHARS, RAN_OUTPUT_BYTES, READ_LIMIT, Target,
    Tools,
};
use crate::util::plural;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// Main checkouts (git common dirs) an action is running in: one at a
/// time per repository.
static BUSY: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub(super) struct RepoLock(PathBuf);

impl RepoLock {
    pub(super) fn take(tools: &Tools, dir: &Path) -> Result<Self, ActionError> {
        let key = Git::new(tools, dir)
            .maybe(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .map(PathBuf::from)
            .unwrap_or_else(|| dir.to_path_buf());
        let key = key.canonicalize().unwrap_or(key);
        let mut busy = BUSY.lock().unwrap_or_else(PoisonError::into_inner);
        if busy.contains(&key) {
            return Err(ActionError::Conflict(
                "another review action is running in this repository; wait for it".into(),
            ));
        }
        busy.push(key.clone());
        Ok(Self(key))
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        BUSY.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|k| k != &self.0);
    }
}

/// Check a confirmed plan before running it.
fn confirmed(plan: &Plan, confirm: &str) -> Result<(), ActionError> {
    if let Some(why) = &plan.refusal {
        return Err(ActionError::Conflict(why.clone()));
    }
    if confirm.trim().is_empty() {
        return Err(ActionError::Invalid(
            "confirm the plan: pass the fingerprint a dry run returned".into(),
        ));
    }
    if plan.fingerprint != confirm.trim() {
        return Err(ActionError::Conflict(
            "the repository changed since this was planned: review it again and confirm the new plan".into(),
        ));
    }
    Ok(())
}

fn ran(command: &Planned, output: &Output) -> Ran {
    let mut text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let err = output.stderr.trim();
    if !err.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(err);
    }
    if text.len() > RAN_OUTPUT_BYTES {
        let mut cut = RAN_OUTPUT_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[cut]");
    }
    Ran {
        display: command.display.clone(),
        code: output.code,
        output: text,
    }
}

fn exec(command: &Planned) -> Result<Output, RunError> {
    runner::run(&Spec {
        program: &command.program,
        dir: &command.dir,
        args: command.args.clone(),
        stdin: command.stdin.clone(),
        timeout: command.timeout,
        limit: READ_LIMIT,
    })
}

/// Run `plan`'s commands in order, stopping at the first failure (an
/// error naming the command and what it said).
pub(super) fn run_plan(commands: &[Planned]) -> Result<Vec<(Ran, Output)>, ActionError> {
    let mut done = Vec::new();
    for command in commands {
        let output = exec(command)?;
        let record = ran(command, &output);
        if !output.success() {
            let how = if output.timed_out {
                "timed out"
            } else {
                "failed"
            };
            let before = if done.is_empty() {
                String::new()
            } else {
                format!(
                    " (after {})",
                    done.iter()
                        .map(|(r, _): &(Ran, Output)| r.display.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            return Err(ActionError::Conflict(format!(
                "{} {how}{before}: {}",
                command.display,
                output.error_text()
            )));
        }
        done.push((record, output));
    }
    Ok(done)
}

fn head_of(tools: &Tools, dir: &Path) -> Option<String> {
    Git::new(tools, dir).maybe(&["rev-parse", "--verify", "-q", "HEAD^{commit}"])
}

/// Planning itself failed (git could not read the repository).
fn unplannable(e: String) -> ActionError {
    ActionError::Conflict(e)
}

/// Commit every change with `message`, as confirmed.
pub fn commit(
    tools: &Tools,
    target: &Target,
    message: &str,
    exclude: Option<&[String]>,
    confirm: &str,
) -> Result<Outcome, ActionError> {
    check_message(message)?;
    let _lock = RepoLock::take(tools, &target.dir)?;
    let plan = plan_commit(tools, target, message, exclude).map_err(unplannable)?;
    confirmed(&plan, confirm)?;
    let done = run_staged_as_planned(tools, target, &plan)?;
    let head = head_of(tools, &target.dir);
    let short = head.as_deref().map_or("?", |h| &h[..h.len().min(10)]);
    // What the commit holds, as git recorded it.
    let files = Git::new(tools, &target.dir)
        .raw(
            &[
                "diff-tree",
                "--no-commit-id",
                "--root",
                "-r",
                "--name-only",
                "-z",
                "HEAD",
            ],
            READ_LIMIT,
        )
        .map(|o| nul_list(&o.stdout).len())
        .unwrap_or_default();
    Ok(Outcome {
        action: "commit",
        ok: true,
        summary: format!("Committed {} as {short}", plural(files, "file")),
        ran: done.into_iter().map(|(r, _)| r).collect(),
        conflicts: Vec::new(),
        url: None,
        head,
    })
}

/// Run a commit plan (`git add -A`, then `git commit`), committing only
/// what was planned: what `add` staged is checked against the plan's
/// contents first, and on a mismatch the index is put back as it was and
/// nothing is committed.
pub(super) fn run_staged_as_planned(
    tools: &Tools,
    target: &Target,
    plan: &Plan,
) -> Result<Vec<(Ran, Output)>, ActionError> {
    let git = Git::new(tools, &target.dir);
    // Staging (add, and unstaging what is left out), then the commit.
    let (commit, stage) = plan
        .commands
        .split_last()
        .ok_or_else(|| ActionError::Conflict("the commit plan runs nothing".into()))?;
    let saved = git.out(&["write-tree"]).map_err(unplannable)?;
    let mut done = run_plan(stage)?;
    let as_planned = staged_as_planned(&git, &plan.contents, &plan.excluded);
    if !matches!(as_planned, Ok(true)) {
        let restored = git.out(&["read-tree", &saved]).is_ok();
        return Err(ActionError::Conflict(format!(
            "the work tree changed while committing (after the plan was checked): nothing was committed{}; review it again and confirm the new plan",
            if restored {
                ""
            } else {
                ", but the index could not be put back (git reset to unstage)"
            }
        )));
    }
    done.extend(run_plan(std::slice::from_ref(commit))?);
    Ok(done)
}

pub fn check_message(message: &str) -> Result<(), ActionError> {
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(ActionError::Invalid(format!(
            "the message is longer than {} KiB",
            MAX_MESSAGE_BYTES / 1024
        )));
    }
    if message.contains('\0') {
        return Err(ActionError::Invalid("the message holds a NUL byte".into()));
    }
    Ok(())
}

/// Push the branch, as confirmed.
pub fn push(
    tools: &Tools,
    target: &Target,
    remote: Option<&str>,
    confirm: &str,
) -> Result<Outcome, ActionError> {
    let _lock = RepoLock::take(tools, &target.dir)?;
    let plan = plan_push(tools, target, remote).map_err(unplannable)?;
    confirmed(&plan, confirm)?;
    let done = run_plan(&plan.commands)?;
    Ok(Outcome {
        action: "push",
        ok: true,
        summary: plan.summary.replacen("Push ", "Pushed ", 1),
        ran: done.into_iter().map(|(r, _)| r).collect(),
        conflicts: Vec::new(),
        url: None,
        head: head_of(tools, &target.dir),
    })
}

/// Open the pull request, as confirmed: its URL.
pub fn pr(
    tools: &Tools,
    target: &Target,
    request: &PrRequest,
    confirm: &str,
) -> Result<Outcome, ActionError> {
    if let Some(title) = &request.title
        && (title.chars().count() > MAX_TITLE_CHARS || title.contains(['\n', '\r', '\0']))
    {
        return Err(ActionError::Invalid(format!(
            "the title must be one line of at most {MAX_TITLE_CHARS} characters"
        )));
    }
    if let Some(body) = &request.body {
        check_message(body)?;
    }
    let _lock = RepoLock::take(tools, &target.dir)?;
    let plan = plan_pr(tools, target, request).map_err(unplannable)?;
    confirmed(&plan, confirm)?;
    let done = run_plan(&plan.commands)?;
    let url = done.last().and_then(|(_, o)| {
        o.stdout_text()
            .lines()
            .rev()
            .find(|l| l.trim().starts_with("https://") || l.trim().starts_with("http://"))
            .map(|l| l.trim().to_owned())
    });
    Ok(Outcome {
        action: "pr",
        ok: true,
        summary: match &url {
            Some(url) => format!("Opened {url}"),
            None => "Opened the pull request".into(),
        },
        ran: done.into_iter().map(|(r, _)| r).collect(),
        conflicts: Vec::new(),
        url,
        head: head_of(tools, &target.dir),
    })
}

/// Rebase onto the base, as confirmed. A conflict is an outcome (not an
/// error): the rebase is left stopped, `conflicts` names the files.
pub fn rebase(tools: &Tools, target: &Target, confirm: &str) -> Result<Outcome, ActionError> {
    let _lock = RepoLock::take(tools, &target.dir)?;
    let plan = plan_rebase(tools, target).map_err(unplannable)?;
    confirmed(&plan, confirm)?;
    let ahead_before = read(tools, target).map(|r| r.ahead).ok();
    let command = &plan.commands[0];
    let output = exec(command)?;
    let record = ran(command, &output);
    let head = head_of(tools, &target.dir);
    if output.success() {
        let ahead_after = read(tools, target).map(|r| r.ahead).ok();
        return Ok(Outcome {
            action: "rebase",
            ok: true,
            summary: rebased_summary(&plan.summary, ahead_before, ahead_after, target),
            ran: vec![record],
            conflicts: Vec::new(),
            url: None,
            head,
        });
    }
    let git = Git::new(tools, &target.dir);
    let git_dir = git
        .maybe(&["rev-parse", "--absolute-git-dir"])
        .map(PathBuf::from);
    let stopped = git_dir.as_deref().and_then(operation).as_deref() == Some("rebase");
    if !stopped {
        return Err(ActionError::Conflict(format!(
            "{} failed: {}",
            command.display,
            output.error_text()
        )));
    }
    let conflicts = git
        .raw(
            &["diff", "--name-only", "-z", "--diff-filter=U"],
            READ_LIMIT,
        )
        .map(|o| nul_list(&o.stdout))
        .unwrap_or_default();
    Ok(Outcome {
        action: "rebase",
        ok: false,
        summary: if conflicts.is_empty() {
            "The rebase stopped (left in progress): see its output".into()
        } else {
            format!(
                "The rebase stopped on {} in {} (left in progress): resolve, git add, git rebase --continue",
                plural(conflicts.len(), "conflict"),
                conflicts.join(", ")
            )
        },
        ran: vec![record],
        conflicts,
        url: None,
        head,
    })
}

/// What a rebase that went through did: git drops a commit whose change
/// the base already has, so "replayed N" is said only of what is left.
fn rebased_summary(
    planned: &str,
    before: Option<u32>,
    after: Option<u32>,
    target: &Target,
) -> String {
    let done = planned.replacen("Replay ", "Replayed ", 1);
    let (Some(before), Some(after)) = (before, after) else {
        return done;
    };
    if after >= before {
        return done;
    }
    let base = target
        .worktree
        .as_ref()
        .map_or("the base", |w| w.base.as_str());
    let dropped = before - after;
    let already = format!(
        "{} already in {base} and {} dropped",
        if dropped == 1 {
            "1 commit was".to_owned()
        } else {
            format!("{dropped} commits were")
        },
        if dropped == 1 { "was" } else { "were" }
    );
    if after == 0 {
        format!("Rebased onto {base}: {already}; the branch now equals {base}")
    } else {
        format!(
            "Rebased onto {base}: {already}; {} replayed",
            plural(after as usize, "commit")
        )
    }
}

/// Abort the stopped rebase, as confirmed.
pub fn abort(tools: &Tools, target: &Target, confirm: &str) -> Result<Outcome, ActionError> {
    let _lock = RepoLock::take(tools, &target.dir)?;
    let plan = plan_abort(tools, target).map_err(unplannable)?;
    confirmed(&plan, confirm)?;
    let done = run_plan(&plan.commands)?;
    Ok(Outcome {
        action: "abort",
        ok: true,
        summary: plan.summary.replacen("Abort ", "Aborted ", 1),
        ran: done.into_iter().map(|(r, _)| r).collect(),
        conflicts: Vec::new(),
        url: None,
        head: head_of(tools, &target.dir),
    })
}

/// Fast-forward the base in the main checkout, as confirmed.
pub fn merge(tools: &Tools, target: &Target, confirm: &str) -> Result<Outcome, ActionError> {
    let _lock = RepoLock::take(tools, &target.dir)?;
    let plan = plan_merge(tools, target).map_err(unplannable)?;
    confirmed(&plan, confirm)?;
    let done = run_plan(&plan.commands)?;
    let main = plan.commands[0].dir.clone();
    Ok(Outcome {
        action: "merge",
        ok: true,
        summary: plan.summary.replacen("Fast-forward ", "Fast-forwarded ", 1),
        ran: done.into_iter().map(|(r, _)| r).collect(),
        conflicts: Vec::new(),
        url: None,
        head: head_of(tools, &main),
    })
}
