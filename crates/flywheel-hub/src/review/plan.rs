//! Dry runs: a plan's exact commands and the fingerprint of the state it
//! was planned against; planning a commit and a push.

use super::model::{FileChange, Remote, left_right, read};
use super::runner;
use super::{COMMIT_TIMEOUT, Git, MAX_FILES, PUSH_TIMEOUT, READ_LIMIT, Target, Tools};
use crate::util::plural;
use serde::Serialize;
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
    /// Commit: the changed paths and what each held when planned (not
    /// the ones left out).
    #[serde(skip)]
    pub contents: Vec<(String, String)>,
    /// Commit: changed files that look like build output, secrets, or are
    /// large, each with whether it is left out (`exclude`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suspects: Vec<Suspect>,
    /// Commit: the changed paths left out (staged ones unstaged again).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<String>,
}

/// A changed file a commit-all may not want.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Suspect {
    pub path: String,
    /// `generated` (build or tool output), `secret` (keys, `.env`), or
    /// `large` (over [`LARGE_FILE_BYTES`]).
    pub kind: &'static str,
    /// Left out of the commit.
    pub excluded: bool,
}

/// A changed file at least this big is named before it is committed.
pub const LARGE_FILE_BYTES: u64 = 5 * 1024 * 1024;

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
    excluded: &[String],
) -> Result<bool, String> {
    let status = porcelain(git, &[])?;
    let left_out = |path: &str| excluded.iter().any(|e| e == path);
    let fully_staged = status_entries(&status).into_iter().all(|(entry, path)| {
        left_out(&path)
            || match entry.first() {
                // "1 XY ...": Y is the work tree against the index.
                Some(b'1') => entry.get(3) == Some(&b'.'),
                _ => false,
            }
    });
    if !fully_staged {
        return Ok(false);
    }
    // A left-out path still changed in the work tree: neither staged
    // (checked above) nor part of the comparison.
    let mut now: Vec<(String, String)> = path_contents(git, &status)
        .into_iter()
        .filter(|(path, _)| !left_out(path))
        .collect();
    let mut planned = planned.to_vec();
    now.sort();
    planned.sort();
    Ok(now == planned)
}

/// The work-tree paths `git status --porcelain=v2 -z` names (changed,
/// unmerged, untracked).
pub(super) fn status_paths(status: &[u8]) -> Vec<String> {
    status_entries(status)
        .into_iter()
        .map(|(_, path)| path)
        .collect()
}

