//! The source checkout: resolution, the flywheel / clean / landed checks,
//! the `cargo` build, and the artifact smoke check.
//!
//! Every check here runs before `cargo` does; a failure is a refusal (exit
//! 3) with nothing changed. A release-binary source is a future second
//! `source.kind`: [`SourceKind`] is the seam it plugs into.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The package and bins a flywheel checkout's `crates/flywheel/Cargo.toml`
/// must declare.
pub const PACKAGE: &str = "flywheel";
pub const BINS: [&str; 2] = ["fw", "flywheel"];
/// The manifest, relative to the checkout root.
pub const MANIFEST: &str = "crates/flywheel/Cargo.toml";
/// The default branch when `refs/remotes/origin/HEAD` names none.
pub const FALLBACK_DEFAULT_BRANCH: &str = "main";
/// The build `cargo` runs (argv after `cargo`).
pub const BUILD_ARGS: [&str; 5] = ["build", "--release", "--locked", "-p", "flywheel"];
/// How long one artifact's `--version` may take.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the binaries come from. Only checkouts exist today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Checkout,
}

/// A resolved, validated flywheel checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub kind: SourceKind,
    /// The work tree root (canonical).
    pub root: PathBuf,
    /// HEAD's commit id.
    pub head: String,
}

/// Run `git -C dir args`, with any inherited repository-selecting
/// variables removed so `dir` alone picks the repo.
fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))
}

fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let output = git(dir, args).ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The work tree root containing `dir`, or `None` outside a git work tree.
pub fn work_tree_root(dir: &Path) -> Option<PathBuf> {
    let inside = git_stdout(dir, &["rev-parse", "--is-inside-work-tree"])?;
    if inside != "true" {
        return None;
    }
    let top = git_stdout(dir, &["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(top);
    Some(top.canonicalize().unwrap_or(top))
}

/// Whether `manifest` (a `Cargo.toml` body) declares package [`PACKAGE`]
/// with both [`BINS`].
pub fn manifest_is_flywheel(manifest: &str) -> Result<(), String> {
    let value: toml::Value =
        toml::from_str(manifest).map_err(|e| format!("{MANIFEST} does not parse: {e}"))?;
    let name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(toml::Value::as_str);
    if name != Some(PACKAGE) {
        return Err(format!("{MANIFEST} does not declare package {PACKAGE}"));
    }
    let bins: Vec<&str> = value
        .get("bin")
        .and_then(toml::Value::as_array)
        .map(|bins| {
            bins.iter()
                .filter_map(|b| b.get("name").and_then(toml::Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let missing: Vec<&str> = BINS.iter().copied().filter(|b| !bins.contains(b)).collect();
    if !missing.is_empty() {
        return Err(format!(
            "{MANIFEST} does not declare bin(s) {}",
            missing.join(", ")
        ));
    }
    Ok(())
}

/// Validate that `dir` is inside a flywheel checkout; returns its root.
pub fn flywheel_checkout(dir: &Path) -> Result<PathBuf, String> {
    let root = work_tree_root(dir)
        .ok_or_else(|| format!("{} is not inside a git work tree", dir.display()))?;
    let manifest = root.join(MANIFEST);
    let body = std::fs::read_to_string(&manifest).map_err(|_| {
        format!(
            "{} is not a flywheel checkout (no {MANIFEST})",
            root.display()
        )
    })?;
    manifest_is_flywheel(&body)
        .map_err(|e| format!("{} is not a flywheel checkout: {e}", root.display()))?;
    Ok(root)
}

/// Resolve the source: `--from`, else the cwd repo when it is a flywheel
/// checkout, else the checkout the last successful update recorded.
pub fn resolve(
    from: Option<&Path>,
    cwd: &Path,
    recorded: Option<&Path>,
) -> Result<PathBuf, String> {
    if let Some(from) = from {
        return flywheel_checkout(from);
    }
    let cwd_error = match flywheel_checkout(cwd) {
        Ok(root) => return Ok(root),
        Err(e) => e,
    };
    match recorded {
        Some(recorded) => flywheel_checkout(recorded)
            .map_err(|e| format!("the recorded checkout is unusable: {e}")),
        None => Err(format!(
            "{cwd_error}; pass --from PATH (no earlier self-update recorded a checkout)"
        )),
    }
}

/// The paths `git status` reports as modified or untracked-not-ignored.
pub fn dirty_paths(root: &Path) -> Result<Vec<String>, String> {
    let output = git(root, &["status", "--porcelain=v1", "--untracked-files=all"])?;
    if !output.status.success() {
        return Err(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.get(3..).unwrap_or(l).to_owned())
        .collect())
}

/// Refuse a tree with any modified tracked or untracked non-ignored file.
pub fn require_clean(root: &Path) -> Result<(), String> {
    let dirty = dirty_paths(root)?;
    if dirty.is_empty() {
        return Ok(());
    }
    let shown: Vec<&str> = dirty.iter().take(5).map(String::as_str).collect();
    let more = if dirty.len() > shown.len() {
        format!(" (and {} more)", dirty.len() - shown.len())
    } else {
        String::new()
    };
    Err(format!(
        "the checkout is not clean: {}{more}",
        shown.join(", ")
    ))
}

/// HEAD's commit id.
pub fn head_commit(root: &Path) -> Result<String, String> {
    git_stdout(root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .ok_or_else(|| format!("{} has no HEAD commit", root.display()))
}

/// The default branch name: `refs/remotes/origin/HEAD`'s target, else
/// [`FALLBACK_DEFAULT_BRANCH`].
pub fn default_branch(root: &Path) -> String {
    git_stdout(root, &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])
        .and_then(|target| {
            target
                .strip_prefix("refs/remotes/origin/")
                .map(str::to_owned)
        })
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| FALLBACK_DEFAULT_BRANCH.to_owned())
}

/// Refuse a HEAD not reachable from the default branch (the local branch or
/// its `origin` remote-tracking ref).
pub fn require_landed(root: &Path, head: &str) -> Result<(), String> {
    let branch = default_branch(root);
    let candidates = [
        format!("refs/heads/{branch}"),
        format!("refs/remotes/origin/{branch}"),
    ];
    let mut any_ref = false;
    for candidate in &candidates {
        if git_stdout(root, &["rev-parse", "--verify", "-q", candidate]).is_none() {
            continue;
        }
        any_ref = true;
        let output = git(root, &["merge-base", "--is-ancestor", head, candidate])?;
        if output.status.success() {
            return Ok(());
        }
    }
    if !any_ref {
        return Err(format!(
            "the default branch {branch} does not exist locally or on origin; pass --allow-unlanded to build an unlanded commit"
        ));
    }
    Err(format!(
        "HEAD {} is not on the default branch {branch}; pass --allow-unlanded to build it anyway",
        short(head)
    ))
}

/// The first 12 characters of a commit id.
pub fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// Every source check, in order: flywheel checkout (already resolved),
/// clean tree, landed HEAD.
pub fn validate(root: &Path, allow_unlanded: bool) -> Result<Checkout, String> {
    require_clean(root)?;
    let head = head_commit(root)?;
    if !allow_unlanded {
        require_landed(root, &head)?;
    }
    Ok(Checkout {
        kind: SourceKind::Checkout,
        root: root.to_path_buf(),
        head,
    })
}

/// The cargo target dir the build writes: `$CARGO_TARGET_DIR` (relative to
/// the checkout when relative), else `<checkout>/target`.
pub fn target_dir(root: &Path, cargo_target_dir: Option<&str>) -> PathBuf {
    match cargo_target_dir.filter(|v| !v.is_empty()) {
        Some(dir) => {
            let dir = Path::new(dir);
            if dir.is_absolute() {
                dir.to_path_buf()
            } else {
                root.join(dir)
            }
        }
        None => root.join("target"),
    }
}

/// The two artifacts a build produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifacts {
    pub fw: PathBuf,
    pub flywheel: PathBuf,
}

pub fn artifacts(root: &Path, cargo_target_dir: Option<&str>) -> Artifacts {
    let release = target_dir(root, cargo_target_dir).join("release");
    Artifacts {
        fw: release.join("fw"),
        flywheel: release.join("flywheel"),
    }
}

/// Run `cargo build --release --locked -p flywheel` in the checkout. Its
/// stdout and stderr go to this process's stderr so `--json` stdout stays
/// JSON Lines.
pub fn build(root: &Path) -> Result<(), String> {
    use std::os::fd::AsFd;
    let stdout = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("cannot duplicate stderr: {e}"))?;
    let status = Command::new("cargo")
        .args(BUILD_ARGS)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo {} failed ({status})", BUILD_ARGS.join(" ")))
    }
}

/// Parse `fw <version>` (one line) from a `--version` stdout.
pub fn parse_version(stdout: &str) -> Option<String> {
    let line = stdout.trim();
    if line.contains('\n') {
        return None;
    }
    let version = line.strip_prefix("fw ")?;
    let valid = !version.is_empty()
        && version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
    valid.then(|| version.to_owned())
}

/// Run `binary --version` and return the version it prints, requiring exit
/// 0 and `fw <version>`.
pub fn version_of(binary: &Path) -> Result<String, String> {
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{} does not run: {e}", binary.display()))?;
    let deadline = Instant::now() + SMOKE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{} --version did not exit within {}s",
                    binary.display(),
                    SMOKE_TIMEOUT.as_secs()
                ));
            }
            Err(e) => return Err(format!("waiting for {}: {e}", binary.display())),
        }
    };
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        use std::io::Read;
        let _ = pipe.read_to_string(&mut stdout);
    }
    if !status.success() {
        return Err(format!("{} --version exited {status}", binary.display()));
    }
    parse_version(&stdout).ok_or_else(|| {
        format!(
            "{} --version printed {:?}, not 'fw <version>'",
            binary.display(),
            stdout.trim()
        )
    })
}

