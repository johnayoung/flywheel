//! Agent permission rules end to end: a real hub on 127.0.0.1:0, real
//! PTYs running stand-ins for Claude Code and Codex that draw their
//! permission dialogs (and record the key that answers them), a real git
//! repo, the HTTP API, and the CLI. Claude Code's hooks are played by
//! sending the reports `fw hub report` sends. Everything lives in a temp
//! dir: the user's ~/.claude and ~/.codex are never read or written.

mod support;

use flywheel_hub::report::{self, Report};
use flywheel_hub::{HubOptions, ServerOptions, serve};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};

const REPORT_PROGRAM: &str = "/opt/flywheel/bin/fw";

struct TestHub {
    dir: tempfile::TempDir,
    addr: SocketAddr,
    token: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A stand-in agent: dumps its environment, draws `<dir>/<name>.screen`,
/// then records the first key it is sent in `<dir>/<name>.answer`.
fn fake_agent(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let d = dir.join(name);
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\nenv > '{d}.env.tmp' && mv '{d}.env.tmp' '{d}.env'\n\
             cat '{d}.screen'\nstty raw -echo\n\
             key=$(dd bs=1 count=1 2>/dev/null)\nprintf '%s' \"$key\" > '{d}.answer'\nsleep 600\n",
            d = d.display()
        ),
    );
    path
}

const RULE: &str =
    "────────────────────────────────────────────────────────────────────────────────";
const DASH: &str =
    "╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌";

impl TestHub {
    async fn start() -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        // The user's global excludes must not decide what the test sees.
        git(&repo, &["config", "core.excludesFile", "/dev/null"]);
        let claude = fake_agent(dir.path(), "claude");
        let codex = fake_agent(dir.path(), "codex");
        std::fs::write(
            dir.path().join("claude.screen"),
            format!(
                "● Bash(cargo test -p flywheel-hub perms::x)\r\n{RULE}\r\n Bash command\r\n Run the tests\r\n{DASH}\r\n cargo test -p flywheel-hub perms::x\r\n{DASH}\r\n Do you want to proceed?\r\n ❯ 1. Yes\r\n   2. Yes, and don't ask again for: cargo test -p flywheel-hub *\r\n   3. No\r\n Esc to cancel · Tab to amend\r\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("codex.screen"),
            "• I'll run the test suite to reproduce it.\r\n  Would you like to run the following command?\r\n  Reason: run the parser tests\r\n  $ cargo test -p parser\r\n› 1. Yes, proceed (y)\r\n  2. Yes, and don't ask again for commands that start with `cargo test -p parser` (p)\r\n  3. No, and tell Codex what to do differently (esc)\r\n  Press enter to confirm or esc to cancel\r\n",
        )
        .unwrap();
        let mut options = HubOptions::new(dir.path().join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = claude.to_string_lossy().into_owned();
        options.agents.codex = codex.to_string_lossy().into_owned();
        options.report_command = Some(vec![REPORT_PROGRAM.into(), "hub".into()]);
        options.claude_config_dir = Some(dir.path().join("claude-config"));
        options.codex_home = Some(dir.path().join("codex-home"));
        let hub = support::open_hub(options);
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, hub, ServerOptions::default(), async {
            let _ = stopped.await;
        }));
        Self {
            dir,
            addr,
            token,
            http: reqwest::Client::new(),
            stop: Some(stop),
            server: Some(server),
        }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo").canonicalize().unwrap()
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

    async fn get(&self, path: &str) -> Value {
        let (status, body) = self.call(reqwest::Method::GET, path, None).await;
        assert!(status.is_success(), "{path}: {status} {body}");
        body
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let (status, answer) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert!(status.is_success(), "{path}: {status} {answer}");
        answer
    }

