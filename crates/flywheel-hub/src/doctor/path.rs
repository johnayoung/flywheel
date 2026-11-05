//! `PATH`: which `fw`/`flywheel` a shell runs, whether it is this binary
//! or one installed beside it, and the version another one answers.

use super::agents::{bounded, first_line};
use super::{Check, Level};
use crate::agent::{self};
use crate::review::runner::{self};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Every executable called `name` on `path`, in `PATH` order (the first
/// is what a shell runs).
pub fn executables_named(name: &str, path: Option<&OsStr>) -> Vec<PathBuf> {
    let Some(path) = path else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in std::env::split_paths(path).filter(|d| !d.as_os_str().is_empty()) {
        let candidate = dir.join(name);
        if let Some(hit) = agent::resolve_program(&candidate.to_string_lossy(), None)
            && !found.contains(&hit)
        {
            found.push(hit);
        }
    }
    found
}

/// What a program found on `PATH` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// This very program (same file, or a byte-identical copy: `cargo
    /// install` writes `fw` and `flywheel` as two copies).
    This,
    /// A script: its `#!` interpreter line.
    Script(String),
    /// A flywheel with `fw hub`, but another build: its version, from
    /// `--version`, when it answers one (builds before `fw --version` do
    /// not).
    OtherFlywheel(Option<String>),
    /// Some other program.
    Other,
}

/// Classify `candidate` against this program `exe`. Scripts are read, not
/// run; a binary that differs is asked for `hub --help`.
pub fn classify(candidate: &Path, exe: Option<&Path>) -> Found {
    let real = candidate
        .canonicalize()
        .unwrap_or_else(|_| candidate.to_path_buf());
    if let Some(exe) = exe {
        if real == exe {
            return Found::This;
        }
        if same_contents(&real, exe) {
            return Found::This;
        }
    }
    let mut head = [0u8; 256];
    let read = std::fs::File::open(&real)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    if head[..read].starts_with(b"#!") {
        let line = String::from_utf8_lossy(&head[2..read]);
        let interpreter = line.lines().next().unwrap_or_default().trim().to_owned();
        return Found::Script(interpreter);
    }
    let help = bounded(&real, &["hub", "--help"]);
    match help {
        Some(out) if out.success() && out.stdout_text().contains("The flywheel hub") => {
            if exe.is_some_and(|exe| installed_together(&real, exe)) {
                Found::This
            } else {
                Found::OtherFlywheel(
                    bounded(&real, &["--version"])
                        .filter(runner::Output::success)
                        .and_then(|out| parse_version(&out.stdout_text())),
                )
            }
        }
        _ => Found::Other,
    }
}

/// The version in a `fw --version` answer (`fw 0.6.1`, `flywheel 0.6.1`).
pub fn parse_version(text: &str) -> Option<String> {
    let line = first_line(text);
    let mut words = line.split_whitespace();
    let prog = words.next()?;
    let version = words.next()?;
    let looks_like = ["fw", "flywheel"].contains(&prog)
        && words.next().is_none()
        && version.starts_with(|c: char| c.is_ascii_digit());
    looks_like.then(|| version.to_owned())
}

/// Binaries one build writes side by side: `fw` and `flywheel` are two
/// bin targets, so never byte-identical, but one `cargo install` (or
/// `cargo build`) puts both in one directory within moments, at about the
/// same size. A copy from another build differs in time (and usually
/// size).
const SIBLING_NAMES: [&str; 2] = ["fw", "flywheel"];
/// How far apart one build writes its two binaries, at most.
const SIBLING_WRITE_WINDOW: Duration = Duration::from_secs(600);

