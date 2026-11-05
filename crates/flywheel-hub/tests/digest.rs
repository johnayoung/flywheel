//! Scheduled digests end to end: a real hub with a real HTTP server, a
//! Claude Code stand-in blocked over the real report socket, the activity
//! journal, and a real HTTP server standing in for ntfy that records every
//! POST. The schedule is driven by `tick` at chosen instants.

mod support;

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::routing::post;
use flywheel_hub::activity::Draft;
use flywheel_hub::digest::{DigestConfig, DigestSection, DigestService, Due};
use flywheel_hub::notify::{Notifier, NotifyConfig};
use flywheel_hub::report::{self, Report};
use flywheel_hub::{Hub, HubEvent, HubOptions, ServerOptions, serve};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};

const REPORT_PROGRAM: &str = "/opt/flywheel/bin/fw";
const TOPIC: &str = "digest-topic-91c2";
const FAKE_TOKEN: &str = "ghp_Zq8rT1x9Kc2mB7nW4yH6vL3pJ0sD5fG8aQ";
const PROMPT: &str = "REFACTOR-THE-BILLING-MODULE-PLEASE";
const COMMAND: &str = "cat ~/.ssh/id_rsa";

#[derive(Debug, Clone)]
struct Received {
    headers: HashMap<String, String>,
    body: String,
}

