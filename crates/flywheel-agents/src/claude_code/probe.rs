//! Installation and authentication probe for Claude Code.
//!
//! Evidence-based, never definitive about billing: an OAuth token or
//! credentials file is a strong indication of account-session auth; an API
//! key indicates API-backed auth; both present is mixed (the CLI's own
//! precedence prefers the API key, which is exactly the silent-subscription
//! strand the container backend's auth guard exists to catch). Evidence
//! strings carry names and paths only, never secret values.

use crate::models::{AgentProbeResult, AssuranceLevel, AuthenticationKind};
use crate::probe::{ProbeEnvironment, find_executable, read_version};

pub const OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";
pub const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
pub const EXECUTABLE: &str = "claude";

pub async fn probe_claude_code() -> AgentProbeResult {
    probe_claude_code_in(&ProbeEnvironment::from_process()).await
}

pub async fn probe_claude_code_in(env: &ProbeEnvironment) -> AgentProbeResult {
    let executable = find_executable(EXECUTABLE, env.path.as_deref());
    let config_dir = env.home.as_ref().map(|home| home.join(".claude"));
    let credentials = config_dir.as_ref().map(|dir| dir.join(".credentials.json"));

    let mut evidence = Vec::new();
    let mut warnings = Vec::new();
    let mut has_account = false;
    let mut has_api_key = false;

    if env.var(OAUTH_TOKEN_ENV).is_some() {
        has_account = true;
        evidence.push(format!("{OAUTH_TOKEN_ENV} is set"));
    }
    if let Some(credentials) = credentials.as_ref().filter(|p| p.is_file()) {
        has_account = true;
        evidence.push(format!("{} exists", credentials.display()));
    }
    if env.var(API_KEY_ENV).is_some() {
        has_api_key = true;
        evidence.push(format!("{API_KEY_ENV} is set"));
    }

    let Some(executable) = executable else {
        return AgentProbeResult::not_installed("claude executable not found on PATH", evidence);
    };

    let version = read_version(&executable).await;
    if version.is_none() {
        warnings.push("claude --version failed; version unknown".to_string());
    }

    let kind = match (has_account, has_api_key) {
        (true, true) => {
            warnings.push(format!(
                "both account credentials and {API_KEY_ENV} present; the CLI prefers the API key, which strands the subscription"
            ));
            AuthenticationKind::Mixed
        }
        (true, false) => AuthenticationKind::AccountSession,
        (false, true) => AuthenticationKind::ApiKey,
        (false, false) => AuthenticationKind::InstallationOnly,
    };

    AgentProbeResult {
        installed: true,
        executable_path: Some(executable),
        version,
        authentication_kind: kind,
        authentication_assurance: if kind == AuthenticationKind::InstallationOnly {
            AssuranceLevel::BestEffort
        } else {
            AssuranceLevel::StrongIndication
        },
        authentication_evidence: evidence,
        config_paths: config_dir.into_iter().filter(|dir| dir.is_dir()).collect(),
        warnings,
    }
}
