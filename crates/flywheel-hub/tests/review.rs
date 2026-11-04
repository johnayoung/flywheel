//! Review from anywhere, end to end: a worktree agent session in a real
//! git repo, commits made through its companion shell, the review API
//! (commits, files, diffs, working tree), the guarded actions (commit all,
//! push to a bare remote, PR refusals, rebase, merge into the base in the
//! main checkout, then removing the worktree), and `fw hub review` /
//! `fw hub merge`.

mod support;

use flywheel_hub::remote::RuntimeInfo;
use flywheel_hub::review::Tools;
use flywheel_hub::{HubOptions, ServerOptions, serve};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct TestHub {
    dir: tempfile::TempDir,
    root: PathBuf,
    addr: std::net::SocketAddr,
    token: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    /// `<tmp>/code/app` (main, one commit, a local identity) with a bare
    /// `origin` at `<tmp>/remote.git`; `gh` is `tools.gh`.
    async fn start(tools: Tools) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("code/app");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Hub Test"]);
        git(&repo, &["config", "user.email", "hub@example.com"]);
        std::fs::write(repo.join("README"), "one\ntwo\n").unwrap();
        git(&repo, &["add", "README"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        let bare = root.join("remote.git");
        git(
            &root,
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&repo, &["push", "-q", "-u", "origin", "main"]);

        flywheel_hub::install_crypto_provider();
        let mut options = HubOptions::new(root.join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        let claude = root.join("claude");
        support::write_script(&claude, "#!/bin/sh\nexec cat\n");
        options.agents.claude = claude.to_string_lossy().into_owned();
        options.claude_config_dir = Some(root.join("claude-config"));
        options.codex_home = Some(root.join("codex-home"));
        let hub = support::open_hub(options);
        let token = hub.token().expose().to_owned();
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
        .write(&root.join("state"))
        .unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server_options = ServerOptions {
            review_tools: tools,
            ..ServerOptions::default()
        };
        let server = tokio::spawn(serve(listener, hub, server_options, async {
            let _ = stopped.await;
        }));
        Self {
            dir,
            root,
            addr,
            token,
            http: reqwest::Client::new(),
            stop: Some(stop),
            server: Some(server),
        }
    }

    fn repo(&self) -> PathBuf {
        self.root.join("code/app")
    }

    fn state_dir(&self) -> String {
        self.root.join("state").to_string_lossy().into_owned()
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .http
            .request(method, format!("http://{}{path}", self.addr))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    async fn get(&self, path: &str) -> Value {
        let (status, value) = self.call(reqwest::Method::GET, path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {value}");
        value
    }

    /// Plan `action`, then run it with the plan's fingerprint.
    async fn act(&self, session: &str, action: &str, body: Value) -> (Value, (StatusCode, Value)) {
        let mut dry = body.clone();
        dry["dry_run"] = json!(true);
        let (status, plan) = self
            .post(&format!("/api/sessions/{session}/review/{action}"), dry)
            .await;
        assert_eq!(status, StatusCode::OK, "{action} plan: {plan}");
        let mut run = body;
        run["confirm"] = plan["fingerprint"].clone();
        let done = self
            .post(&format!("/api/sessions/{session}/review/{action}"), run)
            .await;
        (plan, done)
    }

    async fn shutdown(mut self) {
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(15), self.server.take().unwrap())
            .await
            .expect("server shut down in time")
            .unwrap()
            .unwrap();
        drop(self.dir);
    }
}

fn cli(args: Vec<String>) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = flywheel_hub::cli::run(&args, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

async fn run_cli(args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    tokio::task::spawn_blocking(move || cli(args))
        .await
        .unwrap()
}

/// Wait until the screen text of `session` holds `needle`.
async fn screen_shows(hub: &TestHub, session: &str, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let screen = hub
            .get(&format!(
                "/api/sessions/{session}/screen?source=recent&lines=200"
            ))
            .await;
        let text = screen["lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        if text.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{needle:?} not on {session}'s screen:\n{text}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait until `cond` holds for the review of `session`.
async fn review_until(
    hub: &TestHub,
    session: &str,
    what: &str,
    cond: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let review = hub.get(&format!("/api/sessions/{session}/review")).await;
        if cond(&review) {
            return review;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {review}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worktree_session_is_reviewed_committed_pushed_merged_and_removed() {
    let hub = TestHub::start(Tools {
        gh: "/nonexistent/gh".into(),
        ..Tools::default()
    })
    .await;
    let (status, ws) = hub
        .post(
            "/api/workspaces",
            json!({ "name": "w", "folders": [hub.repo()] }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{ws}");
    let ws = ws["id"].as_str().unwrap().to_owned();
    let (status, agent) = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": ws, "kind": "claude", "worktree": { "task": "feat" } }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_owned();
    let wt = PathBuf::from(agent["worktree"]["path"].as_str().unwrap());

    // A fresh worktree: nothing to review yet.
    let review = hub.get(&format!("/api/sessions/{agent_id}/review")).await;
    assert_eq!(review["branch"], "feat");
    assert_eq!(review["base"]["name"], "main");
    assert_eq!(review["base"]["source"], "worktree");
    assert_eq!(review["ahead"], 0);
    assert_eq!(review["mergeable"], true);
    assert_eq!(review["agent"]["id"], agent_id);
    assert_eq!(review["remotes"][0]["name"], "origin");
    // A base that names nothing is an error, not "nothing to review"; so
    // is the diff of a path the review does not list.
    let (status, body) = hub
        .call(
            reqwest::Method::GET,
            &format!("/api/sessions/{agent_id}/review?base=nosuch"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("names no branch or commit"),
        "{body}"
    );
    let (status, body) = hub
        .call(
            reqwest::Method::GET,
            &format!("/api/sessions/{agent_id}/review/diff?scope=committed&path=nonexistent"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Commits made through the companion shell show up as the branch's.
    let (status, sent) = hub
        .post(
            &format!("/api/sessions/{agent_id}/companion"),
            json!({ "text": "printf 'fn main() {}\\n' > main.rs && git add main.rs && git commit -qm 'feat: main' && echo COMMITTED", "run": true }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{sent}");
    let companion = sent["session"]["id"].as_str().unwrap().to_owned();
    screen_shows(&hub, &companion, "COMMITTED").await;
    let review = review_until(&hub, &agent_id, "the companion's commit", |r| {
        r["ahead"] == 1
    })
    .await;
    assert_eq!(review["commits"][0]["subject"], "feat: main");
    assert_eq!(review["committed"]["files"][0]["path"], "main.rs");
    assert_eq!(review["committed"]["files"][0]["insertions"], 1);
    // The companion reviews the same worktree, against the same base.
    let from_companion = hub.get(&format!("/api/sessions/{companion}/review")).await;
    assert_eq!(from_companion["base"]["name"], "main");
    assert_eq!(from_companion["agent"]["id"], agent_id);
    assert_eq!(from_companion["commits"], review["commits"]);

    // Working-tree changes, apart from the committed ones.
    std::fs::write(wt.join("README"), "one\nTWO\n").unwrap();
    std::fs::write(wt.join("notes.txt"), "remember\n").unwrap();
    let review = hub.get(&format!("/api/sessions/{agent_id}/review")).await;
    assert_eq!(review["unstaged"]["files"][0]["path"], "README");
    assert_eq!(review["untracked"][0]["path"], "notes.txt");
    assert_eq!(review["committed"]["files"].as_array().unwrap().len(), 1);
    let diff = hub
        .get(&format!(
            "/api/sessions/{agent_id}/review/diff?scope=unstaged&path=README"
        ))
        .await;
    let lines: Vec<&str> = diff["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        lines.contains(&"+TWO") && lines.contains(&"-two"),
        "{lines:?}"
    );
    let diff = hub
        .get(&format!(
            "/api/sessions/{agent_id}/review/diff?scope=committed&path=main.rs"
        ))
        .await;
    assert!(
        diff["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l == "+fn main() {}"),
        "{diff}"
    );
    let (status, bad) = hub
        .call(
            reqwest::Method::GET,
            &format!("/api/sessions/{agent_id}/review/diff?scope=unstaged&path=../x"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");

    // Asking the agent pastes the prompt into it (the stand-in echoes).
    let (status, ask) = hub
        .post(
            &format!("/api/sessions/{companion}/review/ask"),
            json!({ "kind": "commit_message", "dry_run": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{ask}");
    assert_eq!(ask["agent"]["id"], agent_id);
    assert!(ask.get("paste").is_none());
    let (status, ask) = hub
        .post(
            &format!("/api/sessions/{companion}/review/ask"),
            json!({ "kind": "commit_message" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{ask}");
    assert_eq!(ask["paste"]["ran"], true);
    screen_shows(&hub, &agent_id, "Write a commit message").await;

    // Commit all names a secret it leaves out by default; `exclude` (here
    // none) says exactly what is left out.
    std::fs::write(wt.join(".env"), "TOKEN=x\n").unwrap();
    let (status, plan) = hub
        .post(
            &format!("/api/sessions/{agent_id}/review/commit"),
            json!({ "message": "x", "dry_run": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    assert_eq!(
        plan["suspects"],
        json!([{ "path": ".env", "kind": "secret", "excluded": true, "files": 1 }])
    );
    assert_eq!(plan["excluded"], json!([".env"]));
    let (status, plan) = hub
        .post(
            &format!("/api/sessions/{agent_id}/review/commit"),
            json!({ "message": "x", "dry_run": true, "exclude": [] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    assert_eq!(plan["suspects"][0]["excluded"], false, "{plan}");
    assert!(plan.get("excluded").is_none(), "{plan}");
    std::fs::remove_file(wt.join(".env")).unwrap();

    // Commit all: no fingerprint, nothing runs; a stale one is refused.
    let (status, refused) = hub
        .post(
            &format!("/api/sessions/{agent_id}/review/commit"),
            json!({ "message": "x" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let (status, refused) = hub
        .post(
            &format!("/api/sessions/{agent_id}/review/commit"),
            json!({ "message": "x", "confirm": "0123" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    let (plan, (status, done)) = hub
        .act(
            &agent_id,
            "commit",
            json!({ "message": "docs: readme and notes" }),
        )
        .await;
    assert_eq!(
        plan["commands"][1]["display"],
        format!("git -C {} commit -F - < the message", wt.display())
    );
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["ok"], true);
    assert_eq!(
        git(&wt, &["log", "-1", "--format=%s"]),
        "docs: readme and notes"
    );
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");

    // A PR before pushing, and without gh: both refused, saying why.
    let (status, plan) = hub
        .post(
            &format!("/api/sessions/{agent_id}/review/pr"),
            json!({ "dry_run": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        plan["refusal"].as_str().unwrap().contains("not pushed"),
        "{plan}"
    );

    // Push: the exact command, then the branch on the bare remote.
    let (plan, (status, done)) = hub.act(&agent_id, "push", json!({})).await;
    assert_eq!(
        plan["commands"][0]["display"],
        format!(
            "git -C {} push origin {}:refs/heads/feat",
            wt.display(),
            git(&wt, &["rev-parse", "HEAD"])
        )
    );
    assert_eq!(
        plan["commands"][1]["display"],
        format!(
            "git -C {} branch --set-upstream-to=origin/feat feat",
            wt.display()
        )
    );
    assert_eq!(status, StatusCode::OK, "{done}");
    let bare = hub.root.join("remote.git");
    assert_eq!(
        git(&bare, &["rev-parse", "feat"]),
        git(&wt, &["rev-parse", "HEAD"])
    );
    let (_, plan) = hub
        .post(
            &format!("/api/sessions/{agent_id}/review/pr"),
            json!({ "dry_run": true }),
        )
        .await;
    assert!(
        plan["refusal"].as_str().unwrap().contains("not on a host"),
        "{plan}"
    );
    git(
        &hub.repo(),
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            "https://github.com/o/r.git",
        ],
    );
    let (_, plan) = hub
        .post(
            &format!("/api/sessions/{agent_id}/review/pr"),
            json!({ "dry_run": true }),
        )
        .await;
    assert!(
        plan["refusal"].as_str().unwrap().contains("not installed"),
        "{plan}"
    );
    git(
        &hub.repo(),
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            bare.to_str().unwrap(),
        ],
    );
    assert_eq!(plan["defaults"]["base"], "main");

    // The base moved on: merge refused until rebased.
    std::fs::write(hub.repo().join("main.txt"), "m\n").unwrap();
    git(&hub.repo(), &["add", "main.txt"]);
    git(&hub.repo(), &["commit", "-q", "-m", "main moves"]);
    let (_, plan) = hub
        .post(
            &format!("/api/sessions/{agent_id}/review/merge"),
            json!({ "dry_run": true }),
        )
        .await;
    assert_eq!(plan["needs_rebase"], true, "{plan}");
    let (status, refused) = hub
        .post(
            &format!("/api/sessions/{agent_id}/review/merge"),
            json!({ "confirm": plan["fingerprint"] }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");

    // `fw hub merge --rebase --yes --remove-worktree` rebases, merges,
    // and removes the worktree (and the companion) with its merged branch.
    let state = hub.state_dir();
    let (code, out, err) = run_cli(&["review", "feat", "--state-dir", &state]).await;
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("feat vs main (worktree): 2 ahead, 1 behind"),
        "{out}"
    );
    assert!(out.contains("docs: readme and notes"), "{out}");
    let (code, out, err) =
        run_cli(&["review", "feat", "--diff", "README", "--state-dir", &state]).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("+TWO"), "{out}");
    // Without --yes and no terminal: the plan only.
    let (code, out, err) = run_cli(&["merge", "feat", "--state-dir", &state]).await;
    assert_eq!(code, 1, "{out}");
    assert!(err.contains("--rebase"), "{err}");
    let (code, out, err) = run_cli(&[
        "merge",
        "feat",
        "--rebase",
        "--yes",
        "--remove-worktree",
        "--state-dir",
        &state,
    ])
    .await;
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        out.contains(&format!("$ git -C {} rebase main", wt.display())),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "$ git -C {} merge --ff-only {}",
            hub.repo().display(),
            git(&hub.repo(), &["rev-parse", "main"])
        )),
        "{out}"
    );
    assert!(out.contains("deleted (merged)"), "{out}");
    assert_eq!(
        git(&hub.repo(), &["log", "-1", "--format=%s", "main"]),
        "docs: readme and notes"
    );
    assert!(!wt.exists());
    let (_, sessions) = hub.call(reqwest::Method::GET, "/api/sessions", None).await;
    assert!(
        sessions["sessions"].as_array().unwrap().is_empty(),
        "{sessions}"
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_is_refused_while_the_main_checkout_is_dirty_or_off_base() {
    let hub = TestHub::start(Tools::default()).await;
    let (_, ws) = hub
        .post(
            "/api/workspaces",
            json!({ "name": "w", "folders": [hub.repo()] }),
        )
        .await;
    let ws = ws["id"].as_str().unwrap().to_owned();
    let (_, agent) = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": ws, "kind": "claude", "worktree": { "task": "x" } }),
        )
        .await;
    let id = agent["id"].as_str().unwrap().to_owned();
    let wt = PathBuf::from(agent["worktree"]["path"].as_str().unwrap());
    std::fs::write(wt.join("a"), "a\n").unwrap();
    git(&wt, &["add", "a"]);
    git(&wt, &["commit", "-q", "-m", "a"]);

    std::fs::write(hub.repo().join("README"), "dirty\n").unwrap();
    let (_, plan) = hub
        .post(
            &format!("/api/sessions/{id}/review/merge"),
            json!({ "dry_run": true }),
        )
        .await;
    assert!(
        plan["refusal"]
            .as_str()
            .unwrap()
            .contains("uncommitted change"),
        "{plan}"
    );
    git(&hub.repo(), &["checkout", "--", "README"]);
    git(&hub.repo(), &["checkout", "-q", "-b", "other"]);
    let (_, plan) = hub
        .post(
            &format!("/api/sessions/{id}/review/merge"),
            json!({ "dry_run": true }),
        )
        .await;
    assert!(
        plan["refusal"]
            .as_str()
            .unwrap()
            .contains("is on other, not main"),
        "{plan}"
    );
    let state = hub.state_dir();
    let (code, _, err) = run_cli(&["merge", "x", "--yes", "--state-dir", &state]).await;
    assert_eq!(code, 1);
    assert!(err.contains("is on other, not main"), "{err}");
    assert_eq!(
        git(&hub.repo(), &["rev-parse", "main"]),
        git(&hub.repo(), &["rev-parse", "other"])
    );

    // A session in the main checkout (no worktree) reviews against its
    // upstream; merging is not offered there.
    git(&hub.repo(), &["checkout", "-q", "main"]);
    let (_, shell) = hub
        .post("/api/sessions", json!({ "workspace_id": ws }))
        .await;
    let shell = shell["id"].as_str().unwrap().to_owned();
    let review = hub.get(&format!("/api/sessions/{shell}/review")).await;
    assert_eq!(review["mergeable"], false);
    assert_eq!(review["base"]["name"], "origin/main");
    assert_eq!(review["base"]["source"], "upstream");
    let (_, plan) = hub
        .post(
            &format!("/api/sessions/{shell}/review/merge"),
            json!({ "dry_run": true }),
        )
        .await;
    assert!(
        plan["refusal"]
            .as_str()
            .unwrap()
            .contains("worktree sessions"),
        "{plan}"
    );
    // A named base overrides; a flag-looking one is refused.
    let review = hub
        .get(&format!("/api/sessions/{shell}/review?base=main"))
        .await;
    assert_eq!(review["base"]["source"], "given");
    let (status, _) = hub
        .call(
            reqwest::Method::GET,
            &format!("/api/sessions/{shell}/review?base=--all"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    hub.shutdown().await;
}
