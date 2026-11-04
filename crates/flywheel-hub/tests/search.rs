//! Search end to end: a real hub on 127.0.0.1:0 with real shell PTYs and
//! scripted stand-ins for the agent CLIs. Terminal output, Claude Code
//! transcripts, and Codex rollouts written the way the agents write them
//! are found over `GET /api/search`, `GET /api/sessions/{id}/transcript`,
//! and `fw hub search`, also after the hub restarts with the agent
//! sessions stopped.

mod support;

use flywheel_hub::agent::claude_project_dir_name;
use flywheel_hub::remote::RuntimeInfo;
use flywheel_hub::report::{self, Report};
use flywheel_hub::{HubOptions, ServerOptions, serve};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};

struct TestHub {
    dir: tempfile::TempDir,
    addr: SocketAddr,
    token: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    async fn start() -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("repo/.git")).unwrap();
        Self::reopen(dir, true).await
    }

    /// A hub on `dir`'s state; `restore` brings back what a previous one
    /// stopped (else those sessions stay stopped, without a terminal).
    async fn reopen(dir: tempfile::TempDir, restore: bool) -> Self {
        let mut options = HubOptions::new(dir.path().join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = fake_agent(dir.path(), "claude")
            .to_string_lossy()
            .into_owned();
        options.agents.codex = fake_agent(dir.path(), "codex")
            .to_string_lossy()
            .into_owned();
        options.report_command = Some(vec!["/opt/flywheel/bin/fw".into(), "hub".into()]);
        options.claude_config_dir = Some(dir.path().join("claude-config"));
        options.codex_home = Some(dir.path().join("codex-home"));
        let hub = support::open_hub(options);
        if restore {
            hub.restore_sessions();
        }
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // What `fw hub serve` records, so the CLI finds this server.
        RuntimeInfo {
            pid: std::process::id(),
            listen: addr,
            public_urls: Vec::new(),
            started_at: None,
            log_file: None,
            tls: None,
        }
        .write(&dir.path().join("state"))
        .unwrap();
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

    async fn post(&self, path: &str, body: Value) -> Value {
        let (status, value) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert!(status.is_success(), "{path}: {status} {value}");
        value
    }

    async fn get(&self, path: &str) -> Value {
        let (status, value) = self.call(reqwest::Method::GET, path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {value}");
        value
    }

    async fn search(&self, query: &str) -> Value {
        self.get(&format!("/api/search?{query}")).await
    }

    /// Type `line` + Enter into a shell, and wait for `marker` on screen.
    async fn run(&self, sid: &str, line: &str, marker: &str) {
        self.post(
            &format!("/api/sessions/{sid}/input"),
            json!({ "data": format!("{line}\r") }),
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let screen = self
                .get(&format!("/api/sessions/{sid}/screen?source=recent&lines=5"))
                .await;
            let lines = screen["lines"].as_array().unwrap();
            if lines.iter().any(|l| l.as_str() == Some(marker)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{marker:?} never appeared: {screen}"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
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

/// A stand-in agent CLI: dumps its environment, then echoes its input.
fn fake_agent(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let env = dir.join(format!("{name}.env"));
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\nenv > '{e}.tmp' && mv '{e}.tmp' '{e}'\nexec cat\n",
            e = env.display()
        ),
    );
    path
}

async fn agent_env(hub: &TestHub, name: &str) -> HashMap<String, String> {
    let path = hub.dir.path().join(format!("{name}.env"));
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(text) = std::fs::read_to_string(&path)
            && !text.is_empty()
        {
            return text
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
        }
        assert!(Instant::now() < deadline, "{name} never launched");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

fn append(path: &Path, line: &Value) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{line}").unwrap();
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

fn group<'a>(answer: &'a Value, sid: &str) -> &'a Value {
    answer["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["session_id"] == sid)
        .unwrap_or_else(|| panic!("no group for {sid}: {answer}"))
}

fn sources(group: &Value) -> Vec<&str> {
    group["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["source"].as_str().unwrap())
        .collect()
}

/// A Claude session with a transcript (a prompt, a reply with a shell
/// block, a tool call), a Codex session with a rollout, and a shell with
/// real output.
struct Fixture {
    hub: TestHub,
    wid: String,
    shell: String,
    claude: String,
    codex: String,
}

const THREAD: &str = "01a07c85-9201-7123-8801-041350ceed6a";

async fn fixture() -> Fixture {
    let hub = TestHub::start().await;
    let ws = hub
        .post(
            "/api/workspaces",
            json!({ "name": "w", "folders": [hub.repo()] }),
        )
        .await;
    let wid = ws["id"].as_str().unwrap().to_owned();
    let shell = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "name": "build" }),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let claude = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "kind": "claude", "name": "agent" }),
        )
        .await;
    let conversation = claude["agent_session_id"].as_str().unwrap().to_owned();
    let claude = claude["id"].as_str().unwrap().to_owned();
    let transcript = hub
        .dir
        .path()
        .join("claude-config/projects")
        .join(claude_project_dir_name(&hub.repo()))
        .join(format!("{conversation}.jsonl"));
    append(
        &transcript,
        &json!({"type": "user", "uuid": "u1", "timestamp": "2026-11-02T10:00:00Z",
            "message": {"role": "user", "content": "please restart the zebra service"}}),
    );
    append(
        &transcript,
        &json!({"type": "assistant", "uuid": "a1", "timestamp": "2026-11-02T10:00:05Z",
        "message": {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "zebra thoughts stay private"},
            {"type": "text", "text": "Restart it with:\n\n```bash\nkubectl rollout restart deploy/zebra\n```"},
            {"type": "tool_use", "id": "t1", "name": "Bash",
                "input": {"command": "kubectl get pods -l app=zebra", "description": "List pods"}}
        ]}}),
    );
    append(
        &transcript,
        &json!({"type": "user", "uuid": "u2", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "zebra-7f9 Running"}]}}),
    );
    append(
        &transcript,
        &json!({"type": "assistant", "uuid": "s1", "isSidechain": true,
            "message": {"role": "assistant", "content": "zebra from a subagent"}}),
    );

    let codex = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "kind": "codex", "name": "coder" }),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let rollout = hub
        .dir
        .path()
        .join("codex-home/sessions/2026/11/02")
        .join(format!("rollout-2026-11-02T10-00-00-{THREAD}.jsonl"));
    append(
        &rollout,
        &json!({"timestamp": "2026-11-02T11:00:00Z", "type": "session_meta", "payload": {"id": THREAD}}),
    );
    append(
        &rollout,
        &json!({"timestamp": "2026-11-02T11:00:01Z", "type": "response_item", "payload": {
            "type": "message", "role": "user", "content": [{"type": "input_text", "text": "migrate the giraffe table"}]}}),
    );
    append(
        &rollout,
        &json!({"timestamp": "2026-11-02T11:00:02Z", "type": "response_item", "payload": {
            "type": "function_call", "name": "shell", "call_id": "call_1",
            "arguments": "{\"command\":[\"bash\",\"-lc\",\"psql -c 'alter table giraffe add column zebra int'\"]}"}}),
    );
    // Codex tells the hub its thread id with its first notify.
    let env = agent_env(&hub, "codex").await;
    report::send(
        Path::new(&env[report::SOCKET_ENV]),
        &Report {
            session: codex.clone(),
            secret: env[report::SECRET_ENV].clone(),
            source: "codex".into(),
            event: "notify".into(),
            detail: None,
            payload: Some(json!({
                "type": "agent-turn-complete",
                "thread-id": THREAD,
                "turn-id": "t1",
                "last-assistant-message": "Done.",
            })),
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while hub.get(&format!("/api/sessions/{codex}")).await["agent_session_id"] != THREAD {
        assert!(Instant::now() < deadline, "codex never reported its thread");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    hub.run(
        &shell,
        "seq 1 3000 | sed 's/^/row-/'; echo zebra-$((40+2))",
        "zebra-42",
    )
    .await;
    Fixture {
        hub,
        wid,
        shell,
        claude,
        codex,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_finds_scrollback_transcripts_and_commands_across_sessions() {
    let Fixture {
        hub,
        wid,
        shell,
        claude,
        codex,
    } = fixture().await;

    let answer = hub.search("q=zebra").await;
    assert_eq!(answer["partial"], false, "{answer}");
    assert_eq!(answer["truncated"], false);

    // The shell: the output line (and the typed command line).
    let terminal = group(&answer, &shell);
    assert_eq!(terminal["kind"], "shell");
    assert_eq!(terminal["terminal"], true);
    let output = &terminal["hits"][0];
    assert_eq!(output["source"], "scrollback");
    assert_eq!(output["match"], "zebra");
    assert_eq!(output["match_text"], "zebra");
    assert_eq!(output["after"], "-42");
    assert_eq!(output["screen"], "primary");
    assert_eq!(output["occurrence"], 0);
    assert!(output["from_end"].as_u64().unwrap() <= 2, "{output}");
    assert!(
        terminal["hits"][1]["after"]
            .as_str()
            .unwrap()
            .starts_with("-$((40+2))"),
        "the command line: {terminal}"
    );

    // Claude: the prompt, the reply, the tool call, and its result (named
    // for the call's tool; newest first); not thinking or a subagent's
    // sidechain. The reply's shell block is a suggested command too.
    let agent = group(&answer, &claude);
    assert_eq!(
        sources(agent),
        [
            "transcript",
            "transcript",
            "transcript",
            "transcript",
            "commands"
        ],
        "{agent}"
    );
    let hits = agent["hits"].as_array().unwrap();
    assert_eq!(hits[0]["kind"], "result");
    assert_eq!(hits[0]["tool"], "Bash");
    assert_eq!(hits[0]["index"], 3);
    assert_eq!(hits[0]["after"], "-7f9 Running");
    let hits = &hits[1..];
    assert_eq!(hits[0]["kind"], "tool");
    assert_eq!(hits[0]["tool"], "Bash");
    assert_eq!(hits[0]["index"], 2);
    assert_eq!(hits[1]["role"], "assistant");
    assert_eq!(hits[1]["kind"], "text");
    assert_eq!(hits[1]["line"], 3, "the match's line in the message");
    assert_eq!(hits[1]["timestamp"], "2026-11-02T10:00:05Z");
    assert_eq!(hits[2]["role"], "user");
    assert_eq!(hits[2]["before"], "please restart the ");
    assert_eq!(hits[3]["lang"], "bash");
    assert_eq!(hits[3]["shell"], true);
    assert_eq!(hits[3]["before"], "kubectl rollout restart deploy/");
    assert_eq!(
        agent["counts"],
        json!({"scrollback": 0, "transcript": 4, "commands": 1})
    );

    // Codex: its rollout's tool call (the user message names a giraffe).
    let coder = group(&answer, &codex);
    assert_eq!(sources(coder), ["transcript"]);
    assert_eq!(coder["hits"][0]["tool"], "shell");
    let giraffe = hub.search("q=giraffe&source=transcript").await;
    assert_eq!(group(&giraffe, &codex)["counts"]["transcript"], 2);

    // The transcript viewer around a hit: messages with their text.
    let page = hub
        .get(&format!(
            "/api/sessions/{claude}/transcript?around=1&before=1&after=1"
        ))
        .await;
    let messages = page["messages"].as_array().unwrap();
    let indexes: Vec<u64> = messages
        .iter()
        .map(|m| m["index"].as_u64().unwrap())
        .collect();
    assert_eq!(indexes, [0, 1, 2]);
    assert_eq!(messages[0]["text"], "please restart the zebra service");
    assert!(messages[1]["text"].as_str().unwrap().contains("```bash"));
    assert_eq!(page["first"], 0);
    assert_eq!(page["next"], 4);
    assert_eq!(page["complete"], true);
    // The newest: the tool's result, named for its call.
    let newest = hub
        .get(&format!("/api/sessions/{claude}/transcript?before=1"))
        .await;
    assert_eq!(newest["messages"][0]["index"], 3);
    assert_eq!(newest["messages"][0]["kind"], "result");
    assert_eq!(newest["messages"][0]["tool"], "Bash");
    assert_eq!(newest["messages"][0]["text"], "zebra-7f9 Running");
    // ... and the index of that call (parallel calls' results come back
    // in any order).
    assert_eq!(newest["messages"][0]["call_index"], 2);
    // ... and that call, short, to tell it from its parallel siblings.
    assert!(
        newest["messages"][0]["call"]
            .as_str()
            .is_some_and(|c| c.starts_with("Bash: ")),
        "{newest}"
    );
    let (status, body) = hub
        .call(
            reqwest::Method::GET,
            &format!("/api/sessions/{shell}/transcript"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Filters: one session, one source, a kind, a workspace.
    let only = hub
        .search(&format!("q=zebra&session={claude}&source=commands"))
        .await;
    assert_eq!(only["groups"].as_array().unwrap().len(), 1);
    assert_eq!(sources(&only["groups"][0]), ["commands"]);
    let shells = hub.search("q=zebra&kind=shell").await;
    assert_eq!(shells["groups"].as_array().unwrap().len(), 1);
    assert_eq!(shells["groups"][0]["session_id"], shell.as_str());
    let in_ws = hub
        .search(&format!("q=zebra&ws={wid}&kind=claude,codex"))
        .await;
    assert_eq!(in_ws["groups"].as_array().unwrap().len(), 2);

    // Case and regex.
    assert_eq!(hub.search("q=ZEBRA&case=true").await["hits"], 0);
    let rows = hub
        .search(&format!(
            "q={}&regex=true&session={shell}&per_session=5",
            "%5Erow-%5Cd%2B%24"
        ))
        .await;
    let rows = &rows["groups"][0];
    assert_eq!(
        rows["counts"]["scrollback"], 3000,
        "every line, not only those returned"
    );
    assert_eq!(rows["more"], true);
    let newest: Vec<&str> = rows["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["match"].as_str().unwrap())
        .collect();
    assert_eq!(
        newest,
        ["row-3000", "row-2999", "row-2998", "row-2997", "row-2996"]
    );
    let capped = hub.search("q=row-&limit=7&per_session=50").await;
    assert_eq!(capped["hits"], 7);
    assert_eq!(capped["truncated"], true);

    // Bad requests answer 400 with why; an unknown session 404.
    for (bad, why) in [
        ("q=", "required"),
        ("q=(&regex=true", "invalid regex"),
        (
            "q=%5Cw%7B1000%7D%5Cw%7B1000%7D%5Cw%7B1000%7D&regex=true",
            "too large",
        ),
        ("q=a&source=disk", "unknown source"),
        ("q=a&kind=vim", "unknown kind"),
        ("q=a&limit=0", "limit"),
        ("q=a&per_session=5000", "per_session"),
        ("q=a&case=maybe", "case"),
    ] {
        let (status, body) = hub
            .call(reqwest::Method::GET, &format!("/api/search?{bad}"), None)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains(why),
            "{bad}: {body}"
        );
    }
    let (status, _) = hub
        .call(reqwest::Method::GET, "/api/search?q=a&session=nope", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Like every route, the token is required.
    let anonymous = hub
        .http
        .get(format!("http://{}/api/search?q=zebra", hub.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    // The CLI: grouped text (exit 0), JSON, a session by name, no hits
    // (exit 1).
    let state = hub.state_dir();
    let (code, out, err) = tokio::task::spawn_blocking({
        let state = state.clone();
        move || {
            cli(vec![
                "search".into(),
                "zebra".into(),
                "--state-dir".into(),
                state,
            ])
        }
    })
    .await
    .unwrap();
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("w/agent (claude, running)"), "{out}");
    assert!(out.contains("transcript #2 tool Bash"), "{out}");
    assert!(out.contains("please restart the [zebra] service"), "{out}");
    assert!(out.contains("commands #"), "{out}");
    assert!(err.contains("hits in 3 sessions"), "{err}");
    let (code, out, _) = tokio::task::spawn_blocking({
        let state = state.clone();
        move || {
            cli(vec![
                "search".into(),
                "zebra".into(),
                "--session".into(),
                "coder".into(),
                "--json".into(),
                "--state-dir".into(),
                state,
            ])
        }
    })
    .await
    .unwrap();
    assert_eq!(code, 0);
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["groups"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["groups"][0]["session"], "coder");
    let (code, _, err) = tokio::task::spawn_blocking(move || {
        cli(vec![
            "search".into(),
            "^zeb.a-4[2]$".into(),
            "--regex".into(),
            "--case".into(),
            "--source".into(),
            "transcript".into(),
            "--state-dir".into(),
            state,
        ])
    })
    .await
    .unwrap();
    assert_eq!(code, 1, "no transcript has that line");
    assert!(err.contains("0 hits"), "{err}");

    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopped_agent_sessions_keep_their_transcripts_and_commands_searchable() {
    let Fixture { hub, claude, .. } = fixture().await;
    // The commands were read while it ran.
    let before = hub.search(&format!("q=zebra&session={claude}")).await;
    assert_eq!(group(&before, &claude)["counts"]["commands"], 1);
    let dir = hub.shutdown().await;

    // The next hub does not restore them: stopped, no terminal.
    let hub = TestHub::reopen(dir, false).await;
    let answer = hub.search("q=zebra").await;
    let agent = group(&answer, &claude);
    assert_eq!(agent["state"], "stopped");
    assert_eq!(agent["terminal"], false);
    assert_eq!(
        agent["counts"],
        json!({"scrollback": 0, "transcript": 4, "commands": 1})
    );
    // No terminal anywhere: no scrollback hits.
    assert!(
        answer["groups"]
            .as_array()
            .unwrap()
            .iter()
            .all(|g| g["counts"]["scrollback"] == 0),
        "{answer}"
    );
    let page = hub.get(&format!("/api/sessions/{claude}/transcript")).await;
    assert_eq!(page["messages"].as_array().unwrap().len(), 4);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exited_sessions_and_full_screen_programs_are_searched() {
    let hub = TestHub::start().await;
    let ws = hub
        .post(
            "/api/workspaces",
            json!({ "name": "w", "folders": [hub.repo()] }),
        )
        .await;
    let wid = ws["id"].as_str().unwrap();
    // Prints, then exits: its terminal is kept, as exited.
    let done = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "command": ["/bin/sh", "-c", "echo walrus-finished"] }),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Prints on the primary screen, then draws a full-screen view.
    let tui = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "command": ["/bin/sh", "-c",
                "echo walrus-before; printf '\\033[?1049h\\033[Hwalrus-on-alt'; exec cat"] }),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(60);
    let answer = loop {
        let answer = hub.search("q=walrus").await;
        let exited = answer["groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["session_id"] == done && g["state"] == "exited");
        if exited && answer["hits"] == 3 {
            break answer;
        }
        assert!(Instant::now() < deadline, "{answer}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let finished = group(&answer, &done);
    assert_eq!(finished["terminal"], true);
    assert_eq!(finished["hits"][0]["after"], "-finished");
    let screens: Vec<(&str, &str)> = group(&answer, &tui)["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["screen"].as_str().unwrap(), h["after"].as_str().unwrap()))
        .collect();
    assert_eq!(screens, [("alternate", "-on-alt"), ("primary", "-before")]);
    hub.shutdown().await;
}

/// The size the task asks to measure: 20 sessions x 5k lines. Prints the
/// latency (debug build here; `docs/hub/internals.md` quotes the release numbers).
/// An answer is complete inside the budget, or partial having spent it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_sessions_of_five_thousand_lines_answer_within_the_budget() {
    let hub = TestHub::start().await;
    let ws = hub
        .post(
            "/api/workspaces",
            json!({ "name": "w", "folders": [hub.repo()] }),
        )
        .await;
    let wid = ws["id"].as_str().unwrap();
    let mut ids = Vec::new();
    for i in 0..20 {
        let script = format!(
            "awk 'BEGIN {{ for (i = 1; i <= 4990; i++) printf \"s{i} line %d lorem ipsum dolor sit amet consectetur %d\\n\", i, i * 7 }}'; echo s{i}-ready; exec cat"
        );
        let id = hub
            .post(
                "/api/sessions",
                json!({ "workspace_id": wid, "command": ["/bin/sh", "-c", script] }),
            )
            .await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        ids.push(id);
    }
    // Wait on the screens, not on search: the first search below reads
    // every terminal cold.
    for (i, id) in ids.iter().enumerate() {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let screen = hub
                .get(&format!("/api/sessions/{id}/screen?source=recent&lines=3"))
                .await;
            if screen["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l == &json!(format!("s{i}-ready")))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "session {i} never finished printing"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    for (query, label) in [
        ("q=s0%20line%204990%20", "rare literal, cold"),
        ("q=s0%20line%204990%20", "rare literal, warm"),
        ("q=lorem&limit=2000", "every line"),
        ("q=LOREM%20IPSUM&case=true", "no match, case-sensitive"),
        ("q=line%20%5Cd%2B7%20&regex=true&limit=2000", "regex"),
    ] {
        // A loaded machine can run a cold search past the budget: the
        // answer must then say so (`partial`) having spent the whole
        // budget, and the same query again resumes on the index the first
        // one warmed, until one answer is complete.
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut attempt = 0;
        let answer = loop {
            attempt += 1;
            let started = std::time::Instant::now();
            let answer = hub.search(query).await;
            let wall = started.elapsed();
            eprintln!(
                "search {label} (attempt {attempt}): {} ms server, {} ms wall, {} hits, {} lines, partial {}",
                answer["elapsed_ms"],
                wall.as_millis(),
                answer["hits"],
                answer["searched"]["lines"],
                answer["partial"]
            );
            if answer["partial"] == false {
                break answer;
            }
            assert_eq!(answer["partial"], true, "{label}: {answer}");
            let budget = u64::try_from(flywheel_hub::search::TIME_BUDGET.as_millis()).unwrap();
            assert!(
                answer["elapsed_ms"].as_u64().unwrap() >= budget,
                "{label}: partial before the budget ran out: {}",
                answer["elapsed_ms"]
            );
            assert!(Instant::now() < deadline, "{label}: never complete");
        };
        assert_eq!(answer["searched"]["sessions"], 20, "{label}");
        assert!(
            answer["searched"]["lines"].as_u64().unwrap() >= 20 * 4990,
            "{label}"
        );
        if label.starts_with("rare literal") {
            assert_eq!(answer["hits"], 1, "{answer}");
            assert_eq!(answer["groups"][0]["session_id"], ids[0].as_str());
        }
        if label == "every line" {
            assert_eq!(answer["groups"].as_array().unwrap().len(), 20);
            assert_eq!(answer["groups"][3]["counts"]["scrollback"], 4990);
        }
    }
    hub.shutdown().await;
}

/// Past `limit`, later sessions are still listed with their counts; a
/// session's hits page through with `offset` until none are left.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_past_the_limit_are_counted_and_hits_page_with_offset() {
    let hub = TestHub::start().await;
    let ws = hub
        .post(
            "/api/workspaces",
            json!({ "name": "w", "folders": [hub.repo()] }),
        )
        .await;
    let wid = ws["id"].as_str().unwrap();
    let mut ids = Vec::new();
    for i in 0..4 {
        let script = format!(
            "awk 'BEGIN {{ for (i = 1; i <= 300; i++) printf \"needle-%d\\n\", i }}'; echo p{i}-ready; exec cat"
        );
        let id = hub
            .post(
                "/api/sessions",
                json!({ "workspace_id": wid, "name": format!("p{i}"), "command": ["/bin/sh", "-c", script] }),
            )
            .await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        ids.push(id);
    }
    for (i, id) in ids.iter().enumerate() {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !hub
            .get(&format!("/api/sessions/{id}/screen?source=recent&lines=3"))
            .await["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l == &json!(format!("p{i}-ready")))
        {
            assert!(Instant::now() < deadline, "session {i} never finished");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // 100 lines match per session; a limit of 15 fills two sessions, and
    // the other two are listed by count, not left out.
    let answer = hub
        .search("q=needle-1%5Cd%5Cd%24&regex=true&limit=15&per_session=10")
        .await;
    assert_eq!(answer["hits"], 15);
    assert_eq!(answer["truncated"], true);
    let groups = answer["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 4, "{answer}");
    for (group, shown) in groups.iter().zip([10, 5, 0, 0]) {
        assert_eq!(group["hits"].as_array().unwrap().len(), shown);
        assert_eq!(group["counts"]["scrollback"], 100);
        assert_eq!(group["more"], true);
    }
    assert_eq!(answer["searched"]["sessions"], 4);

    // Paging one session: every hit once, newest first, then nothing.
    let mut seen = Vec::new();
    let mut offset = 0;
    loop {
        let page = hub
            .search(&format!(
                "q=needle-&session={}&per_session=120&offset={offset}",
                ids[2]
            ))
            .await;
        assert_eq!(page["offset"], offset);
        let Some(group) = page["groups"].as_array().unwrap().first() else {
            break;
        };
        let hits = group["hits"].as_array().unwrap();
        seen.extend(hits.iter().map(|h| {
            format!(
                "{}{}",
                h["match"].as_str().unwrap(),
                h["after"].as_str().unwrap()
            )
        }));
        offset += hits.len();
        assert_eq!(group["more"], offset < 300, "at {offset}");
        assert!(offset <= 300);
    }
    assert_eq!(seen.len(), 300);
    let newest: Vec<String> = (1..=300).rev().map(|n| format!("needle-{n}")).collect();
    assert_eq!(seen, newest, "every hit once, newest first");
    let (status, body) = hub
        .call(reqwest::Method::GET, "/api/search?q=a&offset=100001", None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // The CLI lists the counted sessions too.
    let state = hub.state_dir();
    let (code, out, err) = tokio::task::spawn_blocking(move || {
        cli(vec![
            "search".into(),
            "needle-1\\d\\d$".into(),
            "--regex".into(),
            "--limit".into(),
            "15".into(),
            "--per-session".into(),
            "10".into(),
            "--state-dir".into(),
            state,
        ])
    })
    .await
    .unwrap();
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("w/p3"), "{out}");
    assert!(out.contains("not shown, past --limit"), "{out}");
    assert!(
        err.contains("2 more matching sessions only counted"),
        "{err}"
    );
    hub.shutdown().await;
}
