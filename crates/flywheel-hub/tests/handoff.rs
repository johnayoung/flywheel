//! Cross-agent handoff and review pairing end to end: real hubs on
//! 127.0.0.1:0 over a real git repo, with stand-in `claude` and `codex`
//! CLIs in real PTYs that record their argv, working directory, and the
//! exact bytes typed into them (raw mode, bracketed paste on, as the real
//! TUIs run). A handoff's provenance header, fence, and truncation notice
//! arrive byte for byte; the reviewer launches as the other kind,
//! read-only, in the author's worktree, and its findings go back; the
//! pairing survives a restart and shows in `fw hub ls`; and a handoff
//! crosses machines through the proxy.

mod support;

use flywheel_hub::agent::claude_project_dir_name;
use flywheel_hub::remote::{RuntimeInfo, ServeLock};
use flywheel_hub::{Hub, HubEvent, HubOptions, ServerOptions, serve};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};
use tokio::time::{Instant, timeout};

const WAIT: Duration = Duration::from_secs(60);

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

/// A stand-in agent CLI `name` writing into `log`: per hub session, its
/// working directory (`<name>-<session>.cwd`), argv (NUL-separated,
/// `.argv`), and every byte typed into it (`.in`), in raw mode with
/// bracketed paste enabled.
fn fake_agent(bin: &Path, log: &Path, name: &str) -> PathBuf {
    let path = bin.join(name);
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\nn='{log}/{name}-'\"$FLYWHEEL_HUB_SESSION\"\npwd > \"$n.cwd\"\n\
             for a in \"$@\"; do printf '%s\\000' \"$a\"; done > \"$n.argv\"\n\
             stty raw -echo\nprintf '\\033[?2004h'\nexec cat > \"$n.in\"\n",
            log = log.display()
        ),
    );
    path
}

