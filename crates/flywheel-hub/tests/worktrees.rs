//! Agent sessions in sibling git worktrees and companion shells, end to
//! end: real git repos in temp dirs, a real server on 127.0.0.1:0, real
//! PTYs.

mod support;

use flywheel_hub::hub::RestoreOutcome;
use flywheel_hub::remote::RuntimeInfo;
use flywheel_hub::{HubOptions, ServerOptions, serve};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};

struct TestHub {
    dir: tempfile::TempDir,
    addr: std::net::SocketAddr,
    token: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

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

/// A stand-in agent CLI that just reads its input.
fn fake_agent(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    support::write_script(&path, "#!/bin/sh\nexec cat\n");
    path
}

impl TestHub {
    /// A hub over `<tmp>/code/app` (a git repo with one commit on main)
    /// and `<tmp>/notes` (a plain folder).
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("code/app");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README"), "hi\n").unwrap();
        git(&repo, &["add", "README"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        git(&repo, &["branch", "dev"]);
        std::fs::create_dir_all(dir.path().join("notes")).unwrap();
        Self::reopen(dir).await.0
    }

    async fn reopen(dir: tempfile::TempDir) -> (Self, Vec<RestoreOutcome>) {
        flywheel_hub::install_crypto_provider();
        let mut options = HubOptions::new(dir.path().join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = fake_agent(dir.path()).to_string_lossy().into_owned();
        options.claude_config_dir = Some(dir.path().join("claude-config"));
        options.codex_home = Some(dir.path().join("codex-home"));
        let hub = support::open_hub(options);
        let restored = hub.restore_sessions();
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        RuntimeInfo {
            pid: std::process::id(),
            listen: addr,
            public_urls: Vec::new(),
            started_at: None,
            log_file: None,
        }
        .write(&dir.path().join("state"))
        .unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, hub, ServerOptions::default(), async {
            let _ = stopped.await;
        }));
        let hub = Self {
            dir,
            addr,
            token,
            http: reqwest::Client::new(),
            stop: Some(stop),
            server: Some(server),
        };
        (hub, restored)
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("code/app").canonicalize().unwrap()
    }

    fn worktrees(&self) -> PathBuf {
        self.dir
            .path()
            .join("code/app.worktrees")
            .canonicalize()
            .unwrap()
    }

    fn state_dir(&self) -> String {
        self.dir.path().join("state").to_string_lossy().into_owned()
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

    async fn workspace(&self) -> String {
        let (status, ws) = self
            .post(
                "/api/workspaces",
                json!({ "name": "w", "folders": [self.repo(), self.dir.path().join("notes")] }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{ws}");
        ws["id"].as_str().unwrap().to_owned()
    }

    async fn session(&self, id: &str) -> Value {
        self.get(&format!("/api/sessions/{id}")).await
    }

    async fn shutdown(mut self) -> tempfile::TempDir {
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(15), self.server.take().unwrap())
            .await
            .expect("server shut down in time")
            .unwrap()
            .unwrap();
        self.dir
    }
}

async fn eventually<F: Fn() -> bool>(what: &str, check: F) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(30)).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_in_a_new_worktree_gets_a_companion_shell_that_runs_commands_there() {
    let hub = TestHub::start().await;
    let ws = hub.workspace().await;

    // The form's live check: where it goes, the branches, the default base.
    let plan = hub
        .get(&format!(
            "/api/worktrees/plan?folder={}&task=fix-login",
            hub.repo().display()
        ))
        .await;
    assert_eq!(plan["default_base"], "main");
    assert_eq!(plan["plan"]["branch"], "fix-login");
    assert_eq!(plan["plan"]["base"], "main");
    let expected = hub
        .dir
        .path()
        .canonicalize()
        .unwrap()
        .join("code/app.worktrees/fix-login");
    assert_eq!(plan["plan"]["path"], json!(expected));
    let mut branches: Vec<&str> = plan["branches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    branches.sort_unstable();
    assert_eq!(branches, ["dev", "main"]);
    let plain = hub
        .get(&format!(
            "/api/worktrees/plan?folder={}&task=x",
            hub.dir.path().join("notes").display()
        ))
        .await;
    assert!(
        plain["error"]
            .as_str()
            .unwrap()
            .contains("not a git repository"),
        "{plain}"
    );

    // Plain folders cannot host worktrees; nothing is created.
    let (status, refused) = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": ws, "folder": hub.dir.path().join("notes"), "kind": "claude",
                    "worktree": { "task": "x" } }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(!hub.dir.path().join("notes.worktrees").exists());
    let (status, refused) = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": ws, "kind": "claude", "worktree": { "task": "../escape" } }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");

