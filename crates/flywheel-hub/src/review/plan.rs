//! Dry runs: a plan's exact commands and the fingerprint of the state it
//! was planned against; planning a commit and a push.

use super::model::{FileChange, Remote, left_right, read};
use super::runner;
use super::{COMMIT_TIMEOUT, Git, MAX_FILES, PUSH_TIMEOUT, READ_LIMIT, Target, Tools};
use crate::util::plural;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One command an action runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Planned {
    /// Where it runs.
    pub dir: PathBuf,
    pub program: String,
    pub args: Vec<String>,
    /// How it reads in a shell (quoted), with what it reads on stdin
    /// named after `<`.
    pub display: String,
    #[serde(skip)]
    pub(super) stdin: Option<Vec<u8>>,
    #[serde(skip)]
    pub(super) timeout: Duration,
}

impl Planned {
    pub(super) fn new(program: &str, dir: &Path, args: Vec<String>, timeout: Duration) -> Self {
        let dir_text = dir.to_string_lossy().into_owned();
        let name = program_name(program);
        let shown = if name == "git" {
            let mut with_dir = vec!["-C".to_owned(), dir_text];
            with_dir.extend(args.iter().cloned());
            runner::display("git", &with_dir)
        } else {
            format!(
                "(cd {} && {})",
                runner::quote(&dir_text),
                runner::display(name, &args)
            )
        };
        Self {
            dir: dir.to_path_buf(),
            program: program.to_owned(),
            args,
            display: shown,
            stdin: None,
            timeout,
        }
    }

    pub(super) fn with_stdin(mut self, bytes: Vec<u8>, what: &str) -> Self {
        self.display = format!("{} < {what}", self.display);
        self.stdin = Some(bytes);
        self
    }
}

/// `gh` for showing, whatever path it was resolved at.
fn program_name(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program)
}

/// What an action would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub action: &'static str,
    /// One line: what happens.
    pub summary: String,
    /// What it touches: files committed, commits pushed or merged.
    pub details: Vec<String>,
    /// Worth knowing, not stopping it.
    pub warnings: Vec<String>,
    /// Run in order; the first failure stops the rest.
    pub commands: Vec<Planned>,
    /// Hand back to run it: the state it was planned against.
    pub fingerprint: String,
    /// Why it cannot run now (`commands` show what it would run).
    pub refusal: Option<String>,
    /// Merge: the branch is not a fast-forward of the base; rebase first.
    pub needs_rebase: bool,
    /// Pull request: the title and body offered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defaults: Option<PrDefaults>,
    /// Commit: the changed paths and what each held when planned.
    #[serde(skip)]
    pub contents: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrDefaults {
    pub title: String,
    pub body: String,
    pub base: String,
}

/// One command an action ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ran {
    pub display: String,
    pub code: Option<i32>,
    /// stdout then stderr, trimmed (at most 8 KiB).
    pub output: String,
}

/// What an action did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub action: &'static str,
    /// It did what was planned (a rebase stopped on conflicts did not).
    pub ok: bool,
    pub summary: String,
    pub ran: Vec<Ran>,
    /// A rebase stopped on these; it is left in progress.
    pub conflicts: Vec<String>,
    /// The pull request opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// HEAD of the directory acted on, afterwards.
    pub head: Option<String>,
}

pub(super) fn fingerprint(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part.as_bytes());
        digest.update([0]);
    }
    digest.finalize()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Everything that makes a work tree's state: HEAD, branch, index, every
/// change (untracked files included) and what each changed file holds
/// now, so an edit after the review -- even one keeping the same line
/// counts -- makes the plan stale.
pub(super) fn tree_state(git: &Git<'_>) -> Result<String, String> {
    Ok(tree_snapshot(git)?.0)
}

/// [`tree_state`], and the changed paths with what each holds (a commit
/// checks what it staged against these).
fn tree_snapshot(git: &Git<'_>) -> Result<(String, Vec<(String, String)>), String> {
    let status = porcelain(git, &["--branch"])?;
    let mut state = String::from_utf8_lossy(&status).into_owned();
    let contents = path_contents(git, &status);
    for (path, hash) in &contents {
        state.push('\u{1}');
        state.push_str(path);
        state.push('\u{2}');
        state.push_str(hash);
    }
    Ok((state, contents))
}

