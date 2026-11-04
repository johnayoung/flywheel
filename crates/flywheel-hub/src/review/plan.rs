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
    /// Commit: how many files it takes, and how many of them `details`
    /// names (fewer past the review's [`MAX_FILES`] cut).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<FileCount>,
}

/// A commit's files: all of them, and those its plan lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FileCount {
    pub total: usize,
    pub listed: usize,
}

/// A changed file, or a directory of them, a commit-all may not want.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Suspect {
    /// A file, or a directory of generated output (ending in `/`).
    pub path: String,
    /// `generated` (build or tool output), `build` (a directory that may
    /// be build output: `dist/`, `build/`, ...), `secret` (keys, `.env`),
    /// or `large` (over [`LARGE_FILE_BYTES`]). `generated` and `secret`
    /// are left out by default.
    pub kind: &'static str,
    /// Left out of the commit.
    pub excluded: bool,
    /// The changed files it stands for (a file: 1).
    pub files: usize,
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
/// A work tree's state ([`tree_state`]), each changed path with what it
/// holds, and the `git status` read.
type Snapshot = (String, Vec<(String, String)>, Vec<u8>);

fn tree_snapshot(git: &Git<'_>) -> Result<Snapshot, String> {
    let status = porcelain(git, &["--branch"])?;
    let mut state = String::from_utf8_lossy(&status).into_owned();
    let contents = path_contents(git, &status);
    for (path, hash) in &contents {
        state.push('\u{1}');
        state.push_str(path);
        state.push('\u{2}');
        state.push_str(hash);
    }
    Ok((state, contents, status))
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
    let status = git.raw(&args, STATUS_LIMIT)?;
    if !status.success() {
        return Err(status.error_text());
    }
    if status.truncated {
        return Err(too_many_changes(git));
    }
    Ok(status.stdout)
}

/// How much of `git status` a plan reads (every changed path, one entry
/// each): far past the review's cut, bounded all the same.
const STATUS_LIMIT: usize = 64 * 1024 * 1024;

/// Why a work tree is past planning: its `git status` is over
/// [`STATUS_LIMIT`]. Names the untracked directories (each one entry in a
/// collapsed read), which are almost always what holds it: a
/// `node_modules/` or a build directory with no `.gitignore`.
pub(super) fn too_many_changes(git: &Git<'_>) -> String {
    let base = format!(
        "too many changes to plan against (git status is over {} MiB)",
        STATUS_LIMIT / (1024 * 1024)
    );
    let collapsed = git.raw(
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=normal",
            "--no-renames",
        ],
        READ_LIMIT,
    );
    let dirs: Vec<String> = match collapsed {
        Ok(out) if out.success() => status_entries(&out.stdout)
            .into_iter()
            .filter(|(entry, path)| entry.first() == Some(&b'?') && path.ends_with('/'))
            .map(|(_, path)| path)
            .collect(),
        _ => Vec::new(),
    };
    if dirs.is_empty() {
        return format!("{base}; commit in the terminal");
    }
    let shown: Vec<&str> = dirs.iter().take(3).map(String::as_str).collect();
    let more = match dirs.len() {
        n if n > 3 => format!(" and {} more", n - 3),
        _ => String::new(),
    };
    format!(
        "{base}: untracked {} {}{more}; list what is not source (node_modules/, build output) in .gitignore",
        if dirs.len() == 1 {
            "directory"
        } else {
            "directories"
        },
        shown.join(", ")
    )
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
    let left_out = |path: &str| excluded.iter().any(|e| covers(e, path));
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

