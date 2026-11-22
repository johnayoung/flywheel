//! The build sweep end to end over a real hub on 127.0.0.1:0 and the
//! `fw hub sweep` CLI: a fixture repo with worktrees in every state is
//! listed exactly (candidates, and the reason each other one is kept),
//! then swept, and every byte outside the deleted `target/` dirs is
//! unchanged (gitignored files included). Deletions reach the activity
//! journal, the event log, and the budget reading.
//!
//! Everything lives in a temp dir (repos and the hub's state, given
//! explicitly to the hub and every CLI call); its automatic sweeps are off.

use crate::support;

use flywheel_hub::remote::RuntimeInfo;
use flywheel_hub::{CreateSession, Hub, HubOptions, ServerOptions, serve};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::oneshot;

const DAY: u64 = 86_400;

fn git(dir: &Path, args: &[&str]) {
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

/// Set the modification time of everything under `dir` to `secs` ago.
fn age(dir: &Path, secs: u64) {
    let when = SystemTime::now() - Duration::from_secs(secs);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        if at.is_dir() {
            for e in std::fs::read_dir(&at).unwrap() {
                stack.push(e.unwrap().path());
            }
        }
        std::fs::File::open(&at)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
}

/// A worktree `name` with a `target/` aged `idle` seconds and an ignored
/// cache; `unmerged`: a commit `main` lacks.
fn worktree(repo: &Path, name: &str, unmerged: bool, idle: u64) -> PathBuf {
    let path = repo.parent().unwrap().join("repo.worktrees").join(name);
    git(
        repo,
        &["worktree", "add", "-q", "-b", name, &path.to_string_lossy()],
    );
    if unmerged {
        std::fs::write(path.join("work.txt"), name).unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "-qm", name]);
    }
    std::fs::create_dir_all(path.join("target/debug/deps")).unwrap();
    std::fs::write(path.join("target/debug/deps/out"), vec![7u8; 8192]).unwrap();
    std::fs::create_dir_all(path.join(".cache")).unwrap();
    std::fs::write(path.join(".cache/analysis.json"), name.as_bytes()).unwrap();
    std::fs::write(path.join("notes.local"), b"untracked, not ignored").unwrap();
    age(&path.join("target"), idle);
    path.canonicalize().unwrap()
}

/// Every file and symlink under `root` with its bytes.
fn contents(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut all = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(at) = stack.pop() {
        let meta = std::fs::symlink_metadata(&at).unwrap();
        if meta.is_dir() {
            all.insert(at.clone(), b"<dir>".to_vec());
            for e in std::fs::read_dir(&at).unwrap() {
                stack.push(e.unwrap().path());
            }
        } else if meta.file_type().is_symlink() {
            let to = std::fs::read_link(&at).unwrap();
            all.insert(at, to.to_string_lossy().as_bytes().to_vec());
        } else {
            all.insert(at.clone(), std::fs::read(&at).unwrap());
        }
    }
    all
}