/// A stand-in ntfy: records each POST, answers `status`.
struct Webhook {
    addr: SocketAddr,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Webhook {
    async fn start(status: u16) -> Self {
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let app = axum::Router::new().route(
            "/{*topic}",
            post(move |headers: HeaderMap, body: Bytes| {
                let sink = Arc::clone(&sink);
                async move {
                    sink.lock().unwrap().push(Received {
                        headers: headers
                            .iter()
                            .map(|(k, v)| {
                                (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned())
                            })
                            .collect(),
                        body: String::from_utf8_lossy(&body).into_owned(),
                    });
                    StatusCode::from_u16(status).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { addr, received }
    }

    fn notifier(&self) -> Arc<Notifier> {
        let mut config = NotifyConfig::from_parts(
            Some("token = \"tk_digest\"\n"),
            Some(&format!("http://{}/{TOPIC}", self.addr)),
        )
        .unwrap()
        .unwrap();
        config.click_base = Some("https://box.tail.ts.net".into());
        Notifier::new(config).unwrap()
    }

    fn all(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
}

type SharedConfig = Arc<Mutex<DigestConfig>>;

struct TestHub {
    dir: tempfile::TempDir,
    hub: Arc<Hub>,
    digest: Arc<DigestService>,
    config: SharedConfig,
    addr: SocketAddr,
    token: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

fn digest_config(times: &[String]) -> DigestConfig {
    DigestConfig::resolve(&DigestSection {
        times: Some(times.to_vec()),
        timezone: Some("UTC".into()),
        ..DigestSection::default()
    })
    .unwrap()
}

fn service(
    state: &Path,
    config: &SharedConfig,
    notifier: Option<Arc<Notifier>>,
) -> Arc<DigestService> {
    let config = Arc::clone(config);
    DigestService::new(
        state,
        move || config.lock().unwrap().clone(),
        notifier,
        None,
        Some("https://box.tail.ts.net".into()),
    )
}

impl TestHub {
    async fn start(webhook: &Webhook) -> Self {
        Self::start_with(webhook, false).await
    }

    /// With `watch`: the digest reads a real `hub.toml` watcher, and
    /// Settings (`PUT /api/digest/config`) writes it.
    async fn start_with(webhook: &Webhook, watch: bool) -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("repo/.git")).unwrap();
        let claude = fake_claude(dir.path());
        let mut options = HubOptions::new(dir.path().join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = claude.to_string_lossy().into_owned();
        options.report_command = Some(vec![REPORT_PROGRAM.into(), "hub".into()]);
        options.claude_config_dir = Some(dir.path().join("claude-config"));
        let hub = support::open_hub(options);
        let config: SharedConfig = Arc::new(Mutex::new(DigestConfig::default()));
        let watcher = watch.then(|| {
            Arc::new(
                flywheel_hub::config::ConfigWatch::open(
                    hub.state_dir(),
                    flywheel_hub::config::Flags::default(),
                    None,
                )
                .unwrap(),
            )
        });
        let digest = match &watcher {
            Some(watcher) => {
                let watcher = Arc::clone(watcher);
                DigestService::new(
                    hub.state_dir(),
                    move || watcher.current().digest,
                    Some(webhook.notifier()),
                    None,
                    None,
                )
            }
            None => service(hub.state_dir(), &config, Some(webhook.notifier())),
        };
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            Arc::clone(&hub),
            ServerOptions {
                digest: Some(Arc::clone(&digest)),
                config: watcher,
                ..ServerOptions::default()
            },
            async {
                let _ = stopped.await;
            },
        ));
        Self {
            dir,
            hub,
            digest,
            config,
            addr,
            token,
            http: reqwest::Client::new(),
            stop: Some(stop),
            server: Some(server),
        }
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

    /// A claude session in a fresh workspace: (session id, its hook env).
    async fn claude(&self) -> (String, HashMap<String, String>) {
        let repo = self.dir.path().join("repo").canonicalize().unwrap();
        let (_, workspace) = self
            .call(
                reqwest::Method::POST,
                "/api/workspaces",
                Some(json!({ "name": "infrared", "repos": [repo] })),
            )
            .await;
        let (_, session) = self
            .call(
                reqwest::Method::POST,
                "/api/sessions",
                Some(json!({ "workspace_id": workspace["id"], "kind": "claude", "name": "fixer" })),
            )
            .await;
        let env_file = self.dir.path().join("claude.env");
        let deadline = Instant::now() + Duration::from_secs(60);
        let env = loop {
            if let Ok(text) = std::fs::read_to_string(&env_file)
                && !text.is_empty()
            {
                break text
                    .lines()
                    .filter_map(|l| l.split_once('='))
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect::<HashMap<_, _>>();
            }
            assert!(Instant::now() < deadline, "claude never launched");
            tokio::time::sleep(Duration::from_millis(30)).await;
        };
        (session["id"].as_str().unwrap().to_owned(), env)
    }

    async fn status_is(&self, sid: &str, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let (_, view) = self
                .call(reqwest::Method::GET, &format!("/api/sessions/{sid}"), None)
                .await;
            if view["agent"]["status"] == want {
                return;
            }
            assert!(Instant::now() < deadline, "{sid} never {want}: {view}");
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

fn fake_claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\nenv > '{d}.env.tmp' && mv '{d}.env.tmp' '{d}.env'\nexec cat\n",
            d = path.display()
        ),
    );
    path
}

fn fire(env: &HashMap<String, String>, event: &str, payload: Value) {
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

/// A slot `minutes` from now (UTC, whole minutes): (its `HH:MM`, instant).
fn slot_in(minutes: i64) -> (String, chrono::DateTime<chrono::Utc>) {
    use chrono::{DurationRound, TimeDelta};
    let at = (chrono::Utc::now() + TimeDelta::minutes(minutes))
        .duration_trunc(TimeDelta::minutes(1))
        .unwrap();
    (at.format("%H:%M").to_string(), at)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scheduled_digest_goes_out_once_with_names_and_counts_only() {
    let webhook = Webhook::start(200).await;
    let hub = TestHub::start(&webhook).await;
    let (sid, env) = hub.claude().await;
    // Blocked now, on a command whose text must never reach a digest.
    fire(&env, "UserPromptSubmit", json!({ "prompt": PROMPT }));
    fire(
        &env,
        "PermissionRequest",
        json!({ "tool_name": "Bash", "tool_input": { "command": COMMAND } }),
    );
    hub.status_is(&sid, "blocked").await;
    // What the journal holds: a finished turn (whose text is a prompt), a
    // red PR on a session named with a token.
    let about = hub.hub.event_session(&sid).unwrap();
    hub.hub
        .activity()
        .record(Draft::new("turn.finished", PROMPT).about(&about));
    let mut tokened = about.clone();
    tokened["name"] = json!(format!("fixer {FAKE_TOKEN}"));
    hub.hub.activity().record(
        Draft::new("pr.checks_failed", COMMAND)
            .about(&tokened)
            .with("number", 7),
    );
    hub.hub.activity().record(
        Draft::new("usage", "x")
            .about(&about)
            .with("cost_usd", 0.42),
    );

    // A browser open and a plugin listening for digest.ready: every
    // channel is heard.
    let _browser = hub.hub.connect_viewer("test-browser");
    let plugin_dir = hub.hub.state_dir().join("plugins/digestlog");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("plugin.toml"),
        "[[events]]\non = \"digest.ready\"\ncommand = [\"/bin/true\"]\n",
    )
    .unwrap();
    flywheel_hub::plugins::set_enabled(hub.hub.state_dir(), "digestlog", true).unwrap();
    hub.digest
        .set_plugins(Arc::new(flywheel_hub::plugins::PluginHost::new(
            hub.hub.state_dir(),
            flywheel_hub::plugins::HostOptions {
                max_concurrent: 1,
                base_env: Vec::new(),
                enabled: true,
            },
        )));

    let (hhmm, slot) = slot_in(2);
    *hub.config.lock().unwrap() = digest_config(&[hhmm]);
    let mut events = hub.hub.subscribe();
    // The schedule takes effect; nothing is due yet.
    assert_eq!(
        hub.digest.tick(&hub.hub, chrono::Utc::now()).await,
        Due::Idle
    );
    let at = slot + chrono::TimeDelta::seconds(1);
    let Due::Send(pending) = hub.digest.tick(&hub.hub, at).await else {
        panic!("not sent at its slot")
    };
    let id = format!("dg-{}", slot.timestamp());
    assert_eq!(pending.id, id);

    // The push: once, names and counts, the blocked session as a button.
    let pushes = webhook.all();
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    let push = &pushes[0];
    assert!(push.headers["title"].starts_with("flywheel "), "{push:?}");
    assert!(push.headers["title"].ends_with(" digest"), "{push:?}");
    assert_eq!(push.headers["priority"], "high");
    assert_eq!(push.headers["tags"], "flywheel,digest");
    assert_eq!(push.headers["authorization"], "Bearer tk_digest");
    assert_eq!(
        push.headers["click"],
        format!("https://box.tail.ts.net/#digest={id}")
    );
    assert_eq!(
        push.headers["actions"],
        format!("view, fixer is blocked, https://box.tail.ts.net/#session={sid}")
    );
    assert!(
        push.body
            .starts_with("1 turn finished, 1 blocked, 1 PR red, $0.42 today, $0.42 this week"),
        "{}",
        push.body
    );
    assert!(
        push.body.contains("\n1. fixer is blocked (infrared)"),
        "{}",
        push.body
    );
    assert!(push.body.contains("\ninfrared: "), "{}", push.body);

    // Every browser is told; plugins get the digest.
    let mut told = false;
    let mut plugin_payload = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(told && plugin_payload.is_some()) {
        assert!(Instant::now() < deadline, "events missing");
        match timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Ok(HubEvent::Digest { id: got })) => {
                assert_eq!(got, id);
                told = true;
            }
            Ok(Ok(HubEvent::Lifecycle {
                event: "digest.ready",
                payload,
            })) => plugin_payload = Some(payload),
            Ok(_) => {}
            Err(_) => panic!("no digest events"),
        }
    }
    let plugin_payload = plugin_payload.unwrap();
    assert_eq!(plugin_payload["digest"]["id"], id);
    assert_eq!(plugin_payload["digest"]["totals"]["blocked_now"], 1);

    // Nothing forbidden anywhere it went.
    let (_, kept) = hub
        .call(reqwest::Method::GET, &format!("/api/digests/{id}"), None)
        .await;
    assert_eq!(
        kept["delivery"]["channels"],
        json!(["push", "browser", "plugin"])
    );
    let everything = format!("{pushes:?}{plugin_payload}{kept}");
    for forbidden in [
        PROMPT,
        COMMAND,
        "id_rsa",
        FAKE_TOKEN,
        "Bash",
        hub.token.as_str(),
        &env[report::SECRET_ENV],
        TOPIC,
    ] {
        assert!(
            !everything.contains(forbidden),
            "{forbidden} leaked: {everything}"
        );
    }

    // Exactly once: the same hub again, then a restarted one on the same
    // state dir, both after the slot.
    assert_eq!(
        hub.digest
            .tick(&hub.hub, at + chrono::TimeDelta::seconds(5))
            .await,
        Due::Idle
    );
    let restarted = service(hub.hub.state_dir(), &hub.config, Some(webhook.notifier()));
    assert_eq!(
        restarted
            .tick(&hub.hub, at + chrono::TimeDelta::seconds(30))
            .await,
        Due::Idle
    );
    assert_eq!(webhook.all().len(), 1);

    // The journal says a digest went out (counts, never the items).
    let rows = hub.hub.activity().rows(&flywheel_hub::activity::Filter {
        kinds: vec!["digest.ready".into()],
        ..Default::default()
    });
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].text.contains("1 blocked, 1 need you"),
        "{}",
        rows[0].text
    );

    // The API: the schedule, the last digest, a preview, a send by hand.
    let (status, view) = hub.call(reqwest::Method::GET, "/api/digest", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["enabled"], true);
    assert_eq!(view["timezone"], "UTC");
    assert_eq!(view["recent"][0]["id"], id);
    // (The real clock has not reached the slot the test ticked past.)
    assert_eq!(view["next"][0], json!(slot));
    let (status, preview) = hub
        .call(reqwest::Method::GET, "/api/digest/preview?since=2h", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preview["id"], "preview");
    assert_eq!(preview["totals"]["turns_finished"], 1);
    let (status, _) = hub
        .call(
            reqwest::Method::GET,
            "/api/digest/preview?since=never",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, sent) = hub
        .call(reqwest::Method::POST, "/api/digest/send", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert_eq!(sent["trigger"], "manual");
    assert_eq!(webhook.all().len(), 2);
    assert_eq!(webhook.all()[1].headers["title"], "flywheel digest");
    let (status, _) = hub
        .call(reqwest::Method::GET, "/api/digests/dg-1", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // `fw hub digest`: a preview (nothing sent), then --send --json.
    let base = vec![
        "--url".to_owned(),
        format!("http://{}", hub.addr),
        "--token".to_owned(),
        hub.token.clone(),
    ];
    let args: Vec<String> = ["digest", "--since", "3h"]
        .iter()
        .map(|a| (*a).to_owned())
        .chain(base.clone())
        .collect();
    let (code, out, err) = tokio::task::spawn_blocking(move || cli(args))
        .await
        .unwrap();
    assert_eq!(code, 0, "{err}");
    assert!(out.starts_with("flywheel digest ("), "{out}");
    assert!(
        out.contains(
            "Needs you:\n  1. fixer is blocked (infrared)  https://box.tail.ts.net/#session="
        ),
        "{out}"
    );
    assert!(!out.contains(PROMPT) && !out.contains(FAKE_TOKEN), "{out}");
    assert_eq!(webhook.all().len(), 2, "a preview sends nothing");
    let args: Vec<String> = ["digest", "--send", "--json"]
        .iter()
        .map(|a| (*a).to_owned())
        .chain(base)
        .collect();
    let (code, out, err) = tokio::task::spawn_blocking(move || cli(args))
        .await
        .unwrap();
    assert_eq!(code, 0, "{err}");
    let sent: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(sent["delivery"]["channels"][0], "push");
    assert_eq!(webhook.all().len(), 3);

    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_scheduled_digest_stays_quiet_and_a_failed_push_is_kept() {
    let webhook = Webhook::start(503).await;
    let hub = TestHub::start(&webhook).await;
    let (hhmm, slot) = slot_in(2);
    *hub.config.lock().unwrap() = digest_config(std::slice::from_ref(&hhmm));
    hub.digest.tick(&hub.hub, chrono::Utc::now()).await;
    let at = slot + chrono::TimeDelta::seconds(1);
    assert!(matches!(hub.digest.tick(&hub.hub, at).await, Due::Send(_)));
    assert!(webhook.all().is_empty(), "nothing in it: not sent");
    let (_, view) = hub.call(reqwest::Method::GET, "/api/digest", None).await;
    assert_eq!(view["recent"][0]["quiet"], true);

    // quiet_if_empty = false: sent, and the failing webhook is recorded.
    let mut config = digest_config(&[hhmm]);
    config.quiet_if_empty = false;
    *hub.config.lock().unwrap() = config;
    let next = at + chrono::TimeDelta::days(1);
    assert!(matches!(
        hub.digest.tick(&hub.hub, next).await,
        Due::Send(_)
    ));
    assert_eq!(webhook.all().len(), 1);
    assert_eq!(
        webhook.all()[0].body,
        "Nothing happened, and nothing waits on you."
    );
    let (_, view) = hub.call(reqwest::Method::GET, "/api/digest", None).await;
    let error = view["recent"][0]["push_error"].as_str().unwrap();
    assert!(error.contains("503"), "{error}");
    assert!(!error.contains(TOPIC), "{error}");
    // No browser open, no plugin listening: not claimed as delivered, and
    // said why.
    assert_eq!(view["recent"][0]["channels"], json!([]));
    let (_, kept) = hub
        .call(
            reqwest::Method::GET,
            &format!("/api/digests/{}", view["recent"][0]["id"].as_str().unwrap()),
            None,
        )
        .await;
    let unheard = kept["delivery"]["unheard"].to_string();
    assert!(
        unheard.contains("browser: no browser open")
            && unheard.contains("plugin: no enabled plugin"),
        "{unheard}"
    );
    // A slot missed by more than the grace window is not sent late.
    let late = next + chrono::TimeDelta::days(1) + chrono::TimeDelta::hours(3);
    assert!(matches!(
        hub.digest.tick(&hub.hub, late).await,
        Due::Skipped(_)
    ));
    assert_eq!(webhook.all().len(), 1);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_writes_never_revert_a_newer_hub_toml() {
    let webhook = Webhook::start(200).await;
    let hub = TestHub::start_with(&webhook, true).await;
    let path = hub.hub.state_dir().join(flywheel_hub::config::CONFIG_FILE);
    let put = |body: Value| hub.call(reqwest::Method::PUT, "/api/digest/config", Some(body));

    // Settings opens: it reads the schedule and the file's rev.
    let (status, view) = put(json!({
        "times": ["07:01"],
        "timezone": "America/New_York",
        "quiet_if_empty": false,
    }))
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    let (_, opened) = hub.call(reqwest::Method::GET, "/api/digest", None).await;
    let rev = opened["rev"].as_u64().unwrap();

    // Meanwhile the file is edited by hand (07:05).
    let text = std::fs::read_to_string(&path).unwrap();
    // (A size change too: the watcher's fingerprint cannot miss it.)
    std::fs::write(
        &path,
        format!("# edited by hand\n{}", text.replace("07:01", "07:05")),
    )
    .unwrap();

    // The open form saves what it loaded: refused as stale, the file kept.
    let (status, refused) = put(json!({ "times": ["07:01"], "rev": rev })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["stale"], true);
    assert!(std::fs::read_to_string(&path).unwrap().contains("07:05"));

    // Re-read and applied again: written; keys left out are kept.
    let (_, fresh) = hub.call(reqwest::Method::GET, "/api/digest", None).await;
    assert_eq!(fresh["times"], json!(["07:05"]));
    let (status, saved) = put(json!({
        "times": ["08:00", "08:00"],
        "rev": fresh["rev"],
    }))
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["times"], json!(["08:00"]));
    assert_eq!(saved["timezone"], "America/New_York");
    assert_eq!(saved["timezone_source"], "file");
    assert_eq!(saved["quiet_if_empty"], false);
    // `timezone: ""` removes it (the system's zone).
    let (status, cleared) = put(json!({ "timezone": "" })).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_ne!(cleared["timezone_source"], "file");
    assert!(!std::fs::read_to_string(&path).unwrap().contains("timezone"));

    // A digest cannot start after it ends.
    let (status, error) = hub
        .call(
            reqwest::Method::GET,
            "/api/digest/preview?since=2099-01-01T00:00:00Z",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error["error"].as_str().unwrap().contains("in the future"),
        "{error}"
    );
    hub.shutdown().await;
}