/// The paths with a change in the index (`git status --porcelain=v2 -z`).
fn staged_paths(status: &[u8]) -> Vec<String> {
    status_entries(status)
        .into_iter()
        // "1 XY ..." / "2 XY ...": X is the index against HEAD.
        .filter(|(entry, _)| {
            matches!(entry.first(), Some(b'1' | b'2')) && entry.get(2) != Some(&b'.')
        })
        .map(|(_, path)| path)
        .collect()
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

/// Whether `exclude` (a path, or a directory ending in `/`) leaves `path`
/// out.
pub(super) fn covers(exclude: &str, path: &str) -> bool {
    exclude == path || (exclude.ends_with('/') && path.starts_with(exclude))
}

/// `exclude` as a git pathspec of exactly that path (a directory: every
/// path under it), from the top, with `magic` added.
fn literal_spec(magic: &str, exclude: &str) -> String {
    format!(
        ":(top,literal{magic}){}",
        exclude.strip_suffix('/').unwrap_or(exclude)
    )
}

/// Directories that only ever hold dependency, cache or build output.
const GENERATED_DIRS: &[&str] = &[
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

/// Directories that are often build output and sometimes source (a
/// `build/` of scripts, a checked-in `dist/`): named, never left out by
/// default.
const MAYBE_GENERATED_DIRS: &[&str] = &["dist", "build", "coverage", "venv"];

/// What a Cargo target directory holds at its top.
const CARGO_OUTPUT: &[&str] = &[
    "debug",
    "release",
    "doc",
    "tmp",
    "package",
    "CACHEDIR.TAG",
    ".rustc_info.json",
];

/// How sure a directory is generated output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Generated {
    /// Only ever output: left out by default.
    Surely,
    /// Often output, sometimes source: named, kept by default.
    Maybe,
}

/// Classifies changed paths' directories as generated output, reading the
/// work tree `dir` for the markers that decide it (each looked up once).
struct Classifier<'a> {
    dir: &'a Path,
    files: std::collections::HashMap<String, bool>,
}

impl<'a> Classifier<'a> {
    fn new(dir: &'a Path) -> Self {
        Self {
            dir,
            files: std::collections::HashMap::new(),
        }
    }

    /// Whether `rel` (under the work tree) is a file.
    fn file(&mut self, rel: String) -> bool {
        let dir = self.dir;
        *self
            .files
            .entry(rel)
            .or_insert_with_key(|rel| dir.join(rel).is_file())
    }

    /// Directory `parts[..=i]`, holding `parts[i + 1]`: generated output,
    /// and how surely. Surely: one of [`GENERATED_DIRS`], a `*.egg-info`,
    /// a directory tagged a cache (`CACHEDIR.TAG`: Cargo tags every target
    /// directory, `CARGO_TARGET_DIR` ones included), a virtualenv
    /// (`pyvenv.cfg`), or a `target/` beside a `Cargo.toml`. Maybe: a
    /// `target/` shaped like Cargo's (a profile, `doc`, a target triple)
    /// with no manifest beside it, or one of [`MAYBE_GENERATED_DIRS`].
    fn class(&mut self, parts: &[&str], i: usize) -> Option<Generated> {
        let name = parts[i];
        if GENERATED_DIRS.contains(&name)
            || (name.len() > ".egg-info".len() && name.ends_with(".egg-info"))
        {
            return Some(Generated::Surely);
        }
        let at = parts[..=i].join("/");
        if self.file(format!("{at}/CACHEDIR.TAG")) || self.file(format!("{at}/pyvenv.cfg")) {
            return Some(Generated::Surely);
        }
        if name == "target" {
            let next = parts[i + 1];
            let triple = next.matches('-').count() >= 2 && !next.contains('.');
            let manifest = match i {
                0 => "Cargo.toml".to_owned(),
                _ => format!("{}/Cargo.toml", parts[..i].join("/")),
            };
            if self.file(manifest) || self.file(format!("{at}/.rustc_info.json")) {
                return Some(Generated::Surely);
            }
            if CARGO_OUTPUT.contains(&next) || triple {
                return Some(Generated::Maybe);
            }
        }
        MAYBE_GENERATED_DIRS
            .contains(&name)
            .then_some(Generated::Maybe)
    }

    /// The directory of `path` that holds only generated output, with its
    /// trailing `/`: the outermost sure one, else the outermost maybe.
    fn generated_dir(&mut self, path: &str) -> Option<(String, Generated)> {
        let parts: Vec<&str> = path.split('/').collect();
        let mut maybe = None;
        for i in 0..parts.len().saturating_sub(1) {
            match self.class(&parts, i) {
                Some(Generated::Surely) => {
                    return Some((format!("{}/", parts[..=i].join("/")), Generated::Surely));
                }
                Some(Generated::Maybe) if maybe.is_none() => maybe = Some(i),
                _ => {}
            }
        }
        maybe.map(|i| (format!("{}/", parts[..=i].join("/")), Generated::Maybe))
    }
}