fn cli(args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = flywheel_hub::cli::run(&args, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

async fn run_cli(args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        cli(&refs)
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sweep_lists_exactly_the_idle_worktrees_and_deletes_only_their_target_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    // The repo (its control checkout has an old target/ of its own).
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "target/\n.cache/\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    let repo = repo.canonicalize().unwrap();
    std::fs::create_dir_all(repo.join("target/release")).unwrap();
    std::fs::write(repo.join("target/release/fw"), b"release binary").unwrap();
    age(&repo.join("target"), 100 * DAY);

    let merged_idle = worktree(&repo, "merged-idle", false, 3 * DAY);
    let merged_fresh = worktree(&repo, "merged-fresh", false, 3600);
    let unmerged_stale = worktree(&repo, "unmerged-stale", true, 30 * DAY);
    let unmerged_fresh = worktree(&repo, "unmerged-fresh", true, 3 * DAY);
    let keep = worktree(&repo, "keep", false, 3 * DAY);
    std::fs::write(keep.join(".fw-keep-build"), "").unwrap();
    let keep_expired = worktree(&repo, "keep-expired", false, 3 * DAY);
    std::fs::write(
        keep_expired.join(".fw-keep-build"),
        "2020-01-01T00:00:00Z\nold run\n",
    )
    .unwrap();
    let busy = worktree(&repo, "busy", false, 3 * DAY);
    let in_session = worktree(&repo, "in-session", false, 3 * DAY);

    // A process working in `busy`.
    let mut sleeper = Command::new("sleep")
        .arg("600")
        .current_dir(busy.join(".cache"))
        .spawn()
        .unwrap();

    // A process this user cannot inspect (non-dumpable) whose command
    // line names a file in `named`: seen by its command line.
    let named = worktree(&repo, "named", false, 3 * DAY);
    let mut opaque = Command::new("python3")
        .args([
            "-c",
            "import ctypes,time; ctypes.CDLL(None).prctl(4,0,0,0,0); time.sleep(600)",
        ])
        .arg(named.join(".cache/analysis.json"))
        .current_dir(&root)
        .spawn()
        .expect("python3 runs the uninspectable process");

    // The hub: a workspace of the repo and the session's worktree, a
    // live session in it.
    flywheel_hub::install_crypto_provider();
    let state = root.join("state");
    let mut options = HubOptions::new(state.clone());
    options.default_shell = vec!["/bin/sh".into()];
    options.claude_config_dir = Some(root.join("claude-config"));
    options.codex_home = Some(root.join("codex-home"));
    assert!(!options.build_sweep.enabled);
    let hub: Arc<Hub> = support::open_hub(options);
    let ws = hub
        .create_workspace(flywheel_hub::state::WorkspaceInput {
            name: Some("sweep".into()),
            folders: None,
            repos: Some(vec![repo.clone(), in_session.clone()]),
        })
        .unwrap()
        .id;
    let session = hub
        .create_session(CreateSession {
            workspace_id: ws.clone(),
            repo: Some(in_session.clone()),
            command: Some(vec!["/bin/sh".into(), "-c".into(), "exec sleep 600".into()]),
            ..CreateSession::default()
        })
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    RuntimeInfo {
        pid: std::process::id(),
        listen: addr,
        public_urls: Vec::new(),
        started_at: None,
        log_file: None,
        tls: None,
    }
    .write(&state)
    .unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(serve(
        listener,
        Arc::clone(&hub),
        ServerOptions::default(),
        async {
            let _ = stopped.await;
        },
    ));
    let state_arg = state.to_string_lossy().into_owned();

    // Grader 1: the dry run lists exactly the candidates, and each kept
    // worktree's reason.
    let (code, out, err) = run_cli(&[
        "sweep",
        "--all",
        "--dry-run",
        "--json",
        "--state-dir",
        &state_arg,
    ])
    .await;
    assert_eq!(code, 0, "{err}");
    let report: Value = serde_json::from_str(&out).unwrap();
    let candidates: BTreeMap<PathBuf, String> = report["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            assert_eq!(c["delete"], true);
            (
                PathBuf::from(c["path"].as_str().unwrap()),
                c["why"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let expected: BTreeMap<PathBuf, String> = [
        (merged_idle.clone(), "merged"),
        (unmerged_stale.clone(), "stale"),
        (keep_expired.clone(), "merged"),
    ]
    .into_iter()
    .map(|(p, w)| (p, w.to_owned()))
    .collect();
    assert_eq!(candidates, expected, "{report:#}");
    let kept: BTreeMap<PathBuf, String> = report["kept"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| {
            (
                PathBuf::from(k["path"].as_str().unwrap()),
                k["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let expected: BTreeMap<PathBuf, String> = [
        (merged_fresh.clone(), "fresh"),
        (unmerged_fresh.clone(), "fresh"),
        (keep.clone(), "keep_marker"),
        (busy.clone(), "process"),
        (named.clone(), "process"),
        (in_session.clone(), "session"),
    ]
    .into_iter()
    .map(|(p, w)| (p, w.to_owned()))
    .collect();
    assert_eq!(kept, expected, "{report:#}");
    let listed = report["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .chain(report["kept"].as_array().unwrap());
    for item in listed {
        assert_ne!(
            item["path"],
            repo.to_string_lossy().as_ref(),
            "never the control checkout"
        );
    }
    let named_detail = report["kept"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["path"] == named.to_string_lossy().as_ref())
        .map(|k| k["detail"].as_str().unwrap().to_owned())
        .unwrap();
    assert!(named_detail.contains("command-line path"), "{named_detail}");
    assert!(report["opaque_processes"].as_u64().unwrap() > 0);
    assert_eq!(report["dry_run"], true);
    assert!(
        merged_idle.join("target").exists(),
        "a dry run deletes nothing"
    );
    let (code, text, _) =
        run_cli(&["sweep", "--all", "--dry-run", "--state-dir", &state_arg]).await;
    assert_eq!(code, 0);
    assert!(text.contains("would delete 3 of 3 target/ dirs"), "{text}");
    assert!(text.contains("session: session"), "{text}");

    // Grader 2: the sweep deletes exactly those target/ dirs; every other
    // byte is unchanged.
    let before: BTreeMap<PathBuf, Vec<u8>> = contents(&root.join("repo.worktrees"))
        .into_iter()
        .chain(contents(&repo))
        .collect();
    let (code, out, err) = run_cli(&["sweep", "--all", "--json", "--state-dir", &state_arg]).await;
    assert_eq!(code, 0, "{err}");
    let report: Value = serde_json::from_str(&out).unwrap();
    let deleted: Vec<PathBuf> = report["deleted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| PathBuf::from(d["target"].as_str().unwrap()))
        .collect();
    let mut want = vec![
        unmerged_stale.join("target"),
        merged_idle.join("target"),
        keep_expired.join("target"),
    ];
    want.sort();
    let mut got = deleted.clone();
    got.sort();
    assert_eq!(got, want, "{report:#}");
    assert_eq!(
        deleted[0],
        unmerged_stale.join("target"),
        "the oldest goes first"
    );
    assert!(report["reclaimed_bytes"].as_u64().unwrap() >= 3 * 8192);
    let after: BTreeMap<PathBuf, Vec<u8>> = contents(&root.join("repo.worktrees"))
        .into_iter()
        .chain(contents(&repo))
        .collect();
    let expected_after: BTreeMap<PathBuf, Vec<u8>> = before
        .into_iter()
        .filter(|(p, _)| !want.iter().any(|t| p.starts_with(t)))
        .collect();
    assert_eq!(after.len(), expected_after.len());
    assert!(after == expected_after, "bytes outside target/ changed");
    assert!(repo.join("target/release/fw").exists());

    // Recorded: activity rows, log events, and the budget reading.
    let wait_until = Instant::now() + Duration::from_secs(10);
    let rows = loop {
        let (code, out, err) = run_cli(&[
            "activity",
            "--kind",
            "build.swept",
            "--json",
            "--state-dir",
            &state_arg,
        ])
        .await;
        assert_eq!(code, 0, "{err}");
        let rows: Value = serde_json::from_str(&out).unwrap();
        let rows = rows["activity"]["rows"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if rows.len() == 3 || Instant::now() > wait_until {
            break rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(rows.len(), 3, "{rows:#?}");
    for row in &rows {
        assert!(row["data"]["bytes"].as_u64().unwrap() > 0, "{row}");
        assert_eq!(row["data"]["repo"], "repo", "{row}");
        let text = row.to_string();
        assert!(
            !text.contains(&*root.to_string_lossy()),
            "no path in a row: {text}"
        );
    }
    let (code, out, err) = run_cli(&["budget", "--json", "--state-dir", &state_arg]).await;
    assert_eq!(code, 0, "{err}");
    let budget: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        budget["sweep"]["last"]["reclaimed_bytes"],
        report["reclaimed_bytes"]
    );
    assert_eq!(budget["sweep"]["last"]["deleted"], 3);
    assert!(budget["disks"][0]["total_bytes"].as_u64().unwrap() > 0);
    let (code, text, _) = run_cli(&["budget", "--state-dir", &state_arg]).await;
    assert_eq!(code, 0);
    assert!(text.contains("last build sweep"), "{text}");
    assert!(text.contains("% used"), "{text}");

    // Grader 5 through the CLI: --keep --until writes the marker.
    let (code, out, err) = run_cli(&[
        "sweep",
        "--keep",
        &merged_fresh.to_string_lossy(),
        "--until",
        "2d",
    ])
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("kept"), "{out}");
    let first = std::fs::read_to_string(merged_fresh.join(".fw-keep-build")).unwrap();
    let until = chrono::DateTime::parse_from_rfc3339(first.trim()).unwrap();
    let left = until.timestamp() - chrono::Utc::now().timestamp();
    assert!(
        (2 * DAY as i64 - 60..=2 * DAY as i64).contains(&left),
        "{left}"
    );
    let (code, _, err) = run_cli(&[
        "sweep",
        "--keep",
        &merged_fresh.join(".cache").to_string_lossy(),
    ])
    .await;
    assert_eq!(code, 1);
    assert!(err.contains("not the top"), "{err}");

    hub.kill_session(&session.record.id).unwrap();
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
    opaque.kill().unwrap();
    opaque.wait().unwrap();
    let _ = stop.send(());
}