/// `git status --porcelain=v2 -z` with untracked files, renames off (each
/// changed path is its own entry, staged or not).
fn porcelain(git: &Git<'_>, extra: &[&str]) -> Result<Vec<u8>, String> {
    let mut args = vec![
        "status",
        "--porcelain=v2",
        "-z",
        "--untracked-files=all",
        "--no-renames",
    ];
    args.extend_from_slice(extra);
    let status = git.raw(&args, READ_LIMIT)?;
    if !status.success() {
        return Err(status.error_text());
    }
    if status.truncated {
        return Err("too many changes to plan against (git status is cut)".into());
    }
    Ok(status.stdout)
}

/// Each path `status` names, with what the work tree holds there.
fn path_contents(git: &Git<'_>, status: &[u8]) -> Vec<(String, String)> {
    status_paths(status)
        .into_iter()
        .map(|path| {
            let hash = content_hash(&git.dir.join(&path));
            (path, hash)
        })
        .collect()
}

/// After `git add -A`: the index holds exactly what the commit was
/// planned with -- the same paths changed, each file's content as planned,
/// nothing left unstaged or untracked. A change landing between the plan
/// and the staging (an agent still at work) fails this.
pub(super) fn staged_as_planned(
    git: &Git<'_>,
    planned: &[(String, String)],
) -> Result<bool, String> {
    let status = porcelain(git, &[])?;
    let fully_staged = status
        .split(|b| *b == 0)
        .filter(|t| !t.is_empty())
        .all(|entry| match entry.first() {
            // "1 XY ...": Y is the work tree against the index.
            Some(b'1') => entry.get(3) == Some(&b'.'),
            _ => false,
        });
    if !fully_staged {
        return Ok(false);
    }
    let mut now = path_contents(git, &status);
    let mut planned = planned.to_vec();
    now.sort();
    planned.sort();
    Ok(now == planned)
}

/// The work-tree paths `git status --porcelain=v2 -z` names (changed,
/// unmerged, untracked).
pub(super) fn status_paths(status: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut tokens = status.split(|b| *b == 0).filter(|t| !t.is_empty());
    while let Some(entry) = tokens.next() {
        let entry = String::from_utf8_lossy(entry);
        // Fields before the path: 1 -> 8, 2 -> 9 (then the source path as
        // its own token), u -> 10, ? -> 1.
        let (fields, renamed) = match entry.as_bytes().first() {
            Some(b'1') => (8, false),
            Some(b'2') => (9, true),
            Some(b'u') => (10, false),
            Some(b'?') => (1, false),
            _ => continue,
        };
        if let Some(path) = entry.splitn(fields + 1, ' ').nth(fields) {
            paths.push(path.to_owned());
        }
        if renamed {
            tokens.next();
        }
    }
    paths
}

/// Files larger than this are known by size and modification time, not
/// read through.
const HASH_FILE_LIMIT: u64 = 64 * 1024 * 1024;

/// What a file holds: a SHA-256 of its bytes (a symlink: of its target;
/// a huge file: its size and modification time), or why it has none
/// (gone, a directory such as a submodule).
fn content_hash(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let Ok(meta) = path.symlink_metadata() else {
        return "gone".into();
    };
    let mut digest = Sha256::new();
    if meta.file_type().is_symlink() {
        match std::fs::read_link(path) {
            Ok(to) => digest.update(to.to_string_lossy().as_bytes()),
            Err(_) => return "unreadable link".into(),
        }
    } else if meta.is_file() && meta.len() > HASH_FILE_LIMIT {
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        return format!("large {} {modified}", meta.len());
    } else if meta.is_file() {
        let Ok(mut file) = std::fs::File::open(path) else {
            return format!("unreadable {}", meta.len());
        };
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => digest.update(&buf[..n]),
                Err(_) => return format!("unreadable {}", meta.len()),
            }
        }
    } else {
        return "not a file".into();
    }
    digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn change_line(prefix: &str, f: &FileChange) -> String {
    let counts = match (f.insertions, f.deletions) {
        (Some(a), Some(d)) => format!(" +{a} -{d}"),
        _ if f.binary => " binary".to_owned(),
        _ => String::new(),
    };
    match &f.old_path {
        Some(old) => format!("{prefix}{} {old} -> {}{counts}", f.status, f.path),
        None => format!("{prefix}{} {}{counts}", f.status, f.path),
    }
}

