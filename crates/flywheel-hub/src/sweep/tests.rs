use super::*;
use std::process::Command;

pub(crate) fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A Cargo repo at `root/repo` on `main` with `target/` ignored.
pub(crate) fn repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "target/\n.cache/\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    repo.canonicalize().unwrap()
}

/// A worktree `name` of `repo` with a `target/` of `bytes` and an ignored
/// cache; `unmerged`: one commit `main` lacks.
pub(crate) fn worktree(repo: &Path, name: &str, unmerged: bool, bytes: usize) -> PathBuf {
    let path = repo.parent().unwrap().join("repo.worktrees").join(name);
    git(
        repo,
        &["worktree", "add", "-q", "-b", name, &path.to_string_lossy()],
    );
    if unmerged {
        std::fs::write(path.join(format!("{name}.txt")), name).unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "-qm", name]);
    }
    std::fs::create_dir_all(path.join("target/debug/deps")).unwrap();
    std::fs::write(path.join("target/debug/deps/out"), vec![7u8; bytes]).unwrap();
    std::fs::create_dir_all(path.join(".cache")).unwrap();
    std::fs::write(path.join(".cache/analysis.json"), b"{\"keep\": true}").unwrap();
    path.canonicalize().unwrap()
}

/// Set the modification time of everything under `dir` to `secs` ago.
pub(crate) fn age(dir: &Path, secs: u64) {
    let when = SystemTime::now() - Duration::from_secs(secs);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        if at.is_dir() {
            for e in std::fs::read_dir(&at).unwrap() {
                stack.push(e.unwrap().path());
            }
        }
        let f = std::fs::File::open(&at).unwrap();
        f.set_modified(when).unwrap();
    }
}

const DAY: u64 = 86_400;

fn nobody(_: &Path) -> Option<String> {
    None
}

fn look<'a>(
    repos: &'a [PathBuf],
    sessions: &'a dyn Fn(&Path) -> Option<String>,
    all: bool,
) -> Look<'a> {
    Look {
        repos,
        folders: repos,
        config: SweepConfig::default(),
        sessions,
        all,
    }
}

#[test]
fn keep_markers_read_forever_until_expired_and_bad() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc::now();
    assert_eq!(marker(dir.path(), now), Marker::None);
    std::fs::write(dir.path().join(KEEP_FILE), "").unwrap();
    assert_eq!(marker(dir.path(), now), Marker::Forever);
    std::fs::write(dir.path().join(KEEP_FILE), "2999-01-01T00:00:00Z\nwhy\n").unwrap();
    assert!(matches!(marker(dir.path(), now), Marker::Until(_)));
    std::fs::write(dir.path().join(KEEP_FILE), "2000-01-01T00:00:00Z\n").unwrap();
    assert!(matches!(marker(dir.path(), now), Marker::Expired(_)));
    assert!(!marker(dir.path(), now).keeps());
    std::fs::write(dir.path().join(KEEP_FILE), "next tuesday\n").unwrap();
    let bad = marker(dir.path(), now);
    assert!(matches!(bad, Marker::Bad(_)) && bad.keeps());
}

#[test]
fn the_control_checkout_and_unignored_or_symlinked_builds_are_never_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = repo(&root);
    std::fs::create_dir_all(repo.join("target/release")).unwrap();
    std::fs::write(repo.join("target/release/fw"), b"bin").unwrap();
    age(&repo.join("target"), 100 * DAY);
    let w = worktree(&repo, "linked", false, 10);
    std::fs::remove_dir_all(w.join("target")).unwrap();
    let shared = root.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::os::unix::fs::symlink(&shared, w.join("target")).unwrap();
    let repos = [repo.clone()];
    let mut report = survey(&look(&repos, &nobody, true));
    assert!(report.candidates.is_empty(), "{report:#?}");
    assert_eq!(report.kept.len(), 1);
    assert_eq!(report.kept[0].reason, Reason::NoBuild);
    assert!(report.kept.iter().all(|k| k.path != repo));
    execute(&mut report, SweepConfig::default(), &nobody, &mut |_| {});
    assert!(repo.join("target/release/fw").exists());
    assert!(shared.exists());
}

#[test]
fn a_process_that_starts_in_a_candidate_before_deletion_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = repo(&root);
    let a = worktree(&repo, "a", false, 4096);
    let b = worktree(&repo, "b", true, 4096);
    age(&a.join("target"), 3 * DAY);
    age(&b.join("target"), 30 * DAY);
    let repos = [repo];
    let mut report = survey(&look(&repos, &nobody, true));
    let listed: Vec<&Path> = report.candidates.iter().map(|c| c.path.as_path()).collect();
    assert_eq!(listed, [b.as_path(), a.as_path()], "oldest first");
    // Between the listing and the deletion, a process starts in `a`.
    let mut child = Command::new("sleep")
        .arg("60")
        .current_dir(a.join(".cache"))
        .spawn()
        .unwrap();
    let mut told = Vec::new();
    execute(&mut report, SweepConfig::default(), &nobody, &mut |d| {
        told.push(d.path.clone());
    });
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(told, std::slice::from_ref(&b));
    assert!(!b.join("target").exists());
    assert!(a.join("target/debug/deps/out").exists());
    assert_eq!(report.skipped.len(), 1, "{report:#?}");
    assert_eq!(report.skipped[0].path, a);
    assert_eq!(report.skipped[0].reason, Reason::Process);
    assert!(report.skipped[0].detail.contains("sleep"), "{report:#?}");
    // A session that appears in `a` is seen the same way.
    let a2 = a.clone();
    let in_a = move |p: &Path| (p == a2).then(|| "late (s1)".to_owned());
    let mut report = survey(&look(&repos, &nobody, true));
    execute(&mut report, SweepConfig::default(), &in_a, &mut |_| {});
    assert_eq!(report.skipped[0].reason, Reason::Session);
    assert!(a.join("target").exists());
}

