//! Installation/authentication probes against a temp PATH holding a fake
//! executable and a temp HOME, so no real credentials are inspected.

mod common;

use flywheel_agents::claude_code::probe_claude_code_in;
use flywheel_agents::codex::probe_codex_in;
use flywheel_agents::{AssuranceLevel, AuthenticationKind, ProbeEnvironment};
use std::path::{Path, PathBuf};

fn environment(bin: Option<&Path>, home: &Path) -> ProbeEnvironment {
    ProbeEnvironment {
        path: bin.map(|b| b.to_string_lossy().into_owned()),
        home: Some(home.to_path_buf()),
        vars: Default::default(),
    }
}

/// Install `name` as a symlink to a system binary (never exec a freshly
/// written file: parallel spawns can briefly hold its write fd). The target
/// must not dispatch on argv[0]: multicall coreutils (uutils, the default on
/// Ubuntu 26.04) refuse to run under the fake name, so `env` cannot stand in.
/// `bash --version` prints a version line and exits 0 under any name; `false`
/// exits non-zero either way.
fn install(dir: &Path, name: &str, target: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let target = ["/usr/bin/", "/bin/"]
        .iter()
        .map(|prefix| PathBuf::from(format!("{prefix}{target}")))
        .find(|p| p.exists())
        .expect("system binary present");
    std::os::unix::fs::symlink(target, bin.join(name)).unwrap();
    bin
}

#[tokio::test]
async fn claude_probe_not_installed() {
    let dir = tempfile::tempdir().unwrap();
    let result = probe_claude_code_in(&environment(None, dir.path())).await;
    assert!(!result.installed);
    assert_eq!(result.executable_path, None);
    assert_eq!(result.authentication_kind, AuthenticationKind::Unknown);
    assert!(result.warnings.iter().any(|w| w.contains("not found")));
}

#[tokio::test]
async fn claude_probe_installation_only_reads_version() {
    let dir = tempfile::tempdir().unwrap();
    let bin = install(dir.path(), "claude", "bash");
    let result = probe_claude_code_in(&environment(Some(&bin), dir.path())).await;
    assert!(result.installed);
    assert_eq!(result.executable_path, Some(bin.join("claude")));
    assert!(
        result
            .version
            .as_deref()
            .is_some_and(|v| v.contains("bash")),
        "{:?}",
        result.version
    );
    assert_eq!(
        result.authentication_kind,
        AuthenticationKind::InstallationOnly
    );
    assert_eq!(result.authentication_assurance, AssuranceLevel::BestEffort);
    assert!(result.config_paths.is_empty());
    assert!(result.warnings.is_empty());
}

#[tokio::test]
async fn claude_probe_version_failure_is_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let bin = install(dir.path(), "claude", "false");
    let result = probe_claude_code_in(&environment(Some(&bin), dir.path())).await;
    assert!(result.installed);
    assert_eq!(result.version, None);
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("--version failed"))
    );
}

#[tokio::test]
async fn claude_probe_oauth_token_means_account_session() {
    let dir = tempfile::tempdir().unwrap();
    let bin = install(dir.path(), "claude", "bash");
    let mut env = environment(Some(&bin), dir.path());
    env.vars.insert(
        "CLAUDE_CODE_OAUTH_TOKEN".into(),
        "sk-test-oauth-secret".into(),
    );
    let result = probe_claude_code_in(&env).await;
    assert_eq!(
        result.authentication_kind,
        AuthenticationKind::AccountSession
    );
    assert_eq!(
        result.authentication_assurance,
        AssuranceLevel::StrongIndication
    );
}

#[tokio::test]
async fn claude_probe_api_key_only() {
    let dir = tempfile::tempdir().unwrap();
    let bin = install(dir.path(), "claude", "bash");
    let mut env = environment(Some(&bin), dir.path());
    env.vars
        .insert("ANTHROPIC_API_KEY".into(), "sk-test-api-secret".into());
    let result = probe_claude_code_in(&env).await;
    assert_eq!(result.authentication_kind, AuthenticationKind::ApiKey);
    assert_eq!(
        result.authentication_assurance,
        AssuranceLevel::StrongIndication
    );
}

