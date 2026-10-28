//! Shared fixtures: deterministic fake vendor CLIs written to a tempdir at
//! test time (POSIX sh scripts), plus request builders.
#![allow(dead_code)]

use flywheel_agents::{AgentConfiguration, AuthenticationPolicy, PermissionPolicy, RunRequest};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Whether `pid` is gone (or a zombie awaiting its reaper).
#[cfg(unix)]
pub fn process_dead(pid: i32) -> bool {
    // SAFETY: kill with signal 0 only probes for existence.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    if !alive {
        return true;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .map(|rest| rest.split_whitespace().next() == Some("Z"))
            .unwrap_or(true),
        Err(_) => true,
    }
}

/// Poll until `pid` is dead, up to `within`; the final verdict.
#[cfg(unix)]
pub async fn wait_dead(pid: i32, within: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline && !process_dead(pid) {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    process_dead(pid)
}

/// Poll for a pid written to `path`, up to `within`.
pub async fn read_pid_file(path: &Path, within: std::time::Duration) -> i32 {
    let deadline = std::time::Instant::now() + within;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no pid written to {}",
            path.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

pub fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write fixture script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fixture");
    }
    path
}

/// Fake Claude Code CLI: stream-json lines selected by FAKE_AGENT_SCENARIO.
/// The first stdin line (the stream-json user message, or the piped text
/// prompt) is consumed before any output. argv flags are ignored except
/// that the `approval` scenario, like the real CLI, sends `can_use_tool`
/// only under `--permission-prompt-tool stdio`.
pub const FAKE_CLAUDE: &str = r#"#!/bin/sh
IFS= read -r FIRST_LINE || true
emit() { printf '%s\n' "$1"; }
init() { emit '{"type":"system","subtype":"init","session_id":"sess-1","model":"claude-fake-1"}'; }
text() { emit '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$1"'"}],"stop_reason":"end_turn"}}'; }
result() { emit '{"type":"result","subtype":"'"$1"'","is_error":'"$2"',"result":"'"$3"'","num_turns":3,"total_cost_usd":0.0125}'; }
extract() { printf '%s' "$1" | sed -n 's/.*"'"$2"'":"\([^"]*\)".*/\1/p'; }
case "${FAKE_AGENT_SCENARIO:-happy}" in
happy)
  init
  emit '{"type":"assistant","message":{"content":[{"type":"text","text":"Hello, "}],"stop_reason":"end_turn","usage":{"input_tokens":11,"output_tokens":4,"cache_creation_input_tokens":1,"cache_read_input_tokens":2}}}'
  emit '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-1","name":"Bash","input":{"command":"echo hi"}}],"stop_reason":"tool_use"}}'
  emit '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"hi","is_error":false}]}}'
  text "world."
  emit '{"type":"result","subtype":"success","is_error":false,"result":"Hello, world.","num_turns":3,"total_cost_usd":0.0125,"usage":{"input_tokens":100,"output_tokens":25,"cache_creation_input_tokens":7,"cache_read_input_tokens":3}}'
  ;;
tool_error)
  init
  emit '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-err","name":"Bash","input":{"command":"false"}}],"stop_reason":"tool_use"}}'
  emit '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-err","content":"command failed","is_error":true}]}}'
  result success false "tool failed"
  ;;
malformed)
  init
  text "before garbage "
  emit 'this line is not json'
  emit '{not json'
  emit '[1,2,3]'
  emit '{"type":"mystery","x":1}'
  text "after garbage"
  result success false "survived"
  ;;
crash)
  init
  text "about to crash"
  echo "fake-agent: fatal explosion" >&2
  exit 3
  ;;
hang)
  init
  sleep 300
  ;;
grandchild)
  init
  (trap '' TERM; exec sleep 300) &
  echo $! > grandchild.pid
  text "spawned"
  sleep 300
  ;;
stderr)
  echo "FAKE-AGENT-STDERR-MARKER" >&2
  init
  text "quiet run"
  result success false "quiet run"
  ;;
cwd)
  init
  text "$(pwd)"
  result success false "cwd reported"
  ;;
env)
  init
  text "marker=${TEST_MARKER:-}; api_key=${ANTHROPIC_API_KEY-unset}"
  result success false "environment reported"
  ;;