/// Whether a file looks like a build or tool artifact by itself
/// (bytecode, objects) rather than source.
fn looks_generated(path: &str) -> bool {
    const EXTENSIONS: &[&str] = &["pyc", "pyo", "o", "obj", "class"];
    let name = path.rsplit('/').next().unwrap_or(path);
    name == ".DS_Store"
        || name
            .rsplit_once('.')
            .is_some_and(|(stem, ext)| !stem.is_empty() && EXTENSIONS.contains(&ext))
}

/// Whether a file looks like a secret by its name: a dotenv file (not a
/// checked-in example), a private key or certificate store, credentials.
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
    // `.env`, `.env.local`, `prod.env`; not `.env.example`, `sample.env`.
    let dotenv = name == ".env"
        || name
            .strip_prefix(".env.")
            .is_some_and(|rest| !EXAMPLES.contains(&rest.rsplit('.').next().unwrap_or(rest)))
        || name.strip_suffix(".env").is_some_and(|stem| {
            !stem.is_empty() && !EXAMPLES.contains(&stem.rsplit('.').next().unwrap_or(stem))
        });
    dotenv
        || NAMES.contains(&name)
        || name
            .rsplit_once('.')
            .is_some_and(|(stem, ext)| !stem.is_empty() && EXTENSIONS.contains(&ext))
}

/// A `.key` that is a zip (a Keynote deck), not a key.
fn is_zip(file: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 4];
    std::fs::File::open(file)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| head == *b"PK\x03\x04")
}

/// What a commit-all would take that may not be wanted, from every
/// changed path (`paths`, the full status, not the cut review list): a
/// directory of generated output (one entry, ending in `/`, for all its
/// files; `generated` when sure, `build` when only maybe), and each other
/// file still in the work tree that looks secret or generated (left out
/// unless `exclude` says otherwise) or is large (kept unless excluded).
/// A secret or a sure directory inside a maybe one is its own entry.
/// `exclude` (`None`: those defaults) names exactly what is left out.
fn suspects(dir: &Path, paths: &[&str], exclude: Option<&[String]>) -> Vec<Suspect> {
    let mut found: Vec<Suspect> = Vec::new();
    let mut dirs: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut classifier = Classifier::new(dir);
    for path in paths {
        let under = classifier.generated_dir(path);
        let file = dir.join(path);
        let meta = file.symlink_metadata().ok();
        let secret =
            meta.is_some() && looks_secret(path) && !(path.ends_with(".key") && is_zip(&file));
        let (entry, kind) = match under {
            Some((under, Generated::Surely)) => (under, "generated"),
            _ if secret => ((*path).to_owned(), "secret"),
            _ if meta.is_some() && looks_generated(path) => ((*path).to_owned(), "generated"),
            Some((under, Generated::Maybe)) => (under, "build"),
            None => match &meta {
                Some(meta) if meta.is_file() && meta.len() >= LARGE_FILE_BYTES => {
                    ((*path).to_owned(), "large")
                }
                _ => continue,
            },
        };
        match dirs.get(&entry) {
            Some(&at) => found[at].files += 1,
            None => {
                dirs.insert(entry.clone(), found.len());
                found.push(Suspect {
                    path: entry,
                    kind,
                    excluded: false,
                    files: 1,
                });
            }
        }
    }
    for s in &mut found {
        s.excluded = match exclude {
            Some(list) => list.contains(&s.path),
            None => matches!(s.kind, "generated" | "secret"),
        };
    }
    found
}