    // An agent in a new worktree, cut from `dev`.
    let (status, agent) = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": ws, "folder": hub.repo(), "kind": "claude",
                    "worktree": { "task": "fix-login", "base": "dev" } }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_owned();
    let wt = hub.worktrees().join("fix-login");
    assert_eq!(agent["name"], "fix-login");
    assert_eq!(agent["cwd"], json!(wt));
    assert_eq!(agent["worktree"]["repo"], json!(hub.repo()));
    assert_eq!(agent["worktree"]["branch"], "fix-login");
    assert_eq!(agent["worktree"]["base"], "dev");
    assert_eq!(
        git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "fix-login"
    );
    // Not nested: the repo does not see it as untracked content.
    assert_eq!(git(&hub.repo(), &["status", "--porcelain"]), "");

    // The badge arrives from the background reader.
    let deadline = Instant::now() + Duration::from_secs(10);
    let badge = loop {
        let view = hub.session(&agent_id).await;
        if view["git"].is_object() {
            break view["git"].clone();
        }
        assert!(Instant::now() < deadline, "no git badge: {view}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(badge["branch"], "fix-login");
    assert_eq!(badge["against"], "dev");
    assert_eq!(
        (badge["dirty"].as_u64(), badge["ahead"].as_u64()),
        (Some(0), Some(0))
    );

    // Only agents have companions.
    let (status, plain_shell) = hub
        .post("/api/sessions", json!({ "workspace_id": ws }))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, refused) = hub
        .post(
            &format!(
                "/api/sessions/{}/companion",
                plain_shell["id"].as_str().unwrap()
            ),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");

    // "Open shell & run": a companion shell starts in the worktree, its
    // prompt is waited for, and the command runs there.
    let (status, sent) = hub
        .post(
            &format!("/api/sessions/{agent_id}/companion"),
            json!({ "text": "pwd > where.txt", "run": true }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{sent}");
    assert_eq!(sent["created"], true);
    assert_ne!(sent["ready"], "timeout", "{sent}");
    assert_eq!(sent["paste"]["ran"], true);
    let companion = sent["session"].clone();
    let companion_id = companion["id"].as_str().unwrap().to_owned();
    assert_eq!(companion["kind"], "shell");
    assert_eq!(companion["companion_of"], json!(agent_id));
    assert_eq!(companion["cwd"], json!(wt));
    assert_eq!(companion["worktree"]["path"], json!(wt));
    assert_eq!(companion["name"], "fix-login shell");
    let out = wt.join("where.txt");
    eventually("the command to run in the worktree", || {
        std::fs::read_to_string(&out).is_ok_and(|t| t.trim() == wt.to_string_lossy())
    })
    .await;

    // Again: the same companion, already running; no wait.
    let (status, again) = hub
        .post(
            &format!("/api/sessions/{agent_id}/companion"),
            json!({ "text": "echo two >> where.txt", "run": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["created"], false);
    assert_eq!(again["ready"], "running");
    assert_eq!(again["session"]["id"], json!(companion_id));
    let (status, plain) = hub
        .post(&format!("/api/sessions/{agent_id}/companion"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(plain["id"], json!(companion_id));
    // Bad pastes start nothing.
    let (status, _) = hub
        .post(
            &format!("/api/sessions/{agent_id}/companion"),
            json!({ "text": "  " }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Listed right after its agent (the plain shell was created between).
    let list = hub.get(&format!("/api/sessions?workspace_id={ws}")).await;
    let order: Vec<&str> = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        [
            agent_id.as_str(),
            companion_id.as_str(),
            plain_shell["id"].as_str().unwrap()
        ]
    );

    // Closing with removal: the untracked file is work, so it is refused
    // (nothing closed) until forced.
    let info = hub.get(&format!("/api/sessions/{agent_id}/worktree")).await;
    assert_eq!(info["untracked"], 1, "{info}");
    assert_eq!(info["companions"], json!(["fix-login shell"]));
    assert!(
        info["refusal"]
            .as_str()
            .unwrap()
            .contains("1 untracked file"),
        "{info}"
    );
    let (status, refused) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{agent_id}?remove_worktree=true"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["error"].as_str().unwrap().contains("force"),
        "{refused}"
    );
    assert!(wt.is_dir());
    assert_eq!(hub.session(&companion_id).await["state"], "running");
    let (status, removed) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{agent_id}?remove_worktree=true&force=true"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["removed"], json!([agent_id, companion_id]));
    assert_eq!(removed["worktree"]["branch_deleted"], true);
    assert!(!wt.exists());
    let (status, _) = hub
        .call(
            reqwest::Method::GET,
            &format!("/api/sessions/{companion_id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(git(&hub.repo(), &["branch", "--list", "fix-login"]).is_empty());
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unmerged_worktrees_keep_their_branch_and_another_session_blocks_removal() {
    let hub = TestHub::start().await;
    let ws = hub.workspace().await;
    let (_, agent) = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": ws, "kind": "claude", "worktree": { "task": "feat" } }),
        )
        .await;
    let agent_id = agent["id"].as_str().unwrap().to_owned();
    let wt = hub.worktrees().join("feat");
    std::fs::write(wt.join("work.txt"), "x\n").unwrap();
    git(&wt, &["add", "work.txt"]);
    git(&wt, &["commit", "-q", "-m", "work"]);

    let info = hub.get(&format!("/api/sessions/{agent_id}/worktree")).await;
    assert_eq!(info["unmerged"], 1, "{info}");
    assert!(
        info["refusal"]
            .as_str()
            .unwrap()
            .contains("1 commit not in main")
    );

    // A session opened in the worktree as a workspace folder of its own
    // (not a companion) blocks removal, even forced.
    let (status, _) = hub
        .call(
            reqwest::Method::PATCH,
            &format!("/api/workspaces/{ws}"),
            Some(json!({ "folders": [hub.repo(), hub.dir.path().join("notes"), wt] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, other) = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": ws, "folder": wt, "name": "other" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{other}");
    let info = hub.get(&format!("/api/sessions/{agent_id}/worktree")).await;
    assert_eq!(info["blocking"], json!(["other"]));
    let (status, refused) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{agent_id}?remove_worktree=true&force=true"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("\"other\" also uses"),
        "{refused}"
    );
    assert_eq!(hub.session(&agent_id).await["state"], "running");
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{}", other["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, refused) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{agent_id}?remove_worktree=true"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("1 commit not in main")
    );

    // A malformed flag is refused, nothing removed.
    let (status, removed) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{agent_id}?remove_worktree=yes"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{removed}");

    // The CLI: refused without --force, then forced; the branch is kept.
    let short = &agent_id[..8];
    let state = hub.state_dir();
    let (code, _, err) = run_cli(&[
        "kill",
        short,
        "--rm",
        "--remove-worktree",
        "--state-dir",
        &state,
    ])
    .await;
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("1 commit not in main"), "{err}");
    let (code, out, err) = run_cli(&[
        "kill",
        short,
        "--rm",
        "--remove-worktree",
        "--force",
        "--state-dir",
        &state,
    ])
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("branch feat kept"), "{out}");
    assert!(!wt.exists());
    assert_eq!(git(&hub.repo(), &["branch", "--list", "feat"]), "feat");
    // --remove-worktree on a session that has none says so.
    let (_, shell) = hub
        .post("/api/sessions", json!({ "workspace_id": ws }))
        .await;
    let (code, _, err) = run_cli(&[
        "kill",
        shell["id"].as_str().unwrap(),
        "--rm",
        "--remove-worktree",
        "--state-dir",
        &state,
    ])
    .await;
    assert_eq!(code, 1);
    assert!(err.contains("does not run in a worktree"), "{err}");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_new_worktree_and_restore_keeps_sessions_in_their_worktree() {
    let hub = TestHub::start().await;
    hub.workspace().await;
    let state = hub.state_dir();
    let (code, out, err) = run_cli(&[
        "new",
        "w",
        "--kind",
        "shell",
        "--worktree",
        "spike",
        "--base",
        "dev",
        "--state-dir",
        &state,
    ])
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("app.worktrees/spike on branch spike (from dev)"),
        "{err}"
    );
    let id = out.trim().to_owned();
    let wt = hub.worktrees().join("spike");
    assert_eq!(hub.session(&id).await["cwd"], json!(wt));
    // A plain folder refuses --worktree.
    let (code, _, err) = run_cli(&[
        "new",
        "w",
        "--folder",
        "notes",
        "--worktree",
        "nope",
        "--state-dir",
        &state,
    ])
    .await;
    assert_eq!(code, 1);
    assert!(err.contains("not a git repository"), "{err}");

    // ls shows the branch badge once read.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, out, _) = run_cli(&["ls", "--state-dir", &state]).await;
        if out.contains("[spike +0 -0 vs dev worktree]") {
            break;
        }
        assert!(Instant::now() < deadline, "{out}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A restart brings it back in its worktree, still a worktree session.
    let dir = hub.shutdown().await;
    let (hub, restored) = TestHub::reopen(dir).await;
    let outcome = restored.iter().find(|o| o.id == id).expect("restored");
    assert!(outcome.result.is_ok(), "{:?}", outcome.result);
    assert_eq!(outcome.cwd, wt);
    let view = hub.session(&id).await;
    assert_eq!(view["state"], "running");
    assert_eq!(view["worktree"]["branch"], "spike");
    assert_eq!(view["live"]["cwd"], json!(wt));

    // Its worktree deleted while the hub was down: not restored, and why.
    let dir = hub.shutdown().await;
    git(
        &dir.path().join("code/app"),
        &["worktree", "remove", "--force", &wt.to_string_lossy()],
    );
    let (hub, restored) = TestHub::reopen(dir).await;
    let outcome = restored.iter().find(|o| o.id == id).expect("tried");
    let error = outcome.result.as_ref().unwrap_err();
    assert!(error.contains("no longer exists"), "{error}");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn companions_close_with_their_agent_and_never_remove_its_worktree() {
    let hub = TestHub::start().await;
    let ws = hub.workspace().await;
    let state = hub.state_dir();

    // A plain agent (no worktree) closed without --remove-worktree takes
    // its companion with it: none is left pointing at a gone session.
    let (status, agent) = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": ws, "kind": "claude", "name": "ag2" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_owned();
    let (status, companion) = hub
        .post(&format!("/api/sessions/{agent_id}/companion"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{companion}");
    let companion_id = companion["id"].as_str().unwrap().to_owned();
    let (code, out, _) = run_cli(&["ls", "--state-dir", &state]).await;
    assert_eq!(code, 0);
    assert!(out.contains("(companion of ag2)"), "{out}");
    let (code, _, err) = run_cli(&["kill", "ag2", "--rm", "--state-dir", &state]).await;
    assert_eq!(code, 0, "{err}");
    let sessions = hub.get("/api/sessions").await;
    let ids: Vec<&str> = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(!ids.contains(&companion_id.as_str()), "{sessions}");
    assert!(!ids.contains(&agent_id.as_str()), "{sessions}");

    // A worktree agent: its companion cannot remove the worktree, even
    // with the agent stopped.
    let (status, agent) = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": ws, "kind": "claude", "worktree": { "task": "wt-c" } }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    let agent_id = agent["id"].as_str().unwrap().to_owned();
    let wt = hub.worktrees().join("wt-c");
    let (_, companion) = hub
        .post(&format!("/api/sessions/{agent_id}/companion"), json!({}))
        .await;
    let companion_id = companion["id"].as_str().unwrap().to_owned();
    let (status, _) = hub
        .post(&format!("/api/sessions/{agent_id}/kill"), json!({}))
        .await;
    assert!(status.is_success(), "{status}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while hub.session(&agent_id).await["state"] == "running" {
        assert!(Instant::now() < deadline, "the agent did not stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (status, refused) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{companion_id}?remove_worktree=true&force=true"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("companion shell"),
        "{refused}"
    );
    assert!(wt.is_dir());
    assert_eq!(hub.session(&companion_id).await["id"], json!(companion_id));
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_create_with_a_refused_worktree_leaves_no_workspace() {
    let hub = TestHub::start().await;
    let state = hub.state_dir();
    let plain = hub.dir.path().join("notes");
    let sub = hub.repo().join("sub");
    std::fs::create_dir(&sub).unwrap();
    for (name, folder, why) in [
        ("plainws", &plain, "is not a git repository"),
        ("subws", &sub, "is inside the git repository"),
    ] {
        let folder = folder.to_string_lossy().into_owned();
        let (code, _, err) = run_cli(&[
            "new",
            name,
            "--folder",
            &folder,
            "--create",
            "--worktree",
            "x",
            "--state-dir",
            &state,
        ])
        .await;
        assert_eq!(code, 1, "{err}");
        assert!(err.contains(why), "{err}");
        assert!(!err.contains("created workspace"), "{err}");
    }
    let workspaces = hub.get("/api/workspaces").await;
    assert_eq!(workspaces["workspaces"], json!([]), "{workspaces}");
    hub.shutdown().await;
}