usage_limit)
  init
  text "Claude AI usage limit reached|1751990400"
  result error_during_execution true "Claude AI usage limit reached|1751990400"
  ;;
no_result)
  init
  text "partial work"
  ;;
rate_limit)
  init
  emit '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1751990400,"rateLimitType":"five_hour"},"uuid":"u-1","session_id":"sess-1"}'
  text "throttled but fine"
  result success false "throttled but fine"
  ;;
interrupt)
  init
  text "working"
  IFS= read -r CONTROL || true
  REQ=$(extract "$CONTROL" request_id)
  SUB=$(extract "$CONTROL" subtype)
  emit '{"type":"control_response","response":{"subtype":"success","request_id":"'"$REQ"'","response":{"ok":true}}}'
  text "control:$SUB"
  result success false "acknowledged"
  ;;
control_error)
  init
  text "working"
  IFS= read -r CONTROL || true
  REQ=$(extract "$CONTROL" request_id)
  emit '{"type":"control_response","response":{"subtype":"error","request_id":"'"$REQ"'","error":"not now"}}'
  text "rejected"
  result success false "rejected"
  ;;
approval)
  init
  # The real CLI sends can_use_tool only under --permission-prompt-tool
  # stdio; otherwise it decides the prompt itself.
  case " $* " in
    *" --permission-prompt-tool stdio "*) ;;
    *) result success false "approval:not-routed"; exit 0 ;;
  esac
  emit '{"type":"control_request","request_id":"creq-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf build"},"tool_use_id":"tu-9"}}'
  IFS= read -r ANSWER || true
  BEHAVIOR=$(extract "$ANSWER" behavior)
  result success false "approval:$BEHAVIOR"
  ;;
inject)
  init
  text "ready"
  IFS= read -r SECOND || true
  text "$(extract "$SECOND" content)"
  result success false "injected"
  ;;
first_line)
  init
  text "$(extract "$FIRST_LINE" content)"
  result success false "echoed"
  ;;
*)
  echo "fake_agent: unknown scenario ${FAKE_AGENT_SCENARIO}" >&2
  exit 64
  ;;
esac
"#;

/// Fake codex CLI: `codex exec --json` JSONL selected by FAKE_CODEX_SCENARIO.
pub const FAKE_CODEX: &str = r#"#!/bin/sh
cat >/dev/null
emit() { printf '%s\n' "$1"; }
thread() { emit '{"type":"thread.started","thread_id":"codex-1"}'; }
msg() { emit '{"type":"item.completed","item":{"type":"agent_message","id":"'"$1"'","text":"'"$2"'"}}'; }
USAGE='"usage":{"input_tokens":11,"cached_input_tokens":4,"output_tokens":3}'
case "${FAKE_CODEX_SCENARIO:-happy}" in
happy)
  thread
  emit '{"type":"turn.started"}'
  msg msg-1 "Hello from codex. "
  emit '{"type":"item.started","item":{"type":"command_execution","id":"cmd-1","command":"echo hi"}}'
  emit '{"type":"item.completed","item":{"type":"command_execution","id":"cmd-1","command":"echo hi","aggregated_output":"hi\n","exit_code":0,"status":"completed"}}'
  emit '{"type":"item.completed","item":{"type":"file_change","id":"fc-1","status":"completed","changes":[{"path":"src/app.py","kind":"update"}]}}'
  emit '{"type":"item.completed","item":{"type":"todo_list","id":"todo-1","items":[{"text":"ship it","completed":true}]}}'
  msg msg-2 "All done."
  emit '{"type":"turn.completed",'"$USAGE"'}'
  ;;
tool_error)
  thread
  emit '{"type":"turn.started"}'
  emit '{"type":"item.started","item":{"type":"command_execution","id":"cmd-err","command":"false"}}'
  emit '{"type":"item.completed","item":{"type":"command_execution","id":"cmd-err","command":"false","aggregated_output":"boom","exit_code":2,"status":"failed"}}'
  emit '{"type":"turn.completed"}'
  ;;
turn_failed)
  thread
  emit '{"type":"turn.started"}'
  msg msg-1 "partial answer"
  emit '{"type":"turn.failed","error":{"message":"model stream disconnected"}}'
  ;;