/// Smoke-check both artifacts; returns the version `fw` reports.
pub fn smoke(artifacts: &Artifacts) -> Result<String, String> {
    let fw = version_of(&artifacts.fw)?;
    let flywheel = version_of(&artifacts.flywheel)?;
    if fw != flywheel {
        return Err(format!(
            "the artifacts disagree on the version: fw {fw}, flywheel {flywheel}"
        ));
    }
    Ok(fw)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
[package]
name = "flywheel"
version = "0.1.0"

[[bin]]
name = "flywheel"
path = "src/main.rs"

[[bin]]
name = "fw"
path = "src/bin/fw.rs"
"#;

    #[test]
    fn the_real_manifest_is_a_flywheel_manifest() {
        manifest_is_flywheel(include_str!("../../Cargo.toml")).unwrap();
        manifest_is_flywheel(GOOD).unwrap();
    }

    #[test]
    fn a_manifest_missing_the_package_or_a_bin_is_refused() {
        let renamed = GOOD.replacen("name = \"flywheel\"", "name = \"other\"", 1);
        assert!(
            manifest_is_flywheel(&renamed)
                .unwrap_err()
                .contains("package")
        );
        let no_fw = GOOD.replace("name = \"fw\"", "name = \"fwx\"");
        assert!(manifest_is_flywheel(&no_fw).unwrap_err().contains("fw"));
        assert!(manifest_is_flywheel("[package]\nname = \"flywheel\"\n").is_err());
        assert!(manifest_is_flywheel("not toml [").is_err());
    }

    #[test]
    fn versions_parse_only_from_fw_lines() {
        assert_eq!(parse_version("fw 0.6.1\n").as_deref(), Some("0.6.1"));
        assert_eq!(
            parse_version("fw 1.2.3-rc.1+abc").as_deref(),
            Some("1.2.3-rc.1+abc")
        );
        for bad in [
            "",
            "fw",
            "fw ",
            "flywheel 0.6.1",
            "fw 0.6.1\nmore",
            "fw x.y",
        ] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn target_dir_honours_cargo_target_dir() {
        let root = Path::new("/src");
        assert_eq!(target_dir(root, None), Path::new("/src/target"));
        assert_eq!(target_dir(root, Some("")), Path::new("/src/target"));
        assert_eq!(target_dir(root, Some("/t")), Path::new("/t"));
        assert_eq!(target_dir(root, Some("out")), Path::new("/src/out"));
        let a = artifacts(root, None);
        assert_eq!(a.fw, Path::new("/src/target/release/fw"));
        assert_eq!(a.flywheel, Path::new("/src/target/release/flywheel"));
    }

    #[test]
    fn short_commit_is_twelve_chars() {
        assert_eq!(short("0123456789abcdef"), "0123456789ab");
        assert_eq!(short("abc"), "abc");
    }

    fn sh_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn clean_and_landed_checks_against_a_scratch_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("crates/flywheel")).unwrap();
        std::fs::write(repo.join(MANIFEST), GOOD).unwrap();
        std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        sh_git(&repo, &["init", "-q", "-b", "main"]);
        sh_git(&repo, &["add", "-A"]);
        sh_git(&repo, &["commit", "-qm", "init", "--no-gpg-sign"]);
        let root = flywheel_checkout(&repo.join("crates")).unwrap();
        assert_eq!(root, repo.canonicalize().unwrap());
        let checkout = validate(&root, false).unwrap();
        assert_eq!(checkout.head.len(), 40);

        // Ignored output does not dirty the tree; an untracked file does.
        std::fs::create_dir_all(repo.join("target/release")).unwrap();
        std::fs::write(repo.join("target/release/fw"), "x").unwrap();
        validate(&root, false).unwrap();
        std::fs::write(repo.join("stray.txt"), "x").unwrap();
        assert!(validate(&root, false).unwrap_err().contains("stray.txt"));
        std::fs::remove_file(repo.join("stray.txt")).unwrap();

        // A commit only on a feature branch is unlanded.
        sh_git(&repo, &["checkout", "-qb", "feature"]);
        std::fs::write(repo.join("extra.txt"), "x").unwrap();
        sh_git(&repo, &["add", "-A"]);
        sh_git(&repo, &["commit", "-qm", "extra", "--no-gpg-sign"]);
        let err = validate(&root, false).unwrap_err();
        assert!(err.contains("--allow-unlanded"), "{err}");
        validate(&root, true).unwrap();

        // Not a flywheel checkout.
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        sh_git(&other, &["init", "-q", "-b", "main"]);
        assert!(flywheel_checkout(&other).is_err());
        assert!(resolve(None, &other, None).is_err());
        assert_eq!(resolve(None, &other, Some(&root)).unwrap(), root);
        assert!(flywheel_checkout(tmp.path()).is_err());
    }
}
