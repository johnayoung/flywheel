//! Pull requests and CI end to end: a real hub on 127.0.0.1:0, a real git
//! repository whose branch is pushed to a bare `origin`, a stand-in
//! `claude` in a real PTY driven by its hook reports, and a scripted `gh`
//! answering from files the test rewrites (never GitHub). gh missing, not
//! signed in, rate limited; no pull request, open, failing, passing,
//! merged; the row badge; logs with a token redacted; asking the agent to
//! fix CI (pasted at rest, queued while it works); re-run, ready, merge
//! (refused while failing unless forced with the number); events and
//! activity rows; the slow timer.

mod support;

use flywheel_hub::report::{self, Report};
use flywheel_hub::review::Tools;
use flywheel_hub::{Hub, HubEvent, HubOptions, ServerOptions, serve};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};
use tokio::time::{Instant, sleep, timeout};

const WAIT: Duration = Duration::from_secs(60);
const HEAD_OID: &str = "0123456789abcdef0123456789abcdef01234567";

fn git(dir: &Path, args: &[&str]) {
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
}

/// A stand-in `claude` recording every byte typed into it (`<log>/in`), in
/// raw mode with bracketed paste on, and its environment (`<log>/env`).
fn fake_claude(bin: &Path, log: &Path) -> PathBuf {
    let path = bin.join("claude");
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\nenv > '{log}/env.tmp' && mv '{log}/env.tmp' '{log}/env'\n\
             stty raw -echo\nprintf '\\033[?2004h'\nexec cat >> '{log}/in'\n",
            log = log.display()
        ),
    );
    path
}

/// A scripted `gh`: every call appended to `<dir>/calls.log`; answers
/// from the files in `<dir>` (see [`Gh`]).
fn fake_gh(bin: &Path, dir: &Path) -> PathBuf {
    let path = bin.join("gh");
    support::write_script(
        &path,
        &format!(
            r#"#!/bin/sh
d='{dir}'
printf '%s\n' "$*" >> "$d/calls.log"
case "$1 $2" in
  "pr list")
    [ -f "$d/auth_fail" ] && {{ echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4; }}
    [ -f "$d/rate_limited" ] && {{ echo 'GraphQL: API rate limit exceeded for user ID 1.' >&2; exit 1; }}
    [ -f "$d/pr.json" ] || {{ echo '[]'; exit 0; }}
    printf '['; cat "$d/pr.json"; [ -f "$d/other.json" ] && {{ printf ','; cat "$d/other.json"; }}; printf ']\n';;
  "pr checks")
    cat "$d/checks.json"
    [ -f "$d/checks.exit" ] && exit "$(cat "$d/checks.exit")"
    exit 0;;
  "run view") cat "$d/log.txt";;
  "run rerun") echo "Requested rerun of run $3";;
  "pr ready") echo "Pull request #$3 is marked as ready for review";;
  "pr merge")
    [ -f "$d/merged.json" ] && cp "$d/merged.json" "$d/pr.json"
    echo "Merged pull request #$3";;
  "repo view") cat "$d/repo.json";;
  *) echo "unknown command $*" >&2; exit 1;;
esac
"#,
            dir = dir.display()
        ),
    );
    path
}

/// What `gh` answers next, written whole (the script never reads half).
struct Gh {
    dir: PathBuf,
}

impl Gh {
    fn put(&self, file: &str, text: &str) {
        let tmp = self.dir.join(format!("{file}.tmp"));
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(tmp, self.dir.join(file)).unwrap();
    }

    fn flag(&self, file: &str, on: bool) {
        if on {
            self.put(file, "");
        } else {
            let _ = std::fs::remove_file(self.dir.join(file));
        }
    }

    fn pr(&self, state: &str, draft: bool, review: Option<&str>) -> Value {
        json!({
            "number": 7, "title": "Fix the flaky retry", "url": "https://github.com/o/app/pull/7",
            "state": state, "isDraft": draft, "reviewDecision": review.unwrap_or(""),
            "mergeable": "MERGEABLE", "mergeStateStatus": "BLOCKED",
            "headRefName": "feat", "baseRefName": "main", "headRefOid": HEAD_OID
        })
    }

    fn set_pr(&self, state: &str, draft: bool, review: Option<&str>) {
        self.put("pr.json", &self.pr(state, draft, review).to_string());
    }