malformed)
  thread
  msg msg-1 "before garbage "
  emit 'this line is not json'
  emit '{not json'
  emit '[1,2,3]'
  emit '{"type":"mystery","x":1}'
  emit '{"type":"item.completed","item":{"type":"quantum_flux","id":"q-1"}}'
  msg msg-2 "after garbage"
  emit '{"type":"turn.completed",'"$USAGE"'}'
  ;;
crash)
  thread
  echo "fake-codex: fatal explosion" >&2
  exit 3
  ;;
envelope)
  thread
  emit '{"type":"item.completed","item":{"type":"agent_message","id":"msg-1","text":"Work is committed and ready for grading.\n<!-- LOOP_STATUS -->\n{\"intent\": \"verify\", \"reason\": \"implementation complete\"}\n<!-- /LOOP_STATUS -->"}}'
  emit '{"type":"turn.completed",'"$USAGE"'}'
  ;;
cwd)
  thread
  msg msg-1 "$(pwd)"
  emit '{"type":"turn.completed"}'
  ;;
env)
  thread
  msg msg-1 "marker=${TEST_MARKER:-}; api_key=${OPENAI_API_KEY-unset}"
  emit '{"type":"turn.completed"}'
  ;;
*)
  echo "fake_codex: unknown scenario ${FAKE_CODEX_SCENARIO}" >&2
  exit 64
  ;;
esac
"#;

pub fn fake_claude(dir: &Path) -> PathBuf {
    write_script(dir, "fake_claude.sh", FAKE_CLAUDE)
}

pub fn fake_codex(dir: &Path) -> PathBuf {
    write_script(dir, "fake_codex.sh", FAKE_CODEX)
}

pub struct ScenarioBuilder {
    pub agent_id: String,
    pub script: PathBuf,
    pub scenario_var: String,
    pub scenario: String,
    pub working_directory: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub timeout_seconds: Option<f64>,
    pub authentication_policy: AuthenticationPolicy,
    pub permission_policy: PermissionPolicy,
    pub adapter_options: BTreeMap<String, Value>,
}

impl ScenarioBuilder {
    pub fn claude(script: PathBuf, scenario: &str, working_directory: PathBuf) -> Self {
        Self::new(
            "claude-code",
            script,
            "FAKE_AGENT_SCENARIO",
            scenario,
            working_directory,
        )
    }

    pub fn codex(script: PathBuf, scenario: &str, working_directory: PathBuf) -> Self {
        Self::new(
            "codex",
            script,
            "FAKE_CODEX_SCENARIO",
            scenario,
            working_directory,
        )
    }

    fn new(
        agent_id: &str,
        script: PathBuf,
        scenario_var: &str,
        scenario: &str,
        working_directory: PathBuf,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            script,
            scenario_var: scenario_var.into(),
            scenario: scenario.into(),
            working_directory,
            environment: BTreeMap::new(),
            timeout_seconds: None,
            authentication_policy: AuthenticationPolicy::AccountPreferred,
            permission_policy: PermissionPolicy::Auto,
            adapter_options: BTreeMap::new(),
        }
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    pub fn timeout(mut self, seconds: f64) -> Self {
        self.timeout_seconds = Some(seconds);
        self
    }

    pub fn auth(mut self, policy: AuthenticationPolicy) -> Self {
        self.authentication_policy = policy;
        self
    }

    pub fn option(mut self, key: &str, value: Value) -> Self {
        self.adapter_options.insert(key.into(), value);
        self
    }

    pub fn request(self) -> RunRequest {
        let mut configuration = AgentConfiguration::new(self.agent_id);
        configuration.permission_policy = self.permission_policy;
        configuration.authentication_policy = self.authentication_policy;
        // Run fixtures through sh rather than exec-ing the freshly written
        // file: parallel spawns can briefly hold the write fd (ETXTBSY).
        configuration.command_override = Some(vec![
            "/bin/sh".into(),
            self.script.to_string_lossy().into_owned(),
        ]);
        configuration.environment = self.environment;
        configuration
            .environment
            .insert(self.scenario_var, self.scenario);
        configuration.adapter_options = self.adapter_options;
        RunRequest {
            prompt: "exercise the fake agent".into(),
            working_directory: self.working_directory,
            configuration,
            timeout_seconds: self.timeout_seconds,
        }
    }
}