#[test]
fn a_keep_until_keeps_the_worktree_until_then_and_not_after() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = repo(&root);
    let w = worktree(&repo, "kept", false, 64);
    age(&w.join("target"), 5 * DAY);
    let until = Utc::now() + chrono::Duration::seconds(2);
    let path = write_keep(&w, Some(until)).unwrap();
    assert_eq!(path, w.join(KEEP_FILE));
    let first = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        DateTime::parse_from_rfc3339(first.trim())
            .unwrap()
            .timestamp(),
        until.timestamp()
    );
    let repos = [repo];
    let report = survey(&look(&repos, &nobody, true));
    assert!(report.candidates.is_empty());
    assert_eq!(report.kept[0].reason, Reason::KeepMarker);
    std::thread::sleep(Duration::from_millis(2500));
    let report = survey(&look(&repos, &nobody, true));
    assert_eq!(report.candidates.len(), 1, "{report:#?}");
    assert_eq!(report.candidates[0].path, w);
    // Not the top of a work tree: refused.
    assert!(write_keep(&w.join(".cache"), None).is_err());
}

/// The tmpfs a watermark sweep runs on is mounted in a user and mount
/// namespace of its own: this test runs the inner one there.
#[test]
fn the_watermark_sweep_deletes_oldest_first_until_under_the_low_mark() {
    let has_unshare = Command::new("unshare")
        .args(["-rm", "true"])
        .status()
        .is_ok_and(|s| s.success());
    if !has_unshare {
        eprintln!("skipped: no unprivileged user namespaces (unshare -rm)");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = Command::new("unshare")
        .arg("-rm")
        .arg(&exe)
        .args([
            "sweep::tests::watermark_on_a_small_tmpfs",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("FW_SWEEP_TMPFS_TEST", "1")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed"), "{text}");
}

#[test]
#[ignore = "run inside a mount namespace by the_watermark_sweep_deletes_oldest_first_until_under_the_low_mark"]
fn watermark_on_a_small_tmpfs() {
    if std::env::var_os("FW_SWEEP_TMPFS_TEST").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let status = Command::new("mount")
        .args(["-t", "tmpfs", "-o", "size=4m", "tmpfs"])
        .arg(&root)
        .status()
        .unwrap();
    assert!(status.success());
    let repo = repo(&root);
    let names = ["w1", "w2", "w3", "w4"];
    let trees: Vec<PathBuf> = names
        .iter()
        .map(|n| worktree(&repo, n, false, 200 * 1024))
        .collect();
    for (i, w) in trees.iter().enumerate() {
        // w1 the oldest.
        age(&w.join("target"), (10 - i as u64) * DAY);
    }
    // Fill the disk to 92% used.
    let d = disk::disk_use(&root).unwrap();
    let all = d.used_bytes + d.free_bytes;
    let want = all * 92 / 100;
    let fill = usize::try_from(want.saturating_sub(d.used_bytes)).unwrap();
    std::fs::write(root.join("filler"), vec![1u8; fill]).unwrap();
    let before = disk::disk_use(&root).unwrap();
    assert!(before.used_pct >= 90.0, "{before:?}");
    let repos = [repo];
    let mut report = survey(&look(&repos, &nobody, false));
    assert_eq!(report.candidates.len(), 4, "{report:#?}");
    let picked: Vec<&Path> = report
        .candidates
        .iter()
        .filter(|c| c.delete)
        .map(|c| c.path.as_path())
        .collect();
    assert_eq!(picked, [trees[0].as_path(), trees[1].as_path()]);
    let mut order = Vec::new();
    execute(&mut report, SweepConfig::default(), &nobody, &mut |d| {
        order.push(d.path.clone());
    });
    assert_eq!(order, [trees[0].clone(), trees[1].clone()], "{report:#?}");
    assert!(!report.short);
    let after = disk::disk_use(&root).unwrap();
    assert!(after.used_pct < 85.0, "{after:?}");
    assert!(trees[2].join("target").exists() && trees[3].join("target").exists());
    // Past the high mark with nothing left: short.
    std::fs::write(root.join("filler2"), vec![1u8; 400 * 1024]).unwrap();
    for w in &trees[2..] {
        write_keep(w, None).unwrap();
    }
    let mut report = survey(&look(&repos, &nobody, false));
    assert!(report.short, "{report:#?}");
    execute(&mut report, SweepConfig::default(), &nobody, &mut |_| {});
    assert!(report.short && report.deleted.is_empty());
    let _ = Command::new("umount").arg(&root).status();
}