/// Whether a new file looks like a build or tool artifact (bytecode,
/// objects, dependency and cache directories) rather than source.
fn looks_generated(path: &str) -> bool {
    const DIRS: &[&str] = &[
        "__pycache__",
        "node_modules",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        ".tox",
        ".venv",
        ".gradle",
        ".next",
    ];
    const EXTENSIONS: &[&str] = &["pyc", "pyo", "o", "obj", "class"];
    let parts: Vec<&str> = path.split('/').collect();
    let name = parts.last().copied().unwrap_or(path);
    parts[..parts.len() - 1].iter().any(|d| DIRS.contains(d))
        || name == ".DS_Store"
        || name
            .rsplit_once('.')
            .is_some_and(|(stem, ext)| !stem.is_empty() && EXTENSIONS.contains(&ext))
        || path.starts_with("target/debug/")
        || path.starts_with("target/release/")
}

/// A commit plan's warning about new files that look generated (they are
/// staged with the rest): the first few named.
fn generated_warning<'a>(new_files: impl Iterator<Item = &'a str>) -> Option<String> {
    let generated: Vec<&str> = new_files.filter(|p| looks_generated(p)).collect();
    let first = generated.first()?;
    let more = match generated.len() {
        1 => String::new(),
        n => format!(" and {} more", n - 1),
    };
    Some(format!(
        "{} {first}{more} look{} generated and will be committed too; \
         list {} in .gitignore to leave {} out",
        plural(generated.len(), "new file"),
        if generated.len() == 1 { "s" } else { "" },
        if generated.len() == 1 { "it" } else { "them" },
        if generated.len() == 1 { "it" } else { "them" },
    ))
}

/// Plan committing every change (staged, unstaged, untracked) with
/// `message`.
pub fn plan_commit(tools: &Tools, target: &Target, message: &str) -> Result<Plan, String> {
    let git = Git::new(tools, &target.dir);
    let review = read(tools, target)?;
    let (state, contents) = tree_snapshot(&git)?;
    let mut refusal = None;
    if let Some(op) = &review.operation {
        refusal = Some(format!(
            "a {op} is in progress in {}; finish or abort it in the terminal first",
            target.dir.display()
        ));
    } else if !review.conflicts.is_empty() {
        refusal = Some(format!(
            "{} unresolved: {}",
            plural(review.conflicts.len(), "conflict"),
            review.conflicts.join(", ")
        ));
    } else if review.clean() {
        refusal = Some("nothing to commit: the work tree is clean".into());
    } else if message.trim().is_empty() {
        refusal = Some("write a commit message".into());
    }
    let mut details = Vec::new();
    for f in &review.staged.files {
        details.push(change_line("staged ", f));
    }
    for f in &review.unstaged.files {
        details.push(change_line("", f));
    }
    for u in &review.untracked {
        details.push(format!("new {}", u.path));
    }
    // A file both staged and changed again since is one file.
    let files = review
        .staged
        .files
        .iter()
        .chain(&review.unstaged.files)
        .map(|f| f.path.as_str())
        .chain(review.untracked.iter().map(|u| u.path.as_str()))
        .collect::<HashSet<_>>()
        .len();
    let branch = review
        .branch
        .clone()
        .unwrap_or_else(|| "a detached HEAD".into());
    let mut warnings = Vec::new();
    if review.branch.is_none() {
        warnings.push("HEAD is detached: the commit is on no branch".into());
    }
    if review.untracked_truncated || review.unstaged.truncated || review.staged.truncated {
        warnings.push(format!(
            "more than {MAX_FILES} files change; the list is cut"
        ));
    }
    warnings.extend(generated_warning(
        review.untracked.iter().map(|u| u.path.as_str()),
    ));
    let dir = &target.dir;
    let add = Planned::new(
        &tools.git,
        dir,
        vec!["add".into(), "-A".into()],
        COMMIT_TIMEOUT,
    );
    let commit = Planned::new(
        &tools.git,
        dir,
        vec!["commit".into(), "-F".into(), "-".into()],
        COMMIT_TIMEOUT,
    )
    .with_stdin(message.as_bytes().to_vec(), "the message");
    Ok(Plan {
        action: "commit",
        summary: format!(
            "Stage all {} and commit them on {branch}",
            plural(files, "file")
        ),
        details,
        warnings,
        commands: vec![add, commit],
        fingerprint: fingerprint(&["commit", &state, message]),
        refusal,
        needs_rebase: false,
        defaults: None,
        contents,
    })
}