/// Each entry of `git status --porcelain=v2 -z` (changed, unmerged,
/// untracked) with its work-tree path.
fn status_entries(status: &[u8]) -> Vec<(&[u8], String)> {
    let mut paths = Vec::new();
    let mut tokens = status.split(|b| *b == 0).filter(|t| !t.is_empty());
    while let Some(raw) = tokens.next() {
        let entry = String::from_utf8_lossy(raw);
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
            paths.push((raw, path.to_owned()));
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

/// Whether a file looks like a secret: a dotenv file (not a checked-in
/// example), a private key or certificate store, credentials.
fn looks_secret(path: &str) -> bool {
    const EXTENSIONS: &[&str] = &["pem", "key", "p12", "pfx", "jks", "keystore", "ppk"];
    const NAMES: &[&str] = &[
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        ".netrc",
        ".pgpass",
        "credentials.json",
    ];
    const EXAMPLES: &[&str] = &["example", "sample", "template", "dist"];
    let name = path.rsplit('/').next().unwrap_or(path);
    let dotenv = name == ".env"
        || name
            .strip_prefix(".env.")
            .is_some_and(|rest| !EXAMPLES.contains(&rest.rsplit('.').next().unwrap_or(rest)));
    dotenv
        || NAMES.contains(&name)
        || name
            .rsplit_once('.')
            .is_some_and(|(stem, ext)| !stem.is_empty() && EXTENSIONS.contains(&ext))
}

/// What a commit-all would take that may not be wanted: each changed file
/// still in the work tree that looks generated or secret (left out unless
/// `exclude` says otherwise) or is large (kept unless excluded). `exclude`
/// (`None`: those defaults) names exactly what is left out.
fn suspects(dir: &Path, paths: &[&str], exclude: Option<&[String]>) -> Vec<Suspect> {
    paths
        .iter()
        .filter_map(|path| {
            let meta = dir.join(path).symlink_metadata().ok()?;
            let kind = if looks_secret(path) {
                "secret"
            } else if looks_generated(path) {
                "generated"
            } else if meta.is_file() && meta.len() >= LARGE_FILE_BYTES {
                "large"
            } else {
                return None;
            };
            let excluded = match exclude {
                Some(list) => list.iter().any(|e| e == path),
                None => kind != "large",
            };
            Some(Suspect {
                path: (*path).to_owned(),
                kind,
                excluded,
            })
        })
        .collect()
}

/// `git add -A` of the whole work tree, but for `excluded` (each path
/// exactly, from the top: `:(top,literal,exclude)`).
fn add_args(excluded: &[String]) -> Vec<String> {
    let mut args = vec!["add".to_owned(), "-A".to_owned()];
    if !excluded.is_empty() {
        args.push("--".into());
        args.push(":/".into());
        args.extend(
            excluded
                .iter()
                .map(|p| format!(":(top,literal,exclude){p}")),
        );
    }
    args
}

/// Plan committing every change (staged, unstaged, untracked) with
/// `message`, but for what `exclude` leaves out (`None`: the files that
/// look generated or secret; see [`Suspect`]).
pub fn plan_commit(
    tools: &Tools,
    target: &Target,
    message: &str,
    exclude: Option<&[String]>,
) -> Result<Plan, String> {
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
    // A file both staged and changed again since is one file.
    let mut changed: Vec<&str> = Vec::new();
    for path in review
        .staged
        .files
        .iter()
        .chain(&review.unstaged.files)
        .map(|f| f.path.as_str())
        .chain(review.untracked.iter().map(|u| u.path.as_str()))
    {
        if !changed.contains(&path) {
            changed.push(path);
        }
    }
    if let Some(unknown) = exclude
        .unwrap_or_default()
        .iter()
        .find(|e| !changed.contains(&e.as_str()))
    {
        refusal.get_or_insert_with(|| format!("{unknown} has no change to leave out"));
    }
    let suspects = suspects(&target.dir, &changed, exclude);
    let excluded: Vec<String> = match exclude {
        Some(list) => list.to_vec(),
        None => suspects
            .iter()
            .filter(|s| s.excluded)
            .map(|s| s.path.clone())
            .collect(),
    };
    let left_out = |path: &str| excluded.iter().any(|e| e == path);
    let mut details = Vec::new();
    for f in review.staged.files.iter().filter(|f| !left_out(&f.path)) {
        details.push(change_line("staged ", f));
    }
    for f in review.unstaged.files.iter().filter(|f| !left_out(&f.path)) {
        details.push(change_line("", f));
    }
    for u in review.untracked.iter().filter(|u| !left_out(&u.path)) {
        details.push(format!("new {}", u.path));
    }
    let files = changed.iter().filter(|p| !left_out(p)).count();
    if files == 0 && refusal.is_none() {
        refusal = Some("every change is left out: nothing to commit".into());
    }
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
    if !excluded.is_empty() {
        warnings.push(format!(
            "{} left out (still in the work tree; list {} in .gitignore to keep {} out)",
            plural(excluded.len(), "file"),
            if excluded.len() == 1 { "it" } else { "them" },
            if excluded.len() == 1 { "it" } else { "them" },
        ));
    }
    let kept: Vec<&Suspect> = suspects.iter().filter(|s| !s.excluded).collect();
    if let Some(first) = kept.first() {
        let looks = match first.kind {
            "secret" => "like a secret",
            "generated" => "generated",
            _ => "large (5 MiB or more)",
        };
        let more = match kept.len() {
            1 => String::new(),
            n => format!(" ({} more like it)", n - 1),
        };
        warnings.push(format!(
            "{} looks {looks} and is committed{more}",
            first.path
        ));
    }
    let dir = &target.dir;
    let mut add = vec![Planned::new(
        &tools.git,
        dir,
        add_args(&excluded),
        COMMIT_TIMEOUT,
    )];
    // Left out but staged already: unstaged again (no HEAD yet: dropped
    // from the index), the work tree untouched.
    let staged_out: Vec<String> = review
        .staged
        .files
        .iter()
        .filter(|f| left_out(&f.path))
        .map(|f| format!(":(top,literal){}", f.path))
        .collect();
    if !staged_out.is_empty() {
        let mut args: Vec<String> = if review.head.is_some() {
            vec!["reset".into(), "-q".into(), "--".into()]
        } else {
            vec![
                "rm".into(),
                "--cached".into(),
                "-q".into(),
                "-r".into(),
                "--".into(),
            ]
        };
        args.extend(staged_out);
        add.push(Planned::new(&tools.git, dir, args, COMMIT_TIMEOUT));
    }
    let commit = Planned::new(
        &tools.git,
        dir,
        vec!["commit".into(), "-F".into(), "-".into()],
        COMMIT_TIMEOUT,
    )
    .with_stdin(message.as_bytes().to_vec(), "the message");
    let contents: Vec<(String, String)> = contents
        .into_iter()
        .filter(|(path, _)| !left_out(path))
        .collect();
    add.push(commit);
    let excluded_key = excluded.join("\u{0}");
    Ok(Plan {
        action: "commit",
        summary: match (excluded.len(), files) {
            (0, _) => format!(
                "Stage all {} and commit them on {branch}",
                plural(files, "file")
            ),
            (out, 1) => format!("Stage 1 file (leaving {out} out) and commit it on {branch}"),
            (out, _) => format!(
                "Stage {} (leaving {out} out) and commit them on {branch}",
                plural(files, "file")
            ),
        },
        details,
        warnings,
        commands: add,
        fingerprint: fingerprint(&["commit", &state, message, &excluded_key]),
        refusal,
        needs_rebase: false,
        defaults: None,
        contents,
        suspects,
        excluded,
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
        suspects: Vec::new(),
        excluded: Vec::new(),
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
        suspects: Vec::new(),
        excluded: Vec::new(),
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
        for path in [
            ".env",
            "app/.env.local",
            ".env.production",
            "certs/server.pem",
            "tls.key",
            "store.p12",
            "home/id_ed25519",
            ".netrc",
        ] {
            assert!(looks_secret(path), "{path}");
        }
        for path in [
            ".env.example",
            "config/.env.sample",
            ".env.local.template",
            "id_ed25519.pub",
            "keys.md",
            "src/env.rs",
            ".key",
        ] {
            assert!(!looks_secret(path), "{path}");
        }
    }
}