/// `git add -A` of the whole work tree, but for `excluded` (each path
/// exactly, a directory all of it, from the top).
fn add_args(excluded: &[String]) -> Vec<String> {
    let mut args = vec!["add".to_owned(), "-A".to_owned()];
    if !excluded.is_empty() {
        args.push("--".into());
        args.push(":/".into());
        args.extend(excluded.iter().map(|p| literal_spec(",exclude", p)));
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
    let (state, contents, status) = tree_snapshot(&git)?;
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
    // Every changed path, from the full status (the review lists are
    // cut at MAX_FILES; `git add -A` is not): one entry per path, staged
    // or not.
    let changed: Vec<&str> = contents.iter().map(|(path, _)| path.as_str()).collect();
    if let Some(unknown) = exclude
        .unwrap_or_default()
        .iter()
        .find(|e| !changed.iter().any(|p| covers(e, p)))
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
    let left_out = |path: &str| excluded.iter().any(|e| covers(e, path));
    let mut details = Vec::new();
    // The paths the lines name: a rename's line names both its paths,
    // each a change of its own in `changed` (renames off there).
    let mut named = std::collections::HashSet::new();
    for (prefix, set) in [("staged ", &review.staged), ("", &review.unstaged)] {
        for f in set.files.iter().filter(|f| !left_out(&f.path)) {
            details.push(change_line(prefix, f));
            named.insert(f.path.as_str());
            named.extend(f.old_path.as_deref());
        }
    }
    for u in review.untracked.iter().filter(|u| !left_out(&u.path)) {
        details.push(format!("new {}", u.path));
        named.insert(u.path.as_str());
    }
    let committed: Vec<&str> = changed.iter().copied().filter(|p| !left_out(p)).collect();
    let files = committed.len();
    let listed = committed.iter().filter(|p| named.contains(*p)).count();
    let out = changed.len() - files;
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
    if listed < files
        && (review.untracked_truncated || review.unstaged.truncated || review.staged.truncated)
    {
        warnings.push(format!(
            "{} to commit, {listed} of them listed (cut at {MAX_FILES} per set; what is left out was checked against all)",
            plural(files, "file")
        ));
    }
    if out > 0 {
        warnings.push(format!(
            "{} left out (still in the work tree; list {} in .gitignore to keep {} out)",
            plural(out, "file"),
            if excluded.len() == 1 { "it" } else { "them" },
            if excluded.len() == 1 { "it" } else { "them" },
        ));
    }
    let kept: Vec<&Suspect> = suspects.iter().filter(|s| !s.excluded).collect();
    if let Some(first) = kept.first() {
        let looks = match first.kind {
            "secret" => "like a secret",
            "generated" => "generated",
            "build" => "like build output",
            _ => "large (5 MiB or more)",
        };
        let more = match kept.len() {
            1 => String::new(),
            n => format!(" ({} more like it)", n - 1),
        };
        let what = if first.files > 1 {
            format!("{} ({})", first.path, plural(first.files, "file"))
        } else {
            first.path.clone()
        };
        warnings.push(format!("{what} looks {looks} and is committed{more}"));
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
    let staged = staged_paths(&status);
    let staged_out: Vec<String> = excluded
        .iter()
        .filter(|e| staged.iter().any(|p| covers(e, p)))
        .map(|e| literal_spec("", e))
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
        summary: match (out, files) {
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
        files: Some(FileCount {
            total: files,
            listed,
        }),
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
        files: None,
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
        files: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `path`'s generated directory as `(dir, sure)`, against work tree
    /// `dir`.
    fn generated(dir: &Path, path: &str) -> Option<(String, bool)> {
        Classifier::new(dir)
            .generated_dir(path)
            .map(|(under, how)| (under, how == Generated::Surely))
    }

    #[test]
    fn new_build_artifacts_are_warned_about() {
        let nowhere = Path::new("/nonexistent-hub-test-dir");
        for (path, under, sure) in [
            ("__pycache__/calc.cpython-313.pyc", "__pycache__/", true),
            ("pkg/__pycache__/x.txt", "pkg/__pycache__/", true),
            (
                "web/node_modules/a/node_modules/b.js",
                "web/node_modules/",
                true,
            ),
            ("app/.next/server/page.js", "app/.next/", true),
            (".venv/lib/site.py", ".venv/", true),
            ("src/calc.egg-info/PKG-INFO", "src/calc.egg-info/", true),
            // Shaped like Cargo's but no manifest beside it: only maybe.
            ("target/debug/app", "target/", false),
            (
                "target/x86_64-unknown-linux-musl/release/app",
                "target/",
                false,
            ),
            ("target/doc/index.html", "target/", false),
            ("web/dist/app.js", "web/dist/", false),
            ("build/lib/calc.py", "build/", false),
            ("coverage/lcov.info", "coverage/", false),
            ("venv/bin/python", "venv/", false),
            // A sure directory inside a maybe one wins.
            ("dist/node_modules/x.js", "dist/node_modules/", true),
        ] {
            assert_eq!(
                generated(nowhere, path),
                Some((under.to_owned(), sure)),
                "{path}"
            );
        }
        for path in ["Main.class", "src/lib.o", ".DS_Store"] {
            assert!(looks_generated(path), "{path}");
        }
        for path in [
            "calc.py",
            "src/target.rs",
            "build.rs",
            "docs/obj.md",
            ".o",
            "target/README.md",
            "target",
            "dist",
            ".egg-info/x",
            "src/distance/x.rs",
        ] {
            assert!(!looks_generated(path), "{path}");
            assert_eq!(generated(nowhere, path), None, "{path}");
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
            "zz.env",
            "deploy/prod.env",
            "secrets.env",
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
            "sample.env",
            "config/example.env",
            "env",
        ] {
            assert!(!looks_secret(path), "{path}");
        }
    }

    #[test]
    fn cargo_and_cache_tagged_dirs_are_build_output_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mkfile = |rel: &str, body: &str| {
            let at = root.join(rel);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, body).unwrap();
        };
        mkfile("tool/Cargo.toml", "[package]\n");
        mkfile("tool/target/custom/out.bin", "x");
        mkfile("crates/a/b/Cargo.toml", "[package]\n");
        mkfile("crates/a/b/target/debug/liba.rlib", "x");
        // CARGO_TARGET_DIR=out/cargo: Cargo tags it.
        mkfile(
            "out/cargo/CACHEDIR.TAG",
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        );
        mkfile("out/cargo/release/app", "x");
        mkfile("py/env/pyvenv.cfg", "home = /usr/bin\n");
        mkfile("py/env/bin/python", "x");
        mkfile("venv/pyvenv.cfg", "home = /usr/bin\n");
        for (path, under) in [
            ("tool/target/custom/out.bin", "tool/target/"),
            ("crates/a/b/target/debug/liba.rlib", "crates/a/b/target/"),
            ("out/cargo/release/app", "out/cargo/"),
            ("py/env/bin/python", "py/env/"),
            ("venv/bin/python", "venv/"),
        ] {
            assert_eq!(
                generated(root, path),
                Some((under.to_owned(), true)),
                "{path}"
            );
        }
        // No manifest, no Cargo output inside: a directory like any other.
        assert_eq!(generated(root, "web/target/custom/out.bin"), None);
    }

    #[test]
    fn only_unambiguous_build_output_is_left_out_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for rel in [
            "dist/app.js",
            "dist/app.css",
            "dist/.env",
            "node_modules/x/i.js",
            "calc.egg-info/PKG-INFO",
        ] {
            let at = root.join(rel);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, "x").unwrap();
        }
        let paths = [
            "calc.egg-info/PKG-INFO",
            "dist/.env",
            "dist/app.css",
            "dist/app.js",
            "node_modules/x/i.js",
        ];
        let found = suspects(root, &paths, None);
        let got: Vec<(&str, &str, bool, usize)> = found
            .iter()
            .map(|s| (s.path.as_str(), s.kind, s.excluded, s.files))
            .collect();
        assert_eq!(
            got,
            [
                ("calc.egg-info/", "generated", true, 1),
                ("dist/.env", "secret", true, 1),
                ("dist/", "build", false, 2),
                ("node_modules/", "generated", true, 1),
            ]
        );
    }

    #[test]
    fn a_keynote_deck_is_not_a_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("deck.key"), b"PK\x03\x04rest of a zip").unwrap();
        std::fs::write(dir.path().join("tls.key"), "-----BEGIN PRIVATE KEY-----\n").unwrap();
        let found = suspects(dir.path(), &["deck.key", "tls.key"], None);
        let kinds: Vec<(&str, &str)> = found.iter().map(|s| (s.path.as_str(), s.kind)).collect();
        assert_eq!(kinds, [("tls.key", "secret")]);
    }

    #[test]
    fn a_directory_exclusion_covers_what_is_under_it() {
        assert!(covers("node_modules/", "node_modules/a/b.js"));
        assert!(covers("a.txt", "a.txt"));
        assert!(!covers("node_modules/", "node_modules_old/a.js"));
        assert!(!covers("a", "a/b"));
        assert_eq!(
            literal_spec(",exclude", "node_modules/"),
            ":(top,literal,exclude)node_modules"
        );
    }
}