pub fn installed_together(candidate: &Path, exe: &Path) -> bool {
    let name = |p: &Path| p.file_name().and_then(OsStr::to_str).map(str::to_owned);
    let (Some(a), Some(b)) = (name(candidate), name(exe)) else {
        return false;
    };
    if a == b || !SIBLING_NAMES.contains(&a.as_str()) || !SIBLING_NAMES.contains(&b.as_str()) {
        return false;
    }
    if candidate.parent() != exe.parent() {
        return false;
    }
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(candidate), std::fs::metadata(exe)) else {
        return false;
    };
    let (Ok(ta), Ok(tb)) = (ma.modified(), mb.modified()) else {
        return false;
    };
    let apart = ta.duration_since(tb).or_else(|_| tb.duration_since(ta));
    let (big, small) = (ma.len().max(mb.len()), ma.len().min(mb.len()));
    apart.is_ok_and(|d| d <= SIBLING_WRITE_WINDOW) && big - small <= big / 100
}

fn same_contents(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    if ma.len() != mb.len() {
        return false;
    }
    match (digest(a), digest(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn digest(path: &Path) -> Option<[u8; 32]> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return None,
        }
    }
    Some(hasher.finalize().into())
}

/// The check of what `name` runs in a shell: `found` (in `PATH` order).
pub fn on_path_check(name: &'static str, found: &[PathBuf], exe: Option<&Path>) -> Check {
    on_path_check_with(name, found, exe, &|p| classify(p, exe))
}

pub(crate) fn on_path_check_with(
    name: &'static str,
    found: &[PathBuf],
    exe: Option<&Path>,
    classify: &dyn Fn(&Path) -> Found,
) -> Check {
    let id = if name == "fw" {
        "fw on PATH"
    } else {
        "flywheel on PATH"
    };
    let this = exe.map_or_else(|| "this program".to_owned(), |e| e.display().to_string());
    let exe_dir = exe
        .and_then(Path::parent)
        .map_or_else(|| "its directory".to_owned(), |d| d.display().to_string());
    let Some(first) = found.first() else {
        // `fw` is what docs and hooks' users type; `flywheel` is the
        // long name.
        let level = if name == "fw" {
            Level::Warn
        } else {
            Level::Info
        };
        return Check::new(id, level, format!("no `{name}` on PATH (this is {this})"))
            .fix(format!("add {exe_dir} to PATH"));
    };
    let kind = classify(first);
    let mut check = match &kind {
        Found::This => Check::new(id, Level::Ok, format!("{} is this binary", first.display())),
        Found::OtherFlywheel(version) => Check::new(
            id,
            Level::Warn,
            match version.as_deref() {
                Some(v) if v != env!("CARGO_PKG_VERSION") => format!(
                    "{} is flywheel {v}, not this one ({this}, flywheel {})",
                    first.display(),
                    env!("CARGO_PKG_VERSION")
                ),
                Some(v) => format!(
                    "{} is another flywheel {v} build than this one ({this})",
                    first.display()
                ),
                None => format!(
                    "{} is an older flywheel build (no `--version`) than this one ({this}, \
                     flywheel {})",
                    first.display(),
                    env!("CARGO_PKG_VERSION")
                ),
            },
        )
        .fix(format!(
            "reinstall one build (`cargo install --path crates/flywheel`), or put {exe_dir} \
             first on PATH"
        )),
        Found::Script(_) | Found::Other => {
            let what = match &kind {
                Found::Script(interpreter) => format!("a script run by {interpreter}"),
                _ => "another program".to_owned(),
            };
            // The flywheel it shadows, when one is further down PATH: its
            // directory is the one to move up.
            let shadowed = found[1..]
                .iter()
                .find(|p| matches!(classify(p), Found::This | Found::OtherFlywheel(_)))
                .and_then(|p| p.parent())
                .map_or_else(|| exe_dir.clone(), |d| d.display().to_string());
            Check::new(
                id,
                Level::Fail,
                format!(
                    "`{name}` runs {} ({what}), not this flywheel: `{name} hub ...` in a shell \
                     goes to it",
                    first.display()
                ),
            )
            .detail(format!("this flywheel: {this}"))
            .fix(format!(
                "remove or rename {}, or put {shadowed} before {} on PATH; then \
                 `hash -r`",
                first.display(),
                first
                    .parent()
                    .map_or_else(String::new, |d| d.display().to_string())
            ))
        }
    };
    for later in &found[1..] {
        check = check.detail(format!("also on PATH (shadowed): {}", later.display()));
    }
    check
}
