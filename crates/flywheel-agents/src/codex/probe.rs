//! Installation and authentication probe for the OpenAI Codex CLI.
//!
//! Evidence-based, never definitive about billing: `~/.codex/auth.json` is
//! a strong indication of account-session auth; `OPENAI_API_KEY` indicates
//! API-backed auth; both present is mixed (the API key may shadow the
//! account session). Evidence strings carry names and paths only.

use crate::models::{AgentProbeResult, AssuranceLevel, AuthenticationKind};
use crate::probe::{ProbeEnvironment, find_executable, read_version};

pub const API_KEY_ENV: &str = "OPENAI_API_KEY";
pub const EXECUTABLE: &str = "codex";

pub async fn probe_codex() -> AgentProbeResult {
    probe_codex_in(&ProbeEnvironment::from_process()).await
}

pub async fn probe_codex_in(env: &ProbeEnvironment) -> AgentProbeResult {
    let executable = find_executable(EXECUTABLE, env.path.as_deref());
    let config_dir = env.home.as_ref().map(|home| home.join(".codex"));
    let auth_file = config_dir.as_ref().map(|dir| dir.join("auth.json"));

    let mut evidence = Vec::new();
    let mut warnings = Vec::new();
    let mut has_account = false;
    let mut has_api_key = false;

    if let Some(auth_file) = auth_file.as_ref().filter(|p| p.is_file()) {
        has_account = true;
        evidence.push(format!("{} exists", auth_file.display()));
    }
    if env.var(API_KEY_ENV).is_some() {
        has_api_key = true;
        evidence.push(format!("{API_KEY_ENV} is set"));
    }

    let Some(executable) = executable else {
        return AgentProbeResult::not_installed("codex executable not found on PATH", evidence);
    };

    let version = read_version(&executable).await;
    if version.is_none() {
        warnings.push("codex --version failed; version unknown".to_string());
    }

    let kind = match (has_account, has_api_key) {
        (true, true) => {
            warnings.push(format!(
                "both auth.json account credentials and {API_KEY_ENV} present; the API key may shadow the account session"
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