    /// The folder's suggestions, once there are `n` of them.
    async fn suggestions(&self, folder: &Path, n: usize) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let found = self
                .get(&format!("/api/perms/suggest?folder={}", folder.display()))
                .await;
            if found["suggestions"]
                .as_array()
                .is_some_and(|a| a.len() == n)
            {
                return found;
            }
            assert!(Instant::now() < deadline, "never {n} suggestions: {found}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// `body` with the base of its preview (as every client sends it).
    async fn based(&self, mut body: Value) -> Value {
        let (status, preview) = self
            .call(
                reqwest::Method::POST,
                "/api/perms/preview",
                Some(body.clone()),
            )
            .await;
        if status.is_success() {
            body["base"] = preview["base"].clone();
        }
        body
    }

    async fn agent(&self, kind: &str) -> (String, HashMap<String, String>) {
        let ws = self
            .post(
                "/api/workspaces",
                json!({ "name": format!("ws-{kind}"), "folders": [self.repo()] }),
            )
            .await;
        let session = self
            .post(
                "/api/sessions",
                json!({ "workspace_id": ws["id"], "kind": kind, "cols": 120, "rows": 30 }),
            )
            .await;
        let id = session["id"].as_str().unwrap().to_owned();
        let env_file = self.dir.path().join(format!("{kind}.env"));
        let deadline = Instant::now() + Duration::from_secs(60);
        let env = loop {
            if let Ok(text) = std::fs::read_to_string(&env_file)
                && !text.is_empty()
            {
                break text
                    .lines()
                    .filter_map(|l| l.split_once('='))
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect();
            }
            assert!(Instant::now() < deadline, "{kind} never launched");
            tokio::time::sleep(Duration::from_millis(30)).await;
        };
        (id, env)
    }

    async fn status_is(&self, sid: &str, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let sessions = self.get("/api/sessions").await;
            let status = sessions["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"] == sid)
                .map(|s| s["agent"]["status"].clone())
                .unwrap_or(Value::Null);
            if status == want {
                return;
            }
            assert!(Instant::now() < deadline, "{sid} never {want}: {status}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn screen_shows(&self, sid: &str, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let screen = self
                .get(&format!("/api/sessions/{sid}/screen?source=visible"))
                .await;
            if screen["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l.as_str().is_some_and(|l| l.contains(text)))
            {
                return;
            }
            assert!(Instant::now() < deadline, "{text:?} never drawn: {screen}");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    async fn answer(&self, kind: &str) -> String {
        let path = self.dir.path().join(format!("{kind}.answer"));
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(text) = std::fs::read_to_string(&path) {
                return text;
            }
            assert!(Instant::now() < deadline, "{kind} was never answered");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    async fn shutdown(mut self) {
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(15), self.server.take().unwrap())
            .await
            .expect("server shut down in time")
            .unwrap()
            .unwrap();
    }
}

fn hook(env: &HashMap<String, String>, event: &str, payload: Value) {
    report::send(
        Path::new(&env[report::SOCKET_ENV]),
        &Report {
            session: env[report::SESSION_ENV].clone(),
            secret: env[report::SECRET_ENV].clone(),
            source: "claude".into(),
            event: event.into(),
            detail: None,
            payload: Some(payload),
        },
    )
    .unwrap();
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

async fn run_cli(hub: &TestHub, args: &[&str]) -> (i32, String, String) {
    let mut args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    args.extend([
        "--url".to_owned(),
        format!("http://{}", hub.addr),
        "--token".to_owned(),
        hub.token.clone(),
    ]);
    tokio::task::spawn_blocking(move || cli(args))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn always_allow_from_a_blocked_claude_prompt() {
    let hub = TestHub::start().await;
    let repo = hub.repo();
    let (sid, env) = hub.agent("claude").await;
    hub.screen_shows(&sid, "Do you want to proceed?").await;
    let command = "cargo test -p flywheel-hub perms::x";
    hook(
        &env,
        "PreToolUse",
        json!({ "tool_name": "Bash", "tool_input": { "command": command } }),
    );
    hook(
        &env,
        "PermissionRequest",
        json!({ "tool_name": "Bash", "tool_input": { "command": command, "description": "Run the tests" } }),
    );
    hub.status_is(&sid, "blocked").await;

    // The view: every source in order, the pending request, its proposal.
    let view = hub.get(&format!("/api/perms?session={sid}")).await;
    let scopes: Vec<&str> = view["claude"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["scope"].as_str().unwrap())
        .collect();
    assert_eq!(
        scopes,
        ["managed", "command line", "local", "project", "user"]
    );
    let pending = &view["pending"];
    assert_eq!(pending["request"]["source"], "hook");
    assert_eq!(
        pending["claude"]["rules"],
        json!(["Bash(cargo test -p flywheel-hub *)"])
    );
    assert_eq!(pending["decision"]["outcome"], "none");
    let local = repo.join(".claude/settings.local.json");
    assert_eq!(view["claude"]["targets"][0]["path"], json!(local));
    assert!(
        view["claude"]["targets"][0]["warning"]
            .as_str()
            .unwrap()
            .contains("not git-ignored")
    );

    // Suggested from the journal before anything is allowed (journaled
    // off the report's thread).
    let suggested = hub.suggestions(&repo, 1).await;
    assert_eq!(
        suggested["suggestions"][0]["rules"][0]["rule"],
        "Bash(cargo test -p flywheel-hub *)"
    );
    assert_eq!(suggested["suggestions"][0]["count"], 1);

    // Preview: the file, the diff, the sentence. Nothing written yet.
    let body = json!({ "session": sid, "rules": ["Bash(cargo test -p flywheel-hub *)"] });
    let preview = hub.post("/api/perms/preview", body.clone()).await;
    assert_eq!(preview["path"], json!(local));
    assert!(
        preview["diff"]
            .as_str()
            .unwrap()
            .contains("+      \"Bash(cargo test -p flywheel-hub *)\""),
        "{preview}"
    );
    assert_eq!(
        preview["sentence"],
        format!(
            "This lets the agent run `cargo test -p flywheel-hub` (with any further arguments) without asking, in your Claude Code sessions in {} and its worktrees.",
            repo.display()
        )
    );
    assert!(!local.exists());
    // Unconfirmed: refused.
    let (status, refused) = hub
        .call(
            reqwest::Method::POST,
            "/api/perms/allow",
            Some(body.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(!local.exists());

    // Confirmed: written, then the prompt answered "1" (Yes).
    let mut confirmed = body.clone();
    confirmed["confirm"] = json!(true);
    confirmed["base"] = preview["base"].clone();
    confirmed["answer"] = json!(true);
    confirmed["request_key"] = pending["request"]["key"].clone();
    let done = hub.post("/api/perms/allow", confirmed).await;
    assert_eq!(done["written"], true);
    assert_eq!(done["answered"], true, "{done}");
    assert_eq!(hub.answer("claude").await, "1");
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&local).unwrap()).unwrap();
    assert_eq!(
        written,
        json!({ "permissions": { "allow": ["Bash(cargo test -p flywheel-hub *)"] } })
    );

    // Now already allowed: the suggestion is gone.
    let suggested = hub
        .get(&format!("/api/perms/suggest?folder={}", repo.display()))
        .await;
    assert_eq!(suggested["suggestions"], json!([]));

    // Logged to activity, without the rule's command text.
    let activity = hub.get("/api/activity?kind=perms&machines=none").await;
    let row = &activity["rows"][0];
    assert_eq!(row["kind"], "perms.allowed", "{activity}");
    assert_eq!(
        row["text"],
        "allowed 1 permission rule for claude (local settings)"
    );
    assert!(!activity.to_string().contains("cargo test"));

    // A stale preview is refused.
    let (status, stale) = hub
        .call(
            reqwest::Method::POST,
            "/api/perms/allow",
            Some(json!({ "session": sid, "rules": ["Bash(make *)"], "confirm": true, "base": preview["base"] })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");

    // Dangerous: written only after it is typed back.
    let danger = hub
        .based(json!({ "folder": repo, "rules": ["Bash(rm:*)"], "confirm": true }))
        .await;
    let (status, refused) = hub
        .call(
            reqwest::Method::POST,
            "/api/perms/allow",
            Some(danger.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        refused["error"].as_str().unwrap().contains("type it back"),
        "{refused}"
    );
    let mut typed = danger.clone();
    typed["typed"] = json!("Bash(rm *)");
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            "/api/perms/allow",
            Some(typed.clone()),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a different spelling is not the rule"
    );
    typed["typed"] = json!("Bash(rm:*)");
    assert_eq!(hub.post("/api/perms/allow", typed).await["written"], true);
    let activity = hub.get("/api/activity?kind=perms&machines=none").await;
    assert_eq!(
        activity["rows"][0]["text"],
        "allowed 1 permission rule for claude (local settings), typed-confirmed as dangerous"
    );

    // Folders outside every workspace are refused.
    let elsewhere = tempfile::tempdir().unwrap();
    let (status, _) = hub
        .call(
            reqwest::Method::GET,
            &format!("/api/perms?folder={}", elsewhere.path().display()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A peer credential is refused, reads and writes alike.
    let issued = hub.post("/api/peers", json!({ "name": "laptop" })).await;
    let peer = issued["credential"].as_str().unwrap();
    for (method, path) in [
        (reqwest::Method::GET, format!("/api/perms?session={sid}")),
        (reqwest::Method::POST, "/api/perms/allow".to_owned()),
    ] {
        let response = hub
            .http
            .request(method, format!("http://{}{path}", hub.addr))
            .bearer_auth(peer)
            .json(&json!({ "session": sid, "rules": ["Bash(make *)"], "confirm": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }

    // The CLI: show, allow (--yes), suggest; a dangerous rule without a
    // terminal is refused.
    let (code, out, err) = run_cli(&hub, &["perms", "show", repo.to_str().unwrap()]).await;
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("allow Bash(cargo test -p flywheel-hub *)"),
        "{out}"
    );
    let (code, out, err) = run_cli(
        &hub,
        &[
            "perms",
            "allow",
            repo.to_str().unwrap(),
            "WebFetch(domain:docs.rs)",
            "--file",
            "project",
            "--yes",
        ],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("This lets the agent fetch pages from docs.rs without asking"),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "wrote {}",
            repo.join(".claude/settings.json").display()
        )),
        "{out}"
    );
    let (code, _, err) = run_cli(
        &hub,
        &[
            "perms",
            "allow",
            repo.to_str().unwrap(),
            "Bash(sudo *)",
            "--yes",
        ],
    )
    .await;
    assert_eq!(code, 1);
    assert!(err.contains("dangerous"), "{err}");
    let (code, out, _) = run_cli(&hub, &["perms", "show", &sid, "--json"]).await;
    assert_eq!(code, 0);
    let shown: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(shown["session"]["id"], json!(sid));
    let (code, out, err) = run_cli(&hub, &["perms", "suggest", repo.to_str().unwrap()]).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("no repeated permission prompts"), "{out}");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_prompts_rules_and_settings() {
    let hub = TestHub::start().await;
    let repo = hub.repo();
    let codex_home = hub.dir.path().join("codex-home");
    let (sid, _) = hub.agent("codex").await;
    hub.screen_shows(&sid, "Press enter to confirm").await;
    hub.status_is(&sid, "blocked").await;
    let view = hub.get(&format!("/api/perms?session={sid}")).await;
    let pending = &view["pending"];
    assert_eq!(pending["request"]["source"], "screen");
    assert_eq!(
        pending["codex"]["rules"][0]["pattern"],
        json!(["cargo", "test", "-p", "parser"])
    );
    assert_eq!(view["codex"]["trust_level"], Value::Null);

    // Journaled on the screen's blocked status.
    let suggested = hub.get(&format!("/api/perms/suggest?session={sid}")).await;
    assert_eq!(suggested["suggestions"][0]["agent"], "codex", "{suggested}");

    // A prefix rule in the project's rules file, then the prompt answered y.
    let body = json!({
        "session": sid, "patterns": [["cargo", "test", "-p", "parser"]],
        "confirm": true, "answer": true, "request_key": pending["request"]["key"],
    });
    let done = hub.post("/api/perms/allow", hub.based(body).await).await;
    assert_eq!(done["written"], true);
    assert!(
        done["warning"].as_str().unwrap().contains("trusted"),
        "{done}"
    );
    assert_eq!(hub.answer("codex").await, "y");
    let rules = std::fs::read_to_string(repo.join(".codex/rules/default.rules")).unwrap();
    assert_eq!(
        rules,
        "prefix_rule(pattern = [\"cargo\", \"test\", \"-p\", \"parser\"], decision = \"allow\", justification = \"allowed from fw hub\")\n"
    );

    // Trust in the user's (temp) CODEX_HOME config, with other keys kept.
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::write(codex_home.join("config.toml"), "model = \"gpt-5-codex\"\n").unwrap();
    let trust =
        json!({ "folder": repo, "setting": "trust_level", "value": "trusted", "confirm": true });
    assert_eq!(
        hub.post("/api/perms/allow", hub.based(trust).await).await["written"],
        true
    );
    let config: toml::Table = std::fs::read_to_string(codex_home.join("config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(config["model"].as_str(), Some("gpt-5-codex"));
    assert_eq!(
        config["projects"][repo.to_str().unwrap()]["trust_level"].as_str(),
        Some("trusted")
    );
    // Full access needs the value typed back; undocumented values are refused.
    let full = hub.based(json!({ "folder": repo, "setting": "sandbox_mode", "value": "danger-full-access", "confirm": true })).await;
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            "/api/perms/allow",
            Some(full.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let bad = json!({ "folder": repo, "setting": "approval_policy", "value": "sometimes", "confirm": true });
    let (status, _) = hub
        .call(reqwest::Method::POST, "/api/perms/allow", Some(bad))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (code, out, err) = run_cli(
        &hub,
        &[
            "perms",
            "codex",
            repo.to_str().unwrap(),
            "sandbox_mode",
            "workspace-write",
            "--yes",
        ],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("Codex may change files in the workspace"),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join(".codex/config.toml")).unwrap(),
        "sandbox_mode = \"workspace-write\"\n"
    );
    let view = hub
        .get(&format!("/api/perms?folder={}", repo.display()))
        .await;
    assert_eq!(view["codex"]["trust_level"], "trusted");
    assert_eq!(view["codex"]["sandbox_mode"][0], "workspace-write");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_trust_and_symlinks_keep_the_typed_gate() {
    let hub = TestHub::start().await;
    let repo = hub.repo();
    hub.agent("codex").await;
    // A workspace folder by its directory name (resolved by the hub, as
    // for another machine's).
    let (code, out, err) = run_cli(&hub, &["perms", "show", "repo"]).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains(&format!("folder {}", repo.display())), "{out}");
    // A planted project config: trusting it turns these on.
    std::fs::create_dir_all(repo.join(".codex/rules")).unwrap();
    std::fs::write(
        repo.join(".codex/config.toml"),
        "approval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join(".codex/rules/default.rules"),
        "prefix_rule(pattern = [\"bash\"], decision = \"allow\")\n",
    )
    .unwrap();
    // A nested package reaching full access through a selected profile.
    std::fs::create_dir_all(repo.join("tools/gen/.codex")).unwrap();
    std::fs::write(
        repo.join("tools/gen/.codex/config.toml"),
        "profile = \"p\"\n[profiles.p]\nsandbox_mode = \"danger-full-access\"\n[mcp_servers.x]\ncommand = \"./x\"\n",
    )
    .unwrap();
    let trust = json!({ "folder": repo, "setting": "trust_level", "value": "trusted" });
    let preview = hub.post("/api/perms/preview", trust.clone()).await;
    assert_eq!(preview["type_to_confirm"], "trusted", "{preview}");
    let warning = preview["warning"].as_str().unwrap();
    for planted in [
        "approval_policy = never",
        "danger-full-access",
        "\"bash\"",
        "tools/gen/.codex/config.toml: [profiles.p] (selected) sandbox_mode = danger-full-access",
        "mcp_servers.x runs `./x`",
    ] {
        assert!(warning.contains(planted), "{planted}: {warning}");
    }
    let notes = preview["notes"].to_string();
    assert!(
        notes.contains("loads ") && notes.contains("tools/gen/.codex"),
        "{notes}"
    );
    let mut body = hub.based(trust.clone()).await;
    body["confirm"] = json!(true);
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            "/api/perms/allow",
            Some(body.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    body["typed"] = json!("trusted");
    assert_eq!(hub.post("/api/perms/allow", body).await["written"], true);

    // A project .codex that is a symlink (to the user's) is refused.
    let elsewhere = hub.dir.path().join("elsewhere-codex");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::remove_dir_all(repo.join(".codex")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, repo.join(".codex")).unwrap();
    for body in [
        json!({ "folder": repo, "patterns": [["cargo", "test"]] }),
        json!({ "folder": repo, "setting": "sandbox_mode", "value": "read-only" }),
    ] {
        let (status, err) = hub
            .call(reqwest::Method::POST, "/api/perms/preview", Some(body))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
        assert!(err.to_string().contains("symlink"), "{err}");
    }
    assert!(!elsewhere.join("rules").exists());
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_race_safely_and_answers_check_the_screen() {
    let hub = TestHub::start().await;
    let repo = hub.repo();
    let local = repo.join(".claude/settings.local.json");
    let (sid, env) = hub.agent("claude").await;
    hub.screen_shows(&sid, "Do you want to proceed?").await;

    // Concurrent writes from one preview: one lands, the others are told
    // the file changed; no rule a response reports written is lost.
    for round in 0..6 {
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, "{}\n").unwrap();
        let bodies: Vec<Value> = (0..4)
            .map(|i| json!({ "folder": repo, "rules": [format!("Bash(cargo test -p c{round}x{i} *)")], "confirm": true }))
            .collect();
        let mut based = Vec::new();
        for b in bodies {
            based.push(hub.based(b).await);
        }
        let calls = based.iter().map(|b| {
            let (http, url, token, b) = (
                hub.http.clone(),
                format!("http://{}/api/perms/allow", hub.addr),
                hub.token.clone(),
                b.clone(),
            );
            async move {
                let response = http
                    .post(url)
                    .bearer_auth(token)
                    .json(&b)
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                (
                    status,
                    response.json::<Value>().await.unwrap_or(Value::Null),
                )
            }
        });
        let results = futures_join(calls).await;
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&local).unwrap()).unwrap();
        let present = written["permissions"]["allow"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut wrote = 0;
        for ((status, answer), body) in results.iter().zip(&based) {
            if status.is_success() {
                assert_eq!(answer["written"], true, "{answer}");
                assert!(
                    present.contains(&body["rules"][0]),
                    "round {round}: {answer} lost: {written}"
                );
                wrote += 1;
            } else {
                assert_eq!(*status, StatusCode::CONFLICT, "round {round}: {answer}");
            }
        }
        assert_eq!(wrote, present.len(), "round {round}: {written}");
        assert!(wrote >= 1);
    }
    std::fs::remove_file(&local).unwrap();
    // Without the preview's base: refused.
    let (status, refused) = hub
        .call(
            reqwest::Method::POST,
            "/api/perms/allow",
            Some(json!({ "folder": repo, "rules": ["Bash(cargo test *)"], "confirm": true })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");

    // A deny elsewhere overrides the allow: said before writing, marked after.
    std::fs::write(
        repo.join(".claude/settings.json"),
        "{\"permissions\":{\"deny\":[\"Bash(cargo bench *)\"]}}\n",
    )
    .unwrap();
    let bench = json!({ "folder": repo, "rules": ["Bash(cargo bench -p app)"] });
    let preview = hub.post("/api/perms/preview", bench.clone()).await;
    assert!(
        preview["notes"]
            .to_string()
            .contains("the deny rule Bash(cargo bench *) (project) overrides"),
        "{preview}"
    );
    let mut confirmed = bench.clone();
    confirmed["confirm"] = json!(true);
    confirmed["base"] = preview["base"].clone();
    hub.post("/api/perms/allow", confirmed).await;
    let view = hub
        .get(&format!("/api/perms?folder={}", repo.display()))
        .await;
    let local_source = view["claude"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["scope"] == "local")
        .unwrap()
        .clone();
    assert_eq!(
        local_source["overridden"][0]["by"], "Bash(cargo bench *)",
        "{local_source}"
    );

    // A hook request that is not the dialog on screen is never answered.
    hook(
        &env,
        "PermissionRequest",
        json!({ "tool_name": "Bash", "tool_input": { "command": "cargo build -p other" } }),
    );
    hub.status_is(&sid, "blocked").await;
    let view = hub.get(&format!("/api/perms?session={sid}")).await;
    let stale = hub
        .based(json!({
            "session": sid, "rules": ["Bash(cargo build -p other *)"], "confirm": true,
            "answer": true, "request_key": view["pending"]["request"]["key"],
        }))
        .await;
    let done = hub.post("/api/perms/allow", stale).await;
    assert_eq!(done["answered"], false, "{done}");
    assert!(
        done["answer_note"]
            .as_str()
            .unwrap()
            .contains("not the request"),
        "{done}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !hub.dir.path().join("claude.answer").exists(),
        "a key was typed"
    );

    // The CLI on a session answers the prompt its rule covers.
    let command = "cargo test -p flywheel-hub --lib perms::x";
    hook(
        &env,
        "PermissionRequest",
        json!({ "tool_name": "Bash", "tool_input": { "command": command } }),
    );
    // Journaled: one suggestion more than the stale request's.
    let suggested = hub.suggestions(&repo, 2).await;
    assert!(suggested.to_string().contains("--lib"), "{suggested}");
    // The screen shows the session's original command; make the hook's
    // request match it (the dialog is what gets answered).
    hook(
        &env,
        "PermissionRequest",
        json!({ "tool_name": "Bash", "tool_input": { "command": "cargo test -p flywheel-hub perms::x" } }),
    );
    let (code, out, err) = run_cli(
        &hub,
        &[
            "perms",
            "allow",
            &sid,
            "Bash(cargo test -p flywheel-hub *)",
            "--yes",
        ],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("answered the pending prompt: Yes"), "{out}");
    assert_eq!(hub.answer("claude").await, "1");

    // A broader rule now covers the `--lib` request: no longer suggested.
    let found = hub
        .get(&format!("/api/perms/suggest?folder={}", repo.display()))
        .await;
    assert!(!found.to_string().contains("--lib"), "{found}");

    // --gitignore: the local file is added to the checkout's .gitignore.
    let (code, out, err) = run_cli(
        &hub,
        &[
            "perms",
            "allow",
            repo.to_str().unwrap(),
            "Bash(make test)",
            "--yes",
            "--gitignore",
        ],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("added it to"), "{out}");
    assert_eq!(
        std::fs::read_to_string(repo.join(".gitignore")).unwrap(),
        "/.claude/settings.local.json\n"
    );
    let ignored = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["check-ignore", "-q", ".claude/settings.local.json"])
        .status()
        .unwrap();
    assert!(ignored.success());
    hub.shutdown().await;
}

/// Run every call at once and collect the answers in order.
async fn futures_join<F>(calls: impl Iterator<Item = F>) -> Vec<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handles: Vec<_> = calls.map(tokio::spawn).collect();
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.unwrap());
    }
    out
}