/// Where a branch is pushed.
pub(super) struct PushTo {
    pub(super) remote: String,
    /// Always the same-named branch there (`refs/heads/<branch>`): a
    /// branch tracking another name (`origin/main`) is never pushed onto
    /// it.
    dest: String,
    /// The branch tracks exactly `remote`/`dest` already.
    tracked: bool,
    /// `-u`: it tracks nothing yet, or another branch on the same remote.
    set_upstream: bool,
    /// What it tracks now when that is not `remote`/`dest`.
    tracking: Option<String>,
}

/// Which remote, and which branch on it, `branch` is pushed to.
pub(super) fn push_destination(
    git: &Git<'_>,
    branch: &str,
    remotes: &[Remote],
    wanted: Option<&str>,
) -> Result<PushTo, String> {
    let names: Vec<&str> = remotes.iter().map(|r| r.name.as_str()).collect();
    if names.is_empty() {
        return Err("the repository has no remotes: add one (git remote add)".into());
    }
    let config = |key: &str| git.maybe(&["config", "--get", key]);
    let upstream_remote = config(&format!("branch.{branch}.remote"));
    let upstream_merge = config(&format!("branch.{branch}.merge"));
    let remote = match wanted.map(str::trim).filter(|w| !w.is_empty()) {
        Some(w) if names.contains(&w) => w.to_owned(),
        Some(w) => {
            return Err(format!(
                "no remote named {w:?} (remotes: {})",
                names.join(", ")
            ));
        }
        None => config(&format!("branch.{branch}.pushRemote"))
            .or_else(|| config("remote.pushDefault"))
            .or_else(|| upstream_remote.clone())
            .filter(|r| names.contains(&r.as_str()))
            .or_else(|| names.contains(&"origin").then(|| "origin".to_owned()))
            .or_else(|| (names.len() == 1).then(|| names[0].to_owned()))
            .ok_or_else(|| format!("pick a remote: {}", names.join(", ")))?,
    };
    let dest = format!("refs/heads/{branch}");
    let same_remote = upstream_remote.as_deref() == Some(remote.as_str());
    let tracked = same_remote && upstream_merge.as_deref() == Some(dest.as_str());
    let tracking = match (&upstream_remote, &upstream_merge) {
        (Some(r), Some(m)) if !tracked => Some(format!(
            "{r}/{}",
            m.strip_prefix("refs/heads/").unwrap_or(m)
        )),
        _ => None,
    };
    Ok(PushTo {
        set_upstream: !tracked && (upstream_remote.is_none() || same_remote),
        remote,
        dest,
        tracked,
        tracking,
    })
}