    /// Checks: `(name, bucket)`; failing ones are Actions jobs.
    fn set_checks(&self, checks: &[(&str, &str)]) {
        let list: Vec<Value> = checks
            .iter()
            .enumerate()
            .map(|(i, (name, bucket))| {
                json!({
                    "name": name, "state": bucket.to_uppercase(), "bucket": bucket,
                    "link": format!("https://github.com/o/app/actions/runs/11/job/{}", 20 + i),
                    "startedAt": "2026-11-01T10:00:00Z",
                    "completedAt": if *bucket == "pending" { "0001-01-01T00:00:00Z" } else { "2026-11-01T10:01:30Z" },
                    "workflow": "CI", "description": ""
                })
            })
            .collect();
        self.put("checks.json", &Value::Array(list).to_string());
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

struct TestHub {
    _dir: tempfile::TempDir,
    root: PathBuf,
    addr: std::net::SocketAddr,
    hub: Arc<Hub>,
    http: reqwest::Client,
    gh: Gh,
    _stop: oneshot::Sender<()>,
}

impl TestHub {
    async fn start() -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["bin", "log", "gh"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        // main and feat pushed to a bare origin; origin's HEAD is main.
        let repo = root.join("app");
        let bare = root.join("origin.git");
        std::fs::create_dir_all(&repo).unwrap();
        git(
            &root,
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README"), "one\n").unwrap();
        git(&repo, &["add", "README"]);
        git(&repo, &["commit", "-qm", "init"]);
        git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&repo, &["push", "-q", "origin", "main"]);
        git(&repo, &["remote", "set-head", "origin", "main"]);
        git(&repo, &["checkout", "-qb", "feat"]);
        std::fs::write(repo.join("README"), "two\n").unwrap();
        git(&repo, &["commit", "-qam", "fix: retry"]);
        git(&repo, &["push", "-q", "-u", "origin", "feat"]);

        let mut options = HubOptions::new(root.join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = fake_claude(&root.join("bin"), &root.join("log"))
            .to_string_lossy()
            .into_owned();
        options.agents.codex = root.join("bin/no-codex").to_string_lossy().into_owned();
        options.report_command = Some(vec!["/opt/flywheel/bin/fw".into(), "hub".into()]);
        options.claude_config_dir = Some(root.join("claude-config"));
        options.queue_typing_quiet = Duration::from_secs(1);
        let hub = support::open_hub(options);
        let gh_dir = root.join("gh");
        hub.pr_watch().set_tools(Tools {
            git: "git".into(),
            gh: fake_gh(&root.join("bin"), &gh_dir)
                .to_string_lossy()
                .into_owned(),
        });
        // The tests ask; the timer stays out of the way (its own test
        // turns it up).
        hub.pr_watch().set_timing(flywheel_hub::pr::PrTiming {
            tick: Duration::from_secs(3600),
            pending_every: Duration::from_secs(3600),
            open_every: Duration::from_secs(3600),
            settled_every: Duration::from_secs(3600),
            idle_every: Duration::from_secs(3600),
            forced_gap: Duration::ZERO,
            rate_backoff: Duration::from_secs(3600),
            per_minute: 1000,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        tokio::spawn(serve(
            listener,
            Arc::clone(&hub),
            ServerOptions::default(),
            async {
                let _ = stopped.await;
            },
        ));
        Self {
            _dir: dir,
            root,
            addr,
            hub,
            http: reqwest::Client::new(),
            gh: Gh { dir: gh_dir },
            _stop: stop,
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

    async fn get(&self, path: &str) -> Value {
        let (status, value) = self.call(reqwest::Method::GET, path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {value}");
        value
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let (status, value) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert!(status.is_success(), "{path}: {status} {value}");
        value
    }

    /// A claude session in the repository (on `feat`), its branch read.
    async fn agent(&self) -> String {
        let ws = self
            .post(
                "/api/workspaces",
                json!({ "name": "app", "folders": [self.root.join("app")] }),
            )
            .await;
        let session = self
            .post(
                "/api/sessions",
                json!({ "workspace_id": ws["id"], "kind": "claude", "cols": 100, "rows": 30 }),
            )
            .await;
        let id = session["id"].as_str().unwrap().to_owned();
        let deadline = Instant::now() + WAIT;
        while self.session(&id).await["git"]["branch"] != "feat" {
            assert!(Instant::now() < deadline, "the branch was never read");
            sleep(Duration::from_millis(25)).await;
        }
        id
    }

    /// A plain shell session in `agent`'s workspace, its branch read.
    async fn shell(&self, agent: &str) -> String {
        let ws = self.session(agent).await["workspace_id"].clone();
        assert!(ws.is_string(), "no workspace");
        let session = self
            .post(
                "/api/sessions",
                json!({ "workspace_id": ws, "kind": "shell", "cols": 100, "rows": 30 }),
            )
            .await;
        let id = session["id"].as_str().unwrap().to_owned();
        let deadline = Instant::now() + WAIT;
        while self.session(&id).await["git"]["branch"] != "feat" {
            assert!(Instant::now() < deadline, "the branch was never read");
            sleep(Duration::from_millis(25)).await;
        }
        id
    }

    async fn session(&self, id: &str) -> Value {
        self.get("/api/sessions").await["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    }

    async fn view(&self, id: &str) -> Value {
        self.get(&format!("/api/sessions/{id}/pr?refresh=1")).await
    }

    /// Fire a Claude hook the way Claude Code does.
    async fn hook(&self, event: &str) {
        let env_path = self.root.join("log/env");
        let deadline = Instant::now() + WAIT;
        let env = loop {
            if let Ok(text) = std::fs::read_to_string(&env_path)
                && text.contains(report::SECRET_ENV)
            {
                break text;
            }
            assert!(Instant::now() < deadline, "claude never started");
            sleep(Duration::from_millis(25)).await;
        };
        let var = |name: &str| {
            env.lines()
                .find_map(|l| l.strip_prefix(&format!("{name}=")))
                .unwrap()
                .to_owned()
        };
        report::send(
            Path::new(&var(report::SOCKET_ENV)),
            &Report {
                session: var(report::SESSION_ENV),
                secret: var(report::SECRET_ENV),
                source: "claude".into(),
                event: event.into(),
                detail: None,
                payload: None,
            },
        )
        .unwrap();
    }

    async fn status_is(&self, id: &str, want: &str) {
        let deadline = Instant::now() + WAIT;
        while self.session(id).await["agent"]["status"] != want {
            assert!(Instant::now() < deadline, "{id} never became {want}");
            sleep(Duration::from_millis(25)).await;
        }
    }

    fn typed(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(self.root.join("log/in")).unwrap_or_default())
            .into_owned()
    }

    async fn typed_contains(&self, want: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.typed().contains(want) {
            assert!(
                Instant::now() < deadline,
                "never typed {want:?}: {:?}",
                self.typed()
            );
            sleep(Duration::from_millis(25)).await;
        }
    }
}

/// The next plugin lifecycle event `name`'s payload.
async fn lifecycle(events: &mut broadcast::Receiver<HubEvent>, name: &str) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, events.recv()).await {
            Ok(Ok(HubEvent::Lifecycle { event, payload })) if event == name => return payload,
            Ok(Ok(HubEvent::Lifecycle { event, .. })) if event.starts_with("pr.") => {
                panic!("{event} came before {name}")
            }
            Ok(Ok(_) | Err(broadcast::error::RecvError::Lagged(_))) => {}
            other => panic!("no {name} event: {other:?}"),
        }
    }
}

/// No `pr.*` event is waiting.
fn no_pr_event(events: &mut broadcast::Receiver<HubEvent>) {
    while let Ok(event) = events.try_recv() {
        if let HubEvent::Lifecycle { event, .. } = event {
            assert!(!event.starts_with("pr."), "unexpected {event}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gh_missing_or_signed_out_says_why_and_no_pr_is_none() {
    let hub = TestHub::start().await;
    let id = hub.agent().await;

    // No pull request for the branch.
    let view = hub.view(&id).await;
    assert!(view["pr"].is_null(), "{view}");
    assert!(view["problem"].is_null(), "{view}");
    assert_eq!(view["branch"], "feat");
    assert!(hub.session(&id).await.get("pr").is_none());

    // Not signed in.
    hub.gh.flag("auth_fail", true);
    let view = hub.view(&id).await;
    assert_eq!(view["problem"]["kind"], "unauthenticated", "{view}");
    assert!(
        view["problem"]["reason"]
            .as_str()
            .unwrap()
            .contains("gh auth login")
    );
    hub.gh.flag("auth_fail", false);

    // Not installed: nothing runs.
    let calls = hub.gh.calls().len();
    hub.hub.pr_watch().set_tools(Tools {
        git: "git".into(),
        gh: hub.root.join("bin/no-gh").to_string_lossy().into_owned(),
    });
    let view = hub.view(&id).await;
    assert_eq!(view["problem"]["kind"], "missing", "{view}");
    assert!(
        view["problem"]["reason"]
            .as_str()
            .unwrap()
            .contains("not installed")
    );
    assert_eq!(hub.gh.calls().len(), calls);
    let (status, body) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{id}/pr/merge"),
            Some(json!({ "dry_run": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["refusal"].as_str().unwrap().contains("not installed"),
        "{body}"
    );
    // Asking the agent says the real cause too, not "no pull request".
    let (status, body) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{id}/pr/fix"),
            Some(json!({ "dry_run": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.to_string().contains("not installed"), "{body}");

    // On a default branch: GitHub is not asked at all.
    git(&hub.root.join("app"), &["checkout", "-q", "main"]);
    let deadline = Instant::now() + WAIT;
    while hub.session(&id).await["git"]["branch"] != "main" {
        assert!(
            Instant::now() < deadline,
            "the branch change was never read"
        );
        sleep(Duration::from_millis(50)).await;
    }
    let view = hub.view(&id).await;
    assert!(
        view["note"].as_str().unwrap().contains("default branch"),
        "{view}"
    );
    assert!(view["pr"].is_null());
    assert_eq!(hub.gh.calls().len(), calls);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_pr_shows_logs_redacted_and_events_follow_its_changes() {
    let hub = TestHub::start().await;
    let id = hub.agent().await;
    let mut events = hub.hub.subscribe();
    let token = format!("ghp_{}", "aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bC1dE3fG5");
    hub.gh.put(
        "log.txt",
        &format!(
            "build\tRun tests\t2026-11-01T10:00:01.0000000Z running 3 tests\n\
             build\tRun tests\t2026-11-01T10:00:02.0000000Z using GITHUB_TOKEN={token}\n\
             build\tRun tests\t2026-11-01T10:00:03.0000000Z test retry_backs_off ... FAILED\n\
             build\tRun tests\t2026-11-01T10:00:04.0000000Z evil \x1b[201~rm -rf ~ \x1b]52;c;ZWNobw==\x07 \x1b[31mred\x1b[0m\n"
        ),
    );

    // First look: checks running. No event (a first look is not news).
    hub.gh.set_pr("OPEN", false, Some("REVIEW_REQUIRED"));
    hub.gh.set_checks(&[("test", "pending"), ("lint", "pass")]);
    hub.gh.put("checks.exit", "8");
    let view = hub.view(&id).await;
    assert_eq!(view["pr"]["number"], 7, "{view}");
    assert_eq!(view["pr"]["review"], "review_required");
    assert_eq!(view["summary"]["rollup"], "pending");
    assert_eq!(view["checks"][1]["duration_secs"], 90);
    no_pr_event(&mut events);
    // The row's badge.
    let badge = hub.session(&id).await["pr"].clone();
    assert_eq!(badge["number"], 7, "{badge}");
    assert_eq!(badge["state"], "open");
    assert_eq!(badge["checks"]["pending"], 1);

    // Red, seen from a shell on the same branch: the event names it (the
    // session looking), not the first session on the branch.
    let shell = hub.shell(&id).await;
    hub.gh.set_checks(&[("test", "fail"), ("lint", "pass")]);
    hub.gh.put("checks.exit", "1");
    let view = hub.view(&shell).await;
    assert_eq!(view["summary"]["failing"], 1, "{view}");
    let failed = lifecycle(&mut events, "pr.checks_failed").await;
    assert_eq!(failed["session"]["id"], shell.as_str());
    hub.gh.set_checks(&[("test", "pending"), ("lint", "pass")]);
    hub.gh.put("checks.exit", "8");
    hub.view(&id).await;
    // Red again, from the agent's view.
    hub.gh.set_checks(&[("test", "fail"), ("lint", "pass")]);
    hub.gh.put("checks.exit", "1");
    let view = hub.view(&id).await;
    assert_eq!(view["summary"]["failing"], 1, "{view}");
    assert_eq!(view["checks"][0]["job_id"], 20);
    let failed = lifecycle(&mut events, "pr.checks_failed").await;
    assert_eq!(failed["pr"]["number"], 7);
    assert_eq!(failed["failing"], json!(["test"]));
    assert_eq!(failed["session"]["id"], id.as_str());
    assert_eq!(hub.session(&id).await["pr"]["checks"]["failing"], 1);

    // The failed log: tidied, its token redacted.
    let log = hub
        .get(&format!("/api/sessions/{id}/pr/log?check=test"))
        .await;
    let text = log["text"].as_str().unwrap();
    assert!(!text.contains(&token), "{text}");
    assert!(text.contains("GITHUB_TOKEN=[redacted]"), "{text}");
    assert!(text.contains("== build / Run tests =="), "{text}");
    assert!(text.contains("retry_backs_off ... FAILED"));
    // Terminal control from the log never reaches a terminal or an agent.
    assert!(!text.contains('\x1b') && !text.contains('\x07'), "{text:?}");
    assert!(text.contains("evil rm -rf ~  red"), "{text:?}");
    assert_eq!(log["command"], "gh run view --job 20 --log-failed");
    assert!(
        hub.gh
            .calls()
            .iter()
            .any(|c| c == "run view --job 20 --log-failed")
    );
    let (status, _) = hub
        .call(
            reqwest::Method::GET,
            &format!("/api/sessions/{id}/pr/log?check=nope"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Green, then changes requested, then merged.
    hub.gh.set_checks(&[("test", "pass"), ("lint", "pass")]);
    hub.gh.put("checks.exit", "0");
    hub.view(&id).await;
    let passed = lifecycle(&mut events, "pr.checks_passed").await;
    assert_eq!(passed["checks"]["passing"], 2);
    hub.gh.set_pr("OPEN", false, Some("CHANGES_REQUESTED"));
    hub.view(&id).await;
    lifecycle(&mut events, "pr.review_changes_requested").await;
    hub.gh.set_pr("MERGED", false, Some("APPROVED"));
    let view = hub.view(&id).await;
    assert_eq!(view["pr"]["state"], "merged");
    let merged = lifecycle(&mut events, "pr.merged").await;
    assert_eq!(merged["pr"]["base"], "main");
    assert_eq!(hub.session(&id).await["pr"]["state"], "merged");

    // Each became an activity row.
    let deadline = Instant::now() + WAIT;
    loop {
        let page = hub.get("/api/activity?kind=pr&limit=50").await;
        let texts: Vec<&str> = page["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["text"].as_str())
            .collect();
        if texts.len() >= 4 {
            assert!(texts.contains(&"CI failed on PR #7: test"), "{texts:?}");
            assert!(
                texts.contains(&"CI passed on PR #7 (2 checks)"),
                "{texts:?}"
            );
            assert!(texts.contains(&"changes requested on PR #7"), "{texts:?}");
            assert!(texts.contains(&"PR #7 merged into main"), "{texts:?}");
            break;
        }
        assert!(Instant::now() < deadline, "rows: {page}");
        sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn asking_the_agent_to_fix_ci_queues_while_it_works_and_pastes_at_rest() {
    let hub = TestHub::start().await;
    let id = hub.agent().await;
    // Built at runtime so secret scanners do not flag a fake fixture.
    let token = &["xoxb", "1234567890", "abcdefghijklmnop"].join("-");
    hub.gh.put(
        "log.txt",
        &format!(
            "build\tRun tests\t2026-11-01T10:00:02.0000000Z slack token {token}\n\
             build\tRun tests\t2026-11-01T10:00:03.0000000Z assertion failed: left == right\n"
        ),
    );
    hub.gh.set_pr("OPEN", false, None);
    hub.gh.set_checks(&[("test", "fail"), ("lint", "pass")]);
    hub.view(&id).await;

    // Dry run: the prompt and who gets it, nothing typed.
    let plan = hub
        .post(
            &format!("/api/sessions/{id}/pr/fix"),
            json!({ "dry_run": true }),
        )
        .await;
    let prompt = plan["prompt"].as_str().unwrap().to_owned();
    assert_eq!(plan["check"], "test");
    assert_eq!(plan["agent"]["id"], id.as_str());
    assert!(plan.get("delivery").is_none());
    assert!(
        prompt.starts_with("From fw hub: CI failed on pull request #7 (https://github.com/o/app/pull/7), branch feat at 0123456789."),
        "{prompt}"
    );
    assert!(prompt.contains("Failing checks: test (CI)."), "{prompt}");
    assert!(prompt.contains("assertion failed: left == right"));
    assert!(!prompt.contains(token), "{prompt}");
    assert!(prompt.contains("secrets redacted by the hub"));

    // Working: queued, typed once the turn is done.
    hub.hook("UserPromptSubmit").await;
    hub.status_is(&id, "working").await;
    let sent = hub
        .post(&format!("/api/sessions/{id}/pr/fix"), json!({}))
        .await;
    assert_eq!(sent["delivery"]["delivery"], "queued", "{sent}");
    assert!(
        sent["delivery"]["why"]
            .as_str()
            .unwrap()
            .contains("working")
    );
    assert_eq!(hub.session(&id).await["queue"]["pending"], 1);
    sleep(Duration::from_millis(500)).await;
    assert!(!hub.typed().contains("CI failed"), "typed while working");
    hub.hook("Stop").await;
    hub.typed_contains("From fw hub: CI failed on pull request #7")
        .await;
    assert!(!hub.typed().contains(token));

    // At rest (its turn on the queued ask done, settled): pasted now.
    let deadline = Instant::now() + WAIT;
    while hub.session(&id).await["queue"]["pending"] != 0 {
        assert!(Instant::now() < deadline, "the delivery was never recorded");
        sleep(Duration::from_millis(25)).await;
    }
    sleep(Duration::from_millis(100)).await;
    hub.hook("UserPromptSubmit").await;
    hub.hook("Stop").await;
    hub.status_is(&id, "done").await;
    sleep(Duration::from_millis(1800)).await;
    let before = hub.typed().matches("From fw hub").count();
    let sent = hub
        .post(
            &format!("/api/sessions/{id}/pr/fix"),
            json!({ "check": "test" }),
        )
        .await;
    assert_eq!(sent["delivery"]["delivery"], "pasted", "{sent}");
    let deadline = Instant::now() + WAIT;
    while hub.typed().matches("From fw hub").count() == before {
        assert!(Instant::now() < deadline, "never pasted");
        sleep(Duration::from_millis(25)).await;
    }
    // The same ask again (same check, same head commit): not pasted twice.
    let (status, body) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{id}/pr/fix"),
            Some(json!({ "check": "test" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("asked to fix test"), "{body}");

    // A passing check is nothing to fix.
    let (status, body) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{id}/pr/fix"),
            Some(json!({ "check": "lint", "dry_run": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rerun_ready_and_merge_run_as_planned_and_merge_refuses_red_unless_forced() {
    let hub = TestHub::start().await;
    let id = hub.agent().await;
    let mut events = hub.hub.subscribe();
    hub.gh.put(
        "repo.json",
        r#"{"mergeCommitAllowed":false,"squashMergeAllowed":true,"rebaseMergeAllowed":true,"viewerDefaultMergeMethod":"MERGE"}"#,
    );
    hub.gh.set_pr("OPEN", true, None);
    hub.gh.set_checks(&[("test", "fail"), ("lint", "pass")]);
    hub.view(&id).await;
    let path = |action: &str| format!("/api/sessions/{id}/pr/{action}");

    // Ready: a draft.
    let plan = hub.post(&path("ready"), json!({ "dry_run": true })).await;
    assert!(plan["refusal"].is_null(), "{plan}");
    assert!(
        plan["commands"][0]
            .as_str()
            .unwrap()
            .ends_with("gh pr ready 7)"),
        "{plan}"
    );
    // Merge of a draft: refused outright.
    let merge = hub.post(&path("merge"), json!({ "dry_run": true })).await;
    assert!(
        merge["refusal"].as_str().unwrap().contains("draft"),
        "{merge}"
    );
    assert_eq!(merge["forcible"], false);
    let done = hub
        .post(&path("ready"), json!({ "confirm": plan["fingerprint"] }))
        .await;
    assert_eq!(done["ok"], true, "{done}");
    assert!(hub.gh.calls().iter().any(|c| c == "pr ready 7"));

    // Re-run the failed run.
    hub.gh.set_pr("OPEN", false, None);
    let plan = hub.post(&path("rerun"), json!({ "dry_run": true })).await;
    assert!(plan["refusal"].is_null(), "{plan}");
    assert_eq!(plan["details"], json!(["test"]));
    // A stale fingerprint runs nothing.
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &path("rerun"),
            Some(json!({ "confirm": "0000" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(!hub.gh.calls().iter().any(|c| c.starts_with("run rerun")));
    let done = hub
        .post(&path("rerun"), json!({ "confirm": plan["fingerprint"] }))
        .await;
    assert_eq!(done["ok"], true, "{done}");
    assert!(hub.gh.calls().iter().any(|c| c == "run rerun 11 --failed"));

    // Merge while a check fails: refused, forcible by the number.
    let plan = hub.post(&path("merge"), json!({ "dry_run": true })).await;
    assert_eq!(plan["forcible"], true, "{plan}");
    assert_eq!(plan["force_word"], "7");
    assert_eq!(plan["method"], "squash");
    assert!(
        plan["refusal"]
            .as_str()
            .unwrap()
            .contains("1 check failing (test)"),
        "{plan}"
    );
    assert!(
        plan["commands"][0].as_str().unwrap().ends_with(&format!(
            "gh pr merge 7 --squash --match-head-commit {HEAD_OID})"
        )),
        "{plan}"
    );
    for force in [None, Some("8")] {
        let (status, body) = hub
            .call(
                reqwest::Method::POST,
                &path("merge"),
                Some(json!({ "confirm": plan["fingerprint"], "force": force })),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }
    assert!(!hub.gh.calls().iter().any(|c| c.starts_with("pr merge")));
    let mut merged = hub.gh.pr("MERGED", false, None);
    merged["mergeStateStatus"] = json!("UNKNOWN");
    hub.gh.put("merged.json", &merged.to_string());
    let done = hub
        .post(
            &path("merge"),
            json!({ "confirm": plan["fingerprint"], "force": "7" }),
        )
        .await;
    assert_eq!(done["ok"], true, "{done}");
    assert_eq!(done["pr"]["state"], "merged");
    assert!(
        hub.gh
            .calls()
            .iter()
            .any(|c| c == &format!("pr merge 7 --squash --match-head-commit {HEAD_OID}"))
    );
    lifecycle(&mut events, "pr.merged").await;
    // Merged: nothing more to merge.
    let plan = hub.post(&path("merge"), json!({ "dry_run": true })).await;
    assert!(
        plan["refusal"].as_str().unwrap().contains("merged already"),
        "{plan}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rate_limit_pauses_asking_and_the_timer_refreshes_on_its_own() {
    let hub = TestHub::start().await;
    let id = hub.agent().await;
    hub.gh.set_pr("OPEN", false, None);
    hub.gh.set_checks(&[("test", "pending")]);
    hub.gh.put("checks.exit", "8");
    hub.view(&id).await;

    hub.gh.flag("rate_limited", true);
    let view = hub.view(&id).await;
    assert_eq!(view["problem"]["kind"], "rate_limited", "{view}");
    // What was known stays shown, marked stale.
    assert_eq!(view["pr"]["number"], 7);
    assert!(hub.session(&id).await["pr"]["stale"].is_string());
    // Paused: asking again calls nothing.
    let calls = hub.gh.calls().len();
    let view = hub.view(&id).await;
    assert_eq!(view["problem"]["kind"], "rate_limited");
    assert!(
        view["problem"]["reason"]
            .as_str()
            .unwrap()
            .contains("asks again")
    );
    assert_eq!(hub.gh.calls().len(), calls);
    // Nothing is acted on while GitHub cannot be read again.
    hub.gh.put(
        "repo.json",
        r#"{"mergeCommitAllowed":true,"squashMergeAllowed":false,"rebaseMergeAllowed":false}"#,
    );
    let plan = hub
        .post(
            &format!("/api/sessions/{id}/pr/merge"),
            json!({ "dry_run": true }),
        )
        .await;
    let (status, body) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{id}/pr/merge"),
            Some(json!({ "confirm": plan["fingerprint"], "force": "7" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("could not be read"), "{body}");
    assert!(!hub.gh.calls().iter().any(|c| c.starts_with("pr merge")));

    // A fresh hub with a quick timer: the badge comes on its own.
    let hub = TestHub::start().await;
    hub.gh.set_pr("OPEN", false, None);
    hub.gh.set_checks(&[("test", "pending")]);
    hub.gh.put("checks.exit", "8");
    let id = hub.agent().await;
    hub.hub.pr_watch().set_timing(flywheel_hub::pr::PrTiming {
        tick: Duration::from_millis(100),
        pending_every: Duration::from_millis(200),
        ..flywheel_hub::pr::PrTiming::default()
    });
    let deadline = Instant::now() + WAIT;
    while hub.session(&id).await["pr"]["checks"]["pending"] != 1 {
        assert!(Instant::now() < deadline, "the timer never asked");
        sleep(Duration::from_millis(50)).await;
    }
    // Checks running: asked again soon, without anyone asking.
    hub.gh.set_checks(&[("test", "pass")]);
    hub.gh.put("checks.exit", "0");
    while hub.session(&id).await["pr"]["checks"]["passing"] != 1 {
        assert!(Instant::now() < deadline, "the timer never asked again");
        sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix_asks_are_queued_once_and_dropped_when_ci_turns_green() {
    let hub = TestHub::start().await;
    let id = hub.agent().await;
    hub.gh.put(
        "log.txt",
        "build\tRun tests\t2026-11-01T10:00:03.0000000Z assertion failed\n",
    );
    hub.gh.set_pr("OPEN", false, None);
    hub.gh.set_checks(&[("test", "fail"), ("lint", "pass")]);
    hub.view(&id).await;
    hub.hook("UserPromptSubmit").await;
    hub.status_is(&id, "working").await;

    // Asked twice while it works: one queued ask, the newer text.
    let first = hub
        .post(&format!("/api/sessions/{id}/pr/fix"), json!({}))
        .await;
    assert_eq!(first["delivery"]["delivery"], "queued", "{first}");
    let second = hub
        .post(&format!("/api/sessions/{id}/pr/fix"), json!({}))
        .await;
    assert_eq!(second["delivery"]["delivery"], "queued", "{second}");
    assert!(
        second["delivery"]["why"]
            .as_str()
            .unwrap()
            .contains("replaces"),
        "{second}"
    );
    assert_eq!(second["delivery"]["item"], first["delivery"]["item"]);
    assert_eq!(hub.session(&id).await["queue"]["pending"], 1);

    // CI goes green before the turn ends: the stale ask is dropped.
    hub.gh.set_checks(&[("test", "pass"), ("lint", "pass")]);
    hub.gh.put("checks.exit", "0");
    hub.view(&id).await;
    let deadline = Instant::now() + WAIT;
    // (An empty queue has no brief at all.)
    while hub.session(&id).await["queue"]["pending"]
        .as_u64()
        .unwrap_or(0)
        != 0
    {
        assert!(
            Instant::now() < deadline,
            "the stale ask stayed queued: {}",
            hub.get(&format!("/api/sessions/{id}/queue")).await
        );
        sleep(Duration::from_millis(25)).await;
    }
    hub.hook("Stop").await;
    sleep(Duration::from_millis(1500)).await;
    assert!(!hub.typed().contains("CI failed"), "a stale ask was typed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pruned_head_branch_keeps_its_pr_and_the_merge_is_announced() {
    let hub = TestHub::start().await;
    let id = hub.agent().await;
    let mut events = hub.hub.subscribe();
    hub.gh.set_pr("OPEN", false, Some("APPROVED"));
    hub.gh.set_checks(&[("test", "pass")]);
    hub.view(&id).await;
    assert_eq!(hub.session(&id).await["pr"]["state"], "open");

    // Merged on GitHub, which deleted the head branch; a prune dropped the
    // remote-tracking ref before the hub saw the merge.
    hub.gh.set_pr("MERGED", false, Some("APPROVED"));
    git(
        &hub.root.join("app"),
        &["update-ref", "-d", "refs/remotes/origin/feat"],
    );
    let view = hub.view(&id).await;
    assert_eq!(view["pr"]["state"], "merged", "{view}");
    assert!(view["note"].is_null(), "{view}");
    lifecycle(&mut events, "pr.merged").await;
    assert_eq!(hub.session(&id).await["pr"]["state"], "merged");
}