#[tokio::test]
async fn claude_probe_mixed_with_strand_warning_and_no_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let bin = install(dir.path(), "claude", "bash");
    let claude_dir = dir.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join(".credentials.json"), "{}").unwrap();
    let mut env = environment(Some(&bin), dir.path());
    env.vars.insert(
        "CLAUDE_CODE_OAUTH_TOKEN".into(),
        "sk-test-oauth-secret".into(),
    );
    env.vars
        .insert("ANTHROPIC_API_KEY".into(), "sk-test-api-secret".into());
    let result = probe_claude_code_in(&env).await;
    assert_eq!(result.authentication_kind, AuthenticationKind::Mixed);
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("strands the subscription"))
    );
    assert_eq!(result.config_paths, vec![claude_dir]);
    assert_eq!(result.authentication_evidence.len(), 3);
    for text in result
        .authentication_evidence
        .iter()
        .chain(result.warnings.iter())
    {
        assert!(!text.contains("sk-test"));
    }
}

#[tokio::test]
async fn codex_probe_not_installed() {
    let dir = tempfile::tempdir().unwrap();
    let result = probe_codex_in(&environment(None, dir.path())).await;
    assert!(!result.installed);
    assert_eq!(result.authentication_kind, AuthenticationKind::Unknown);
    assert!(result.warnings.iter().any(|w| w.contains("not found")));
}

#[tokio::test]
async fn codex_probe_installation_only() {
    let dir = tempfile::tempdir().unwrap();
    let bin = install(dir.path(), "codex", "bash");
    let result = probe_codex_in(&environment(Some(&bin), dir.path())).await;
    assert!(result.installed);
    assert!(
        result
            .version
            .as_deref()
            .is_some_and(|v| v.contains("bash"))
    );
    assert_eq!(
        result.authentication_kind,
        AuthenticationKind::InstallationOnly
    );
    assert_eq!(result.authentication_assurance, AssuranceLevel::BestEffort);
    assert!(result.config_paths.is_empty());
}

#[tokio::test]
async fn codex_probe_api_key_only() {
    let dir = tempfile::tempdir().unwrap();
    let bin = install(dir.path(), "codex", "bash");
    let mut env = environment(Some(&bin), dir.path());
    env.vars
        .insert("OPENAI_API_KEY".into(), "sk-test-api-secret".into());
    let result = probe_codex_in(&env).await;
    assert_eq!(result.authentication_kind, AuthenticationKind::ApiKey);
    assert_eq!(
        result.authentication_assurance,
        AssuranceLevel::StrongIndication
    );
}

#[tokio::test]
async fn codex_probe_auth_json_means_account_session() {
    let dir = tempfile::tempdir().unwrap();
    let bin = install(dir.path(), "codex", "bash");
    let codex_dir = dir.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(
        codex_dir.join("auth.json"),
        "{\"tokens\": \"sk-test-account-secret\"}",
    )
    .unwrap();
    let result = probe_codex_in(&environment(Some(&bin), dir.path())).await;
    assert_eq!(
        result.authentication_kind,
        AuthenticationKind::AccountSession
    );
    assert_eq!(
        result.authentication_assurance,
        AssuranceLevel::StrongIndication
    );
    assert_eq!(result.config_paths, vec![codex_dir]);
}

#[tokio::test]
async fn codex_probe_mixed_with_shadow_warning_and_no_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let bin = install(dir.path(), "codex", "bash");
    let codex_dir = dir.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(
        codex_dir.join("auth.json"),
        "{\"tokens\": \"sk-test-account-secret\"}",
    )
    .unwrap();
    let mut env = environment(Some(&bin), dir.path());
    env.vars
        .insert("OPENAI_API_KEY".into(), "sk-test-api-secret".into());
    let result = probe_codex_in(&env).await;
    assert_eq!(result.authentication_kind, AuthenticationKind::Mixed);
    assert!(result.warnings.iter().any(|w| w.contains("shadow")));
    assert_eq!(result.authentication_evidence.len(), 2);
    for text in result
        .authentication_evidence
        .iter()
        .chain(result.warnings.iter())
    {
        assert!(!text.contains("sk-test"));
    }
}