/// Plan pushing the branch (never forced) to the same-named branch on
/// its push remote, else its upstream's, else `origin`, else the only
/// remote; `-u` when it tracks nothing there yet.
pub fn plan_push(tools: &Tools, target: &Target, remote: Option<&str>) -> Result<Plan, String> {
    let git = Git::new(tools, &target.dir);
    let review = read(tools, target)?;
    let head = review.head.clone().unwrap_or_default();
    let mut refusal = None;
    let mut commands = Vec::new();
    let mut details = Vec::new();
    let mut summary = "Push the branch".to_owned();
    let mut chosen = String::new();
    match &review.branch {
        None => refusal = Some("HEAD is detached: there is no branch to push".into()),
        Some(_) if review.head.is_none() => {
            refusal = Some("the branch has no commits yet".into());
        }
        Some(branch) => match push_destination(&git, branch, &review.remotes, remote) {
            Err(why) => refusal = Some(why),
            Ok(to) => {
                let name = &to.remote;
                // The commit planned, not the branch: one the agent adds
                // after the plan is not pushed unreviewed.
                let args = vec![
                    "push".to_owned(),
                    name.clone(),
                    format!("{head}:{}", to.dest),
                ];
                commands.push(Planned::new(&tools.git, &target.dir, args, PUSH_TIMEOUT));
                if to.set_upstream {
                    // `push -u` sets no upstream for a commit id: the
                    // branch is pointed at what it was pushed to.
                    commands.push(Planned::new(
                        &tools.git,
                        &target.dir,
                        vec![
                            "branch".into(),
                            format!("--set-upstream-to={name}/{branch}"),
                            branch.clone(),
                        ],
                        COMMIT_TIMEOUT,
                    ));
                }
                let url = review
                    .remotes
                    .iter()
                    .find(|r| r.name == *name)
                    .map(|r| r.url.clone())
                    .unwrap_or_default();
                summary = format!("Push {branch} to {name}/{branch} ({url})");
                // What the remote has of it, as last fetched.
                let there = format!("{name}/{branch}");
                let known = git
                    .maybe(&[
                        "rev-parse",
                        "--verify",
                        "-q",
                        &format!("refs/remotes/{there}^{{commit}}"),
                    ])
                    .is_some();
                if known {
                    let (behind, ahead) = left_right(&git, &format!("refs/remotes/{there}...HEAD"));
                    if ahead == 0 && behind == 0 {
                        refusal = Some(format!("{branch} is up to date with {there}"));
                    } else {
                        details.push(format!(
                            "{} ahead of {there}",
                            plural(ahead as usize, "commit")
                        ));
                    }
                    if behind > 0 {
                        details.push(format!(
                            "{there} has {} this branch lacks: the push is refused unless you pull or rebase first (it is never forced)",
                            plural(behind as usize, "commit")
                        ));
                    }
                } else {
                    details.push(format!("creates {there}"));
                }
                match (&to.tracking, to.set_upstream) {
                    (_, false) if to.tracked => {}
                    (Some(old), true) => details.push(format!(
                        "sets {there} as the upstream of {branch} (it tracks {old} now, which is not pushed to)"
                    )),
                    (None, true) => {
                        details.push(format!("sets {there} as the upstream of {branch}"));
                    }
                    (Some(old), false) => {
                        details.push(format!("{branch} keeps tracking {old}"));
                    }
                    (None, false) => {}
                }
                chosen = format!("{name}\u{1}{}\u{1}{}", to.dest, to.set_upstream);
            }
        },
    }
    let mut warnings = Vec::new();
    if !review.clean() {
        warnings.push("uncommitted changes are not pushed (commit them first)".into());
    }
    Ok(Plan {
        action: "push",
        summary,
        details,
        warnings,
        commands,
        fingerprint: fingerprint(&["push", &head, &chosen]),
        refusal,
        needs_rebase: false,
        defaults: None,
        contents: Vec::new(),
    })
}

pub(super) fn refused(action: &'static str, why: &str) -> Plan {
    Plan {
        action,
        summary: why.to_owned(),
        details: Vec::new(),
        warnings: Vec::new(),
        commands: Vec::new(),
        fingerprint: fingerprint(&[action, "refused"]),
        refusal: Some(why.to_owned()),
        needs_rebase: false,
        defaults: None,
        contents: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_build_artifacts_are_warned_about() {
        for path in [
            "__pycache__/calc.cpython-313.pyc",
            "pkg/__pycache__/x.txt",
            "web/node_modules/a/index.js",
            "Main.class",
            "src/lib.o",
            ".DS_Store",
            "target/debug/app",
        ] {
            assert!(looks_generated(path), "{path}");
        }
        for path in [
            "calc.py",
            "src/target.rs",
            "build.rs",
            "docs/obj.md",
            ".o",
            "target/README.md",
        ] {
            assert!(!looks_generated(path), "{path}");
        }
        assert_eq!(generated_warning(["calc.py", "notes.md"].into_iter()), None);
        assert_eq!(
            generated_warning(["calc.py", "__pycache__/calc.cpython-313.pyc"].into_iter()).unwrap(),
            "1 new file __pycache__/calc.cpython-313.pyc looks generated and will be committed \
             too; list it in .gitignore to leave it out"
        );
        let two = generated_warning(["a.pyc", "b.pyc"].into_iter()).unwrap();
        assert!(
            two.starts_with("2 new files a.pyc and 1 more look generated"),
            "{two}"
        );
    }
}