struct TestHub {
    dir: Option<tempfile::TempDir>,
    root: PathBuf,
    addr: SocketAddr,
    hub: Arc<Hub>,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    /// A hub over `<tmp>/code/app` (main, one commit); `codex` installed
    /// only when asked.
    async fn start(codex: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("code/app");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("log")).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README"), "one\ntwo\n").unwrap();
        git(&repo, &["add", "README"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        Self::open(dir, root, codex).await
    }

    async fn open(dir: tempfile::TempDir, root: PathBuf, codex: bool) -> Self {
        flywheel_hub::install_crypto_provider();
        let mut options = HubOptions::new(root.join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        let log = root.join("log");
        options.agents.claude = fake_agent(&root.join("bin"), &log, "claude")
            .to_string_lossy()
            .into_owned();
        options.agents.codex = if codex {
            fake_agent(&root.join("bin"), &log, "codex")
                .to_string_lossy()
                .into_owned()
        } else {
            root.join("bin/no-codex").to_string_lossy().into_owned()
        };
        options.claude_config_dir = Some(root.join("claude-config"));
        options.codex_home = Some(root.join("codex-home"));
        let hub = support::open_hub(options);
        hub.restore_sessions();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            Arc::clone(&hub),
            ServerOptions::default(),
            async {
                let _ = stopped.await;
            },
        ));
        Self {
            dir: Some(dir),
            root,
            addr,
            hub,
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

    /// Record this server as the state dir's running hub, for the CLI.
    fn as_running(&self) {
        assert!(
            ServeLock::acquire(&self.root.join("state"))
                .unwrap()
                .is_none()
        );
        RuntimeInfo {
            pid: std::process::id(),
            listen: self.addr,
            public_urls: Vec::new(),
            started_at: None,
            log_file: None,
            tls: None,
        }
        .write(&self.root.join("state"))
        .unwrap();
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
            .bearer_auth(self.hub.token().expose());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let (status, value) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert!(status.is_success(), "{path}: {status} {value}");
        value
    }

    async fn refused(&self, path: &str, body: Value) -> (StatusCode, String) {
        let (status, value) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert!(!status.is_success(), "{path} was not refused: {value}");
        (
            status,
            value["error"].as_str().unwrap_or_default().to_owned(),
        )
    }

    async fn get(&self, path: &str) -> Value {
        let (status, value) = self.call(reqwest::Method::GET, path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {value}");
        value
    }

    async fn session(&self, id: &str) -> Value {
        let list = self.get("/api/sessions").await;
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    }

    async fn workspace(&self) -> String {
        let ws = self
            .post(
                "/api/workspaces",
                json!({ "name": "app", "folders": [self.repo()] }),
            )
            .await;
        ws["id"].as_str().unwrap().to_owned()
    }

    async fn spawn(&self, body: Value) -> (String, String) {
        let session = self.post("/api/sessions", body).await;
        (
            session["id"].as_str().unwrap().to_owned(),
            session["name"].as_str().unwrap().to_owned(),
        )
    }

    /// Wait until agent `name`'s session `sid` started and enabled
    /// bracketed paste; its argv and working directory.
    async fn started(&self, name: &str, sid: &str) -> (Vec<String>, PathBuf) {
        let base = self.root.join(format!("log/{name}-{sid}"));
        let input = base.with_extension("in");
        let deadline = Instant::now() + WAIT;
        while !input.exists() {
            assert!(Instant::now() < deadline, "{name} {sid} never started");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        loop {
            let screen = self.get(&format!("/api/sessions/{sid}/screen")).await;
            if screen["modes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m == "bracketed_paste")
            {
                break;
            }
            assert!(Instant::now() < deadline, "bracketed paste never enabled");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let argv = std::fs::read(base.with_extension("argv")).unwrap();
        let argv = String::from_utf8(argv)
            .unwrap()
            .split('\0')
            .filter(|a| !a.is_empty())
            .map(str::to_owned)
            .collect();
        let cwd = std::fs::read_to_string(base.with_extension("cwd")).unwrap();
        (argv, PathBuf::from(cwd.trim()))
    }

    async fn shutdown(mut self) -> (tempfile::TempDir, PathBuf) {
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(15), self.server.take().unwrap())
            .await
            .expect("server shut down in time")
            .unwrap()
            .unwrap();
        self.hub.shutdown(Duration::from_secs(5));
        (self.dir.take().unwrap(), self.root.clone())
    }
}

/// What a paste of `text` then Enter types into a bracketed-paste program.
fn pasted(text: &str) -> String {
    format!(
        "\x1b[200~{}\x1b[201~\r",
        text.trim_end_matches('\n').replace('\n', "\r")
    )
}

/// Wait until `name`'s session `sid` was typed exactly `want` (so far).
async fn typed(root: &Path, name: &str, sid: &str, want: &str) {
    let path = root.join(format!("log/{name}-{sid}.in"));
    let deadline = Instant::now() + WAIT;
    loop {
        let bytes = std::fs::read(&path).unwrap_or_default();
        if bytes.len() >= want.len() {
            assert_eq!(String::from_utf8_lossy(&bytes), want);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{name} {sid}: got {:?}, want {want:?}",
            String::from_utf8_lossy(&bytes)
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// The next lifecycle event `name`'s payload.
async fn lifecycle(events: &mut broadcast::Receiver<HubEvent>, name: &str) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, events.recv()).await {
            Ok(Ok(HubEvent::Lifecycle { event, payload })) if event == name => return payload,
            Ok(Ok(_) | Err(broadcast::error::RecvError::Lagged(_))) => {}
            other => panic!("no {name} event: {other:?}"),
        }
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

/// Commit `files` in `dir`.
fn commit(dir: &Path, files: &[(&str, &str)], message: &str) {
    for (path, text) in files {
        std::fs::write(dir.join(path), text).unwrap();
        git(dir, &["add", path]);
    }
    git(dir, &["commit", "-q", "-m", message]);
}

fn append(path: &Path, line: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{line}").unwrap();
}

/// Claude Code's transcript of hub session `session` (its conversation
/// id) running in `cwd`.
fn claude_transcript(root: &Path, session: &Value) -> PathBuf {
    let cwd = PathBuf::from(session["cwd"].as_str().unwrap());
    root.join("claude-config/projects")
        .join(claude_project_dir_name(&cwd))
        .join(format!(
            "{}.jsonl",
            session["agent_session_id"].as_str().unwrap()
        ))
}

fn claude_line(uuid: &str, role: &str, text: &str) -> Value {
    json!({
        "type": role,
        "uuid": uuid,
        "sessionId": "x",
        "timestamp": "2026-11-03T10:00:00Z",
        "message": { "role": role, "content": [{ "type": "text", "text": text }] },
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoffs_reach_another_agent_exactly_under_their_provenance() {
    let hub = TestHub::start(true).await;
    let mut events = hub.hub.subscribe();
    let wid = hub.workspace().await;
    let (claude, claude_name) = hub
        .spawn(json!({ "workspace_id": wid, "kind": "claude", "worktree": { "task": "feat" } }))
        .await;
    let (codex, _) = hub
        .spawn(json!({ "workspace_id": wid, "kind": "codex" }))
        .await;
    let (shell, _) = hub
        .spawn(json!({ "workspace_id": wid, "command": ["/bin/sh", "-c", "exec cat"] }))
        .await;
    let (_, worktree) = hub.started("claude", &claude).await;
    hub.started("codex", &codex).await;
    assert_eq!(worktree, hub.root.join("code/app.worktrees/feat"));

    // A selection, fenced past the backticks in it, under its header.
    let selection = "cargo test\n  two  spaces ```inline```\n";
    let composed = hub
        .post(
            &format!("/api/sessions/{claude}/handoff/compose"),
            json!({ "source": { "kind": "selection", "text": selection } }),
        )
        .await;
    let want = format!(
        "From {claude_name} on feat: selected terminal text\n\n````text\ncargo test\n  two  spaces ```inline```\n````\n"
    );
    assert_eq!(composed["prompt"], want);
    assert_eq!(composed["truncated"], false);
    assert_eq!(composed["from"]["branch"], "feat");
    // Edited before sending: what arrives is the edit, exactly.
    let edited = format!("{want}\nPlease run it.");
    let delivered = hub
        .post(
            &format!("/api/sessions/{codex}/handoff"),
            json!({
                "prompt": edited,
                "from": composed["from"],
                "source": composed["source"],
                "truncated": false,
            }),
        )
        .await;
    assert_eq!(delivered["to"]["id"], codex.as_str());
    assert_eq!(delivered["paste"]["bracketed"], true);
    let mut sent = pasted(&edited);
    typed(&hub.root, "codex", &codex, &sent).await;
    let event = lifecycle(&mut events, "handoff.sent").await;
    assert_eq!(event["from"]["session_id"], claude.as_str());
    assert_eq!(event["to"]["id"], codex.as_str());
    assert_eq!(event["source"], "selection");
    assert_eq!(event["bytes"], edited.len());

    // The branch diff, cut to the cap with a notice.
    let big: String = (0..400).map(|i| format!("line {i}\n")).collect();
    commit(&worktree, &[("big.txt", &big)], "big");
    let diff = hub
        .post(
            &format!("/api/sessions/{claude}/handoff/compose"),
            json!({ "source": { "kind": "diff" }, "max_bytes": 512 }),
        )
        .await;
    let prompt = diff["prompt"].as_str().unwrap();
    assert!(
        prompt.starts_with(&format!(
            "From {claude_name} on feat: its branch's diff vs main (git diff main...HEAD)\n\n```diff\ndiff --git a/big.txt b/big.txt\n"
        )),
        "{prompt}"
    );
    assert_eq!(diff["truncated"], true);
    let kept = diff["kept_bytes"].as_u64().unwrap();
    assert!(kept <= 512 && kept > 300, "{diff}");
    assert!(
        prompt.ends_with(&format!(
            "```\n\n[truncated by fw hub: kept the first {kept} of {} bytes]\n",
            diff["content_bytes"]
        )),
        "{prompt}"
    );
    assert!(!prompt.contains("line 399"));
    // One file of it.
    commit(&worktree, &[("small.txt", "x\n")], "small");
    let one = hub
        .post(
            &format!("/api/sessions/{claude}/handoff/compose"),
            json!({ "source": { "kind": "diff", "path": "small.txt" } }),
        )
        .await;
    let one = one["prompt"].as_str().unwrap();
    assert!(one.contains("+x\n") && !one.contains("big.txt"), "{one}");

    // The last suggested command, through the CLI.
    hub.as_running();
    let transcript = claude_transcript(&hub.root, &hub.session(&claude).await);
    // Asked before the agent has written anything: nothing yet (and the
    // hub has now looked for the transcript and missed it).
    let (status, why) = hub
        .refused(
            &format!("/api/sessions/{claude}/handoff/compose"),
            json!({ "source": { "kind": "last_command" } }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        why,
        format!("{claude_name} has suggested no shell command yet")
    );
    let (status, why) = hub
        .refused(
            &format!("/api/sessions/{claude}/handoff/compose"),
            json!({ "source": { "kind": "transcript", "index": 0 } }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        why,
        format!("message 0 of {claude_name}'s transcript not found")
    );
    // The agent's first reply creates the transcript; a handoff right
    // after it reads it then, not at the hub's next poll.
    assert!(!transcript.exists());
    append(
        &transcript,
        &claude_line("u1", "assistant", "Run:\n```bash\nmake check\n```"),
    );
    let (code, out, err) = run_cli(&[
        "handoff",
        &claude,
        &codex,
        "--last-command",
        "--state-dir",
        &hub.state_dir(),
    ])
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("the last command it suggested"), "{out}");
    let header = format!("From {claude_name} on feat: the last command it suggested");
    sent.push_str(&pasted(&format!("{header}\n\n```bash\nmake check\n```\n")));
    typed(&hub.root, "codex", &codex, &sent).await;
    // A transcript message by index (the search hit's / viewer's).
    let (code, out, err) = run_cli(&[
        "handoff",
        &claude,
        &codex,
        "--message",
        "0",
        "--dry-run",
        "--state-dir",
        &hub.state_dir(),
    ])
    .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        format!(
            "From {claude_name} on feat: a reply from its transcript\n\n````text\nRun:\n```bash\nmake check\n```\n````\n"
        )
    );
    // A message that is not there names the session, not its id.
    let (status, why) = hub
        .refused(
            &format!("/api/sessions/{claude}/handoff/compose"),
            json!({ "source": { "kind": "transcript", "index": 999 } }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        why,
        format!("message 999 of {claude_name}'s transcript not found")
    );
    // A suggested command goes in its copyable form: no `$ ` prompt.
    append(
        &transcript,
        &claude_line("u2", "assistant", "Then:\n```sh\n$ cargo test\n```"),
    );
    // Read at once: the compose catches the transcript up itself.
    let last = hub
        .post(
            &format!("/api/sessions/{claude}/handoff/compose"),
            json!({ "source": { "kind": "last_command" } }),
        )
        .await;
    assert_eq!(
        last["prompt"],
        format!("{header}\n\n```sh\ncargo test\n```\n")
    );
    // A selection from a file, the CLI's way.
    let file = hub.root.join("sel.txt");
    std::fs::write(&file, "picked\n").unwrap();
    let (code, _, err) = run_cli(&[
        "handoff",
        &claude,
        &codex,
        "--selection-file",
        file.to_str().unwrap(),
        "--state-dir",
        &hub.state_dir(),
    ])
    .await;
    assert_eq!(code, 0, "{err}");
    sent.push_str(&pasted(&format!(
        "From {claude_name} on feat: selected terminal text\n\n```text\npicked\n```\n"
    )));
    typed(&hub.root, "codex", &codex, &sent).await;

    // Refusals: a shell target, the source itself, empty content, a cap
    // out of bounds, nothing suggested.
    let (status, why) = hub
        .refused(
            &format!("/api/sessions/{shell}/handoff"),
            json!({ "prompt": "x" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(why.contains("not an agent session"), "{why}");
    let (_, why) = hub
        .refused(
            &format!("/api/sessions/{claude}/handoff"),
            json!({ "prompt": "x", "from": composed["from"] }),
        )
        .await;
    assert!(why.contains("the session it comes from"), "{why}");
    let (_, why) = hub
        .refused(
            &format!("/api/sessions/{claude}/handoff/compose"),
            json!({ "source": { "kind": "selection", "text": "  \n" } }),
        )
        .await;
    assert!(why.contains("empty"), "{why}");
    let (_, why) = hub
        .refused(
            &format!("/api/sessions/{claude}/handoff/compose"),
            json!({ "source": { "kind": "selection", "text": "x" }, "max_bytes": 10 }),
        )
        .await;
    assert!(why.contains("max_bytes"), "{why}");
    let (status, _) = hub
        .refused(
            &format!("/api/sessions/{codex}/handoff/compose"),
            json!({ "source": { "kind": "last_command" } }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (code, _, err) = run_cli(&[
        "handoff",
        &claude,
        &claude,
        "--last-command",
        "--state-dir",
        &hub.state_dir(),
    ])
    .await;
    assert_eq!(code, 1);
    assert!(err.contains("the session it comes from"), "{err}");
    // Nothing else reached the target.
    tokio::time::sleep(Duration::from_millis(200)).await;
    typed(&hub.root, "codex", &codex, &sent).await;

    // Claude's work gets a Codex reviewer: no approvals, read-only
    // sandbox, in the worktree, the diff in its prompt.
    let launched = hub
        .post(
            &format!("/api/sessions/{claude}/review/reviewer"),
            json!({}),
        )
        .await;
    assert_eq!(launched["kind"], "codex");
    let reviewer = launched["reviewer"]["id"].as_str().unwrap().to_owned();
    let (argv, cwd) = hub.started("codex", &reviewer).await;
    assert_eq!(cwd, worktree);
    let flags = ["-a", "never", "-s", "read-only"];
    assert!(argv.windows(4).any(|w| w == flags), "{argv:?}");
    assert!(
        !argv
            .iter()
            .any(|a| a.contains("danger") || a == "--full-auto")
    );
    assert_eq!(argv[argv.len() - 2], "--");
    let prompt = argv.last().unwrap();
    assert!(prompt.contains("+x"), "{prompt}");
    assert!(prompt.contains("diff --git a/big.txt"), "{prompt}");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_goes_to_the_other_kind_read_only_and_its_findings_come_back() {
    let hub = TestHub::start(true).await;
    let mut events = hub.hub.subscribe();
    let wid = hub.workspace().await;
    // Codex work in a worktree: the reviewer is Claude.
    let (author, author_name) = hub
        .spawn(json!({ "workspace_id": wid, "kind": "codex", "worktree": { "task": "fix" } }))
        .await;
    let (_, worktree) = hub.started("codex", &author).await;
    commit(
        &worktree,
        &[("lib.rs", "fn answer() -> u32 { 41 }\n")],
        "answer",
    );

    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{author}/review/findings/send"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "no reviewer yet");
    let dry = hub
        .post(
            &format!("/api/sessions/{author}/review/reviewer"),
            json!({ "dry_run": true }),
        )
        .await;
    assert_eq!(dry["kind"], "claude");
    assert_eq!(dry["flags"], json!(["--permission-mode", "plan"]));
    assert!(dry.get("reviewer").is_none());
    let launched = hub
        .post(
            &format!("/api/sessions/{author}/review/reviewer"),
            json!({}),
        )
        .await;
    assert_eq!(launched["reused"], false);
    assert_eq!(launched["base"], "main");
    assert_eq!(launched["branch"], "fix");
    let reviewer = launched["reviewer"]["id"].as_str().unwrap().to_owned();
    let reviewer_name = launched["reviewer"]["name"].as_str().unwrap().to_owned();
    assert_eq!(reviewer_name, format!("{author_name} review"));
    let (argv, cwd) = hub.started("claude", &reviewer).await;
    assert_eq!(cwd, worktree, "the reviewer runs in the author's worktree");
    let mode = argv.iter().position(|a| a == "--permission-mode").unwrap();
    assert_eq!(argv[mode + 1], "plan");
    assert!(
        !argv
            .iter()
            .any(|a| a.contains("bypass") || a.contains("dangerously"))
    );
    let dash = argv.iter().position(|a| a == "--").unwrap();
    let prompt = &argv[dash + 1];
    assert_eq!(dash + 2, argv.len(), "the prompt goes last");
    assert_eq!(prompt, launched["prompt"].as_str().unwrap());
    assert!(prompt.contains("`git diff main...HEAD`"), "{prompt}");
    assert!(prompt.contains("read-only review"), "{prompt}");
    assert!(prompt.contains("+fn answer() -> u32 { 41 }"), "{prompt}");
    let event = lifecycle(&mut events, "review.requested").await;
    assert_eq!(event["author"]["id"], author.as_str());
    assert_eq!(event["reviewer"]["id"], reviewer.as_str());
    assert_eq!(event["reviewer"]["review_of"], author.as_str());
    assert_eq!(event["reused"], false);

    // Paired on both records.
    let a = hub.session(&author).await;
    let r = hub.session(&reviewer).await;
    assert_eq!(a["reviewer"], reviewer.as_str());
    assert_eq!(r["review_of"], author.as_str());
    assert_eq!(r["launch"]["permission_mode"], "plan");

    // Its answer, read back from its transcript.
    let findings = hub
        .get(&format!("/api/sessions/{author}/review/findings"))
        .await;
    assert!(findings.get("reply").is_none(), "{findings}");
    let transcript = claude_transcript(&hub.root, &r);
    append(&transcript, &claude_line("p1", "user", prompt));
    let reply =
        "1. lib.rs:1 returns 41, not 42.\n\n```sh\ncargo test answer\n```\n\nVerdict: fix it.";
    append(&transcript, &claude_line("r1", "assistant", reply));
    // (A transcript looked for and not found is looked for again within
    // seconds.)
    let deadline = Instant::now() + WAIT;
    let findings = loop {
        let findings = hub
            .get(&format!("/api/sessions/{reviewer}/review/findings"))
            .await;
        if findings.get("reply").is_some() {
            break findings;
        }
        assert!(Instant::now() < deadline, "no reply read: {findings}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(findings["reply"], reply);
    assert_eq!(findings["author"]["id"], author.as_str());
    assert_eq!(findings["blocks"][0]["text"], "cargo test answer");
    assert_eq!(findings["blocks"][0]["shell"], true);
    let header = format!(
        "From {reviewer_name} on fix: review findings (claude reviewed {author_name}'s work, read-only)"
    );
    let want = format!(
        "{header}\n\n````markdown\n{reply}\n````\n\nAddress these findings: fix what is right, and say briefly why for any you disagree with.\n"
    );
    assert_eq!(findings["prompt"], want);
    let (code, out, err) = {
        hub.as_running();
        run_cli(&[
            "ask-review",
            &author,
            "--send-findings",
            "--state-dir",
            &hub.state_dir(),
        ])
        .await
    };
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!("sent the findings to {author_name}")),
        "{out}"
    );
    typed(&hub.root, "codex", &author, &pasted(&want)).await;
    let event = lifecycle(&mut events, "handoff.sent").await;
    assert_eq!(event["source"], "review_findings");
    assert_eq!(event["from"]["session_id"], reviewer.as_str());

    // Asking again reuses the running reviewer: the new prompt is pasted.
    let again = hub
        .post(
            &format!("/api/sessions/{author}/review/reviewer"),
            json!({}),
        )
        .await;
    assert_eq!(again["reused"], true);
    assert_eq!(again["reviewer"]["id"], reviewer.as_str());
    typed(
        &hub.root,
        "claude",
        &reviewer,
        &pasted(again["prompt"].as_str().unwrap()),
    )
    .await;

    // `fw hub ls` shows the pair.
    let (code, out, err) = run_cli(&["ls", "--state-dir", &hub.state_dir()]).await;
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!("(reviewed by {reviewer_name})")),
        "{out}"
    );
    assert!(out.contains(&format!("(reviews {author_name})")), "{out}");
    // A reviewer's own worktree removal is the author's to make.
    let (status, body) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{reviewer}?remove_worktree=true"),
            None,
        )
        .await;
    assert!(!status.is_success(), "{body}");

    // The pairing survives a restart.
    let (dir, root) = hub.shutdown().await;
    let hub = TestHub::open(dir, root, true).await;
    let a = hub.session(&author).await;
    let r = hub.session(&reviewer).await;
    assert_eq!(a["reviewer"], reviewer.as_str());
    assert_eq!(r["review_of"], author.as_str());
    let findings = hub
        .get(&format!("/api/sessions/{author}/review/findings"))
        .await;
    assert_eq!(findings["reply"], reply);
    // Closing the author takes its reviewer with it.
    let (status, body) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{author}"),
            None,
        )
        .await;
    assert!(status.is_success(), "{body}");
    assert!(hub.session(&reviewer).await.is_null());
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_codex_a_claude_author_gets_a_claude_reviewer() {
    let hub = TestHub::start(false).await;
    let wid = hub.workspace().await;
    // A cheap model on purpose: its reviewer (the same kind) keeps it
    // instead of the user's default model.
    let (author, _) = hub
        .spawn(json!({
            "workspace_id": wid, "kind": "claude", "model": "haiku",
            "worktree": { "task": "solo" },
        }))
        .await;
    let (_, worktree) = hub.started("claude", &author).await;
    let (status, why) = hub
        .refused(
            &format!("/api/sessions/{author}/review/reviewer"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(why.contains("nothing to review"), "{why}");
    commit(&worktree, &[("a.txt", "a\n")], "a");
    let (status, why) = hub
        .refused(
            &format!("/api/sessions/{author}/review/reviewer"),
            json!({ "kind": "codex" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(why.contains("codex is not installed"), "{why}");
    let named = hub
        .post(
            &format!("/api/sessions/{author}/review/reviewer"),
            json!({ "dry_run": true, "model": "sonnet" }),
        )
        .await;
    assert_eq!(
        named["flags"],
        json!(["--model", "sonnet", "--permission-mode", "plan"]),
        "a named model wins"
    );
    hub.as_running();
    let (code, out, err) = run_cli(&["ask-review", &author, "--state-dir", &hub.state_dir()]).await;
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("codex is not installed here: the reviewer is claude"),
        "{err}"
    );
    assert!(out.contains("launched claude reviewer"), "{out}");
    assert!(
        out.contains("(--model haiku --permission-mode plan)"),
        "{out}"
    );
    let author_view = hub.session(&author).await;
    let reviewer = author_view["reviewer"].as_str().unwrap().to_owned();
    let (argv, cwd) = hub.started("claude", &reviewer).await;
    assert_eq!(cwd, worktree);
    assert!(
        argv.windows(2).any(|w| w == ["--permission-mode", "plan"]),
        "{argv:?}"
    );
    assert!(
        argv.windows(2).any(|w| w == ["--model", "haiku"]),
        "the author's model: {argv:?}"
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handoff_crosses_machines_through_the_proxy() {
    let b = TestHub::start(true).await;
    let a = TestHub::start(true).await;
    let mut b_events = b.hub.subscribe();
    let b_ws = b.workspace().await;
    let (target, target_name) = b
        .spawn(json!({ "workspace_id": b_ws, "kind": "codex" }))
        .await;
    b.started("codex", &target).await;
    let a_ws = a.workspace().await;
    let (source, source_name) = a
        .spawn(json!({ "workspace_id": a_ws, "kind": "claude" }))
        .await;
    a.started("claude", &source).await;

    // A registers B with a peer credential B minted.
    let issued = b.post("/api/peers", json!({ "name": "hub-a" })).await;
    let base = format!("http://{}", b.addr);
    a.post(
        "/api/machines",
        json!({ "name": "b", "url": base, "credential": issued["credential"] }),
    )
    .await;

    // Composed on A (naming its machine), delivered on B through A's proxy.
    let composed = a
        .post(
            &format!("/api/sessions/{source}/handoff/compose"),
            json!({
                "source": { "kind": "selection", "text": "from afar" },
                "cross_machine": true,
            }),
        )
        .await;
    let host = flywheel_hub::machines::host_name();
    let want = format!(
        "From {source_name} on main (machine {host}): selected terminal text\n\n```text\nfrom afar\n```\n"
    );
    assert_eq!(composed["prompt"], want);
    let delivered = a
        .post(
            &format!("/m/b/api/sessions/{target}/handoff"),
            json!({
                "prompt": composed["prompt"],
                "from": composed["from"],
                "source": composed["source"],
            }),
        )
        .await;
    assert_eq!(delivered["to"]["name"], target_name.as_str());
    let mut sent = pasted(&want);
    typed(&b.root, "codex", &target, &sent).await;
    let event = lifecycle(&mut b_events, "handoff.sent").await;
    assert_eq!(event["from"]["machine"], host.as_str());
    assert_eq!(event["from"]["session_id"], source.as_str());

    // The CLI: FROM on A, TO on machine b.
    a.as_running();
    let file = a.root.join("sel.txt");
    std::fs::write(&file, "by cli\n").unwrap();
    let (code, out, err) = run_cli(&[
        "handoff",
        &source_name,
        &target_name,
        "--selection-file",
        file.to_str().unwrap(),
        "--to-machine",
        "b",
        "--state-dir",
        &a.state_dir(),
    ])
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!("to {target_name}")) && out.contains("on b"),
        "{out}"
    );
    sent.push_str(&pasted(&format!(
        "From {source_name} on main (machine {host}): selected terminal text\n\n```text\nby cli\n```\n"
    )));
    typed(&b.root, "codex", &target, &sent).await;

    // And back: FROM on machine b (--machine), TO on this hub (`local`).
    std::fs::write(&file, "from b\n").unwrap();
    let (code, out, err) = run_cli(&[
        "handoff",
        &target_name,
        &source_name,
        "--machine",
        "b",
        "--to-machine",
        "local",
        "--selection-file",
        file.to_str().unwrap(),
        "--state-dir",
        &a.state_dir(),
    ])
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!("to {source_name}")) && out.contains("on this hub"),
        "{out}"
    );
    typed(
        &a.root,
        "claude",
        &source,
        &pasted(&format!(
            "From {target_name} on main (machine {host}): selected terminal text\n\n```text\nfrom b\n```\n"
        )),
    )
    .await;
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn asking_twice_at_once_starts_one_reviewer_and_a_new_kind_replaces_it() {
    let hub = TestHub::start(true).await;
    let wid = hub.workspace().await;
    let (author, author_name) = hub
        .spawn(json!({ "workspace_id": wid, "kind": "codex", "worktree": { "task": "dup" } }))
        .await;
    let (_, worktree) = hub.started("codex", &author).await;
    commit(&worktree, &[("d.txt", "d\n")], "d");

    // A double click: two requests at once, one reviewer.
    let path = format!("/api/sessions/{author}/review/reviewer");
    let (one, two) = tokio::join!(hub.post(&path, json!({})), hub.post(&path, json!({})));
    let first = one["reviewer"]["id"].as_str().unwrap().to_owned();
    assert_eq!(two["reviewer"]["id"], first.as_str(), "{one} {two}");
    let mut reused = [one["reused"].as_bool(), two["reused"].as_bool()];
    reused.sort();
    assert_eq!(reused, [Some(false), Some(true)]);
    hub.started("claude", &first).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let claudes = std::fs::read_dir(hub.root.join("log"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("claude-") && name.ends_with(".argv")
        })
        .count();
    assert_eq!(claudes, 1, "one claude reviewer was started");
    let list = hub.get("/api/sessions").await;
    let reviewers = |list: &Value| {
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["review_of"] == author.as_str())
            .map(|s| s["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(reviewers(&list), vec![first.clone()]);

    // Another kind replaces it: the first is closed, the pair is one-to-one.
    let codex = hub.post(&path, json!({ "kind": "codex" })).await;
    assert_eq!(codex["reused"], false);
    assert_eq!(codex["replaced"]["id"], first.as_str());
    let second = codex["reviewer"]["id"].as_str().unwrap().to_owned();
    let (argv, cwd) = hub.started("codex", &second).await;
    assert_eq!(cwd, worktree);
    assert!(
        argv.windows(4)
            .any(|w| w == ["-a", "never", "-s", "read-only"]),
        "{argv:?}"
    );
    assert!(
        hub.session(&first).await.is_null(),
        "the replaced reviewer is closed"
    );
    assert_eq!(hub.session(&author).await["reviewer"], second.as_str());
    assert_eq!(
        reviewers(&hub.get("/api/sessions").await),
        vec![second.clone()]
    );
    hub.as_running();
    let (code, out, err) = run_cli(&["ls", "--state-dir", &hub.state_dir()]).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(out.matches("(reviews ").count(), 1, "{out}");
    assert!(out.contains(&format!("(reviews {author_name})")), "{out}");

    // After a restart without a conversation to resume, Codex comes back
    // as a plain shell: a handoff to it says why and what to do.
    let (dir, root) = hub.shutdown().await;
    let hub = TestHub::open(dir, root, true).await;
    let a = hub.session(&author).await;
    assert!(!a["fallback_command"].is_null(), "{a}");
    assert_eq!(a["reviewer"], second.as_str());
    let (status, why) = hub
        .refused(
            &format!("/api/sessions/{author}/handoff"),
            json!({ "prompt": "x" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        why.contains(&format!(
            "{author_name} is a codex session, but codex is not running in it"
        )) && why.contains(&format!("fw hub relaunch {author_name}")),
        "{why}"
    );
    hub.shutdown().await;
}
