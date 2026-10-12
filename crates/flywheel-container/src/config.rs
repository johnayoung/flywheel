//! Bridge from resolved `[sandbox.container]` policy to a strategy (00045).
//!
//! Keeps the policy-to-strategy mapping in flywheel-container (which owns
//! [`ContainerSubmitStrategy`] / [`ClaudeAuth`]), so the worker just hands
//! over the policy, its resolved model, the process env, and the core invoke
//! builder, and calls [`maybe_wrap_for_backend`]. SDK-free.

use crate::auth::{API_KEY_ENV, AuthError, ClaudeAuth, OAUTH_TOKEN_ENV};
use crate::submit::{ContainerConfig, ContainerLog, ContainerSubmitStrategy, HostInvokeBuilder};
use flywheel_core::deadline_config::DeadlineClass;
use flywheel_orchestrator::policy::WorkPolicy;
use flywheel_orchestrator::strategy::SubmitStrategy;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("sandbox.container auth={mode:?} requires the {var} environment variable to be set")]
    MissingToken { mode: String, var: String },
    #[error("unknown container auth mode {0:?}")]
    UnknownAuthMode(String),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(
        "sandbox.backend = 'container' needs an explicit model: set [sandbox.container] model, \
         [agent] model, or --model (the agent CLI is invoked with --model and has no implicit \
         default here)."
    )]
    MissingModel,
}

fn default_token_env(mode: &str) -> Option<&'static str> {
    match mode {
        "oauth" => Some(OAUTH_TOKEN_ENV),
        "api_key" => Some(API_KEY_ENV),
        _ => None,
    }
}

/// Resolve a `[sandbox.container] auth` mode to a [`ClaudeAuth`].
///
/// `oauth`/`api_key` read the token from `env` by name (`token_env` or the
/// mode's default var); an unset or empty var is a clear error rather than a
/// silent unauthenticated run. `session` mounts `~/.claude`; `none` injects
/// nothing (the operator pre-authenticated the image or passes env directly).
pub fn resolve_auth(
    mode: &str,
    env: &BTreeMap<String, String>,
    token_env: Option<&str>,
) -> Result<Option<ClaudeAuth>, ConfigError> {
    match mode {
        "none" => Ok(None),
        "session" => Ok(Some(ClaudeAuth::session(None))),
        "oauth" | "api_key" => {
            let var = token_env
                .filter(|name| !name.is_empty())
                .or_else(|| default_token_env(mode))
                .unwrap_or_default();
            let token = env
                .get(var)
                .filter(|token| !token.is_empty())
                .ok_or_else(|| ConfigError::MissingToken {
                    mode: mode.to_owned(),
                    var: var.to_owned(),
                })?;
            let auth = if mode == "oauth" {
                ClaudeAuth::oauth_token(token.clone())?
            } else {
                ClaudeAuth::api_key(token.clone())?
            };
            Ok(Some(auth))
        }
        other => Err(ConfigError::UnknownAuthMode(other.to_owned())),
    }
}

/// Build a [`ContainerConfig`] from primitives (the Python
/// `build_container_strategy` parameters).
///
/// `exec_timeout` of `0` (or negative) means unbounded; an empty
/// `egress_network` means unset. `management_timeout` is the wall-clock
/// ceiling for docker *management* calls; `None` is the unbounded opt-out,
/// and the policy bridge supplies the `[deadlines] docker_management_seconds`
/// value.
#[allow(clippy::too_many_arguments)]
pub fn build_container_config(
    image: &str,
    model: &str,
    exec_timeout: Option<i64>,
    network_policy: Option<&str>,
    allow_hosts: &[String],
    egress_network: Option<&str>,
    auth: Option<ClaudeAuth>,
    env: BTreeMap<String, String>,
    management_timeout: Option<Duration>,
) -> ContainerConfig {
    ContainerConfig {
        auth,
        env,
        network_policy: network_policy.map(str::to_owned),
        allow_hosts: allow_hosts.to_vec(),
        egress_network: egress_network
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        exec_timeout: exec_timeout
            .filter(|seconds| *seconds > 0)
            .map(|s| s as f64),
        management_timeout,
        ..ContainerConfig::new(image, model)
    }
}

/// Resolve the container backend's config from a loaded policy.
///
/// `Ok(None)` when `[sandbox] backend` is not `"container"`. The model is
/// `[sandbox.container] model`, falling back to `model` (the worker's
/// resolved agent model); neither set is an error, since the agent CLI is
/// invoked with `--model` and has no implicit default here. Auth tokens are
/// read from `env` at resolve time and never from the policy file. Docker
/// management calls run under the policy's `[deadlines]
/// docker_management_seconds` ceiling, not the crate default.
pub fn container_config_from_policy(
    policy: &WorkPolicy,
    model: Option<&str>,
    env: &BTreeMap<String, String>,
) -> Result<Option<ContainerConfig>, ConfigError> {
    let sandbox = &policy.sandbox;
    if sandbox.backend != "container" {
        return Ok(None);
    }
    let container = &sandbox.container;
    let resolved_model = if container.model.is_empty() {
        model.filter(|m| !m.is_empty())
    } else {
        Some(container.model.as_str())
    }
    .ok_or(ConfigError::MissingModel)?;
    let auth = resolve_auth(
        &container.auth,
        env,
        Some(container.auth_env.as_str()).filter(|name| !name.is_empty()),
    )?;
    Ok(Some(build_container_config(
        &container.image,
        resolved_model,
        Some(container.exec_timeout),
        Some(sandbox.network.policy.as_str()),
        &sandbox.network.allow_hosts,
        Some(container.egress_network.as_str()),
        auth,
        BTreeMap::new(),
        policy
            .deadlines
            .duration_for_class(DeadlineClass::DockerManagement),
    )))
}

/// Select the run's submit strategy from `[sandbox] backend` (spec 00045).
///
/// `backend = "worktree"` (the default, or no policy) returns `inner`
/// unchanged. `backend = "container"` wraps it in a
/// [`ContainerSubmitStrategy`] configured from `[sandbox.container]` /
/// `[sandbox.network]`: the inner backend still provisions and lands; the
/// agent runs in the container through `invoke_builder` (the worker passes
/// the core `agents_invoke` builder over the host this strategy provides).
pub fn maybe_wrap_for_backend(
    inner: Arc<dyn SubmitStrategy>,
    policy: Option<&WorkPolicy>,
    model: Option<&str>,
    env: &BTreeMap<String, String>,
    invoke_builder: HostInvokeBuilder,
    log: ContainerLog,
) -> Result<Arc<dyn SubmitStrategy>, ConfigError> {
    let Some(policy) = policy else {
        return Ok(inner);
    };
    let Some(config) = container_config_from_policy(policy, model, env)? else {
        return Ok(inner);
    };
    log(&format!(
        "backend=container image={} model={} auth={} network={}",
        config.image, config.model, policy.sandbox.container.auth, policy.sandbox.network.policy
    ));
    let strategy = ContainerSubmitStrategy::new(inner, config, invoke_builder)?.with_log(log);
    Ok(Arc::new(strategy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::default_management_timeout;
    use flywheel_orchestrator::policy::load_policy_str;
    use flywheel_orchestrator::strategy::{InvokeFn, PlainDirectoryStrategy};
    use std::path::Path;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn builder() -> HostInvokeBuilder {
        Arc::new(|_host, _spec| -> InvokeFn {
            Arc::new(|_req| Box::pin(async { unreachable!("never invoked in config tests") }))
        })
    }

    fn quiet() -> ContainerLog {
        Arc::new(|_line| {})
    }

    // --- resolve_auth ----------------------------------------------------------

    #[test]
    fn oauth_reads_default_env() {
        let auth = resolve_auth("oauth", &env(&[(OAUTH_TOKEN_ENV, "tok")]), None)
            .unwrap()
            .unwrap();
        let (resolved, _) = auth.resolve(&BTreeMap::new()).unwrap();
        assert_eq!(resolved, env(&[(OAUTH_TOKEN_ENV, "tok")]));
        assert!(auth.is_subscription());
    }

    #[test]
    fn api_key_reads_default_env() {
        let auth = resolve_auth("api_key", &env(&[(API_KEY_ENV, "sk-ant")]), None)
            .unwrap()
            .unwrap();
        let (resolved, _) = auth.resolve(&BTreeMap::new()).unwrap();
        assert_eq!(resolved, env(&[(API_KEY_ENV, "sk-ant")]));
        assert!(!auth.is_subscription());
    }

    #[test]
    fn custom_token_env_name() {
        let auth = resolve_auth("oauth", &env(&[("MY_TOKEN", "t")]), Some("MY_TOKEN"))
            .unwrap()
            .unwrap();
        let (resolved, _) = auth.resolve(&BTreeMap::new()).unwrap();
        assert_eq!(resolved, env(&[(OAUTH_TOKEN_ENV, "t")]));
    }

    #[test]
    fn missing_or_empty_token_env_is_clear_error() {
        let err = resolve_auth("oauth", &BTreeMap::new(), None).unwrap_err();
        assert_eq!(
            err,
            ConfigError::MissingToken {
                mode: "oauth".into(),
                var: OAUTH_TOKEN_ENV.into()
            }
        );
        assert!(err.to_string().contains(OAUTH_TOKEN_ENV));
        assert!(resolve_auth("api_key", &env(&[(API_KEY_ENV, "")]), None).is_err());
    }

    #[test]
    fn session_needs_no_env() {
        let auth = resolve_auth("session", &BTreeMap::new(), None)
            .unwrap()
            .unwrap();
        let (_, mounts) = auth.resolve(&BTreeMap::new()).unwrap();
        assert_eq!(mounts.len(), 1);
    }

    #[test]
    fn none_injects_nothing() {
        assert!(
            resolve_auth("none", &BTreeMap::new(), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unknown_mode_rejected() {
        let err = resolve_auth("magic", &BTreeMap::new(), None).unwrap_err();
        assert_eq!(err, ConfigError::UnknownAuthMode("magic".into()));
        assert!(err.to_string().starts_with("unknown container auth mode"));
    }

    // --- build_container_config -----------------------------------------------------

    #[test]
    fn exec_timeout_zero_is_unbounded_and_empty_egress_is_unset() {
        let cfg = build_container_config(
            "img",
            "m",
            Some(0),
            Some("deny"),
            &[],
            Some(""),
            None,
            BTreeMap::new(),
            default_management_timeout(),
        );
        assert_eq!(cfg.exec_timeout, None);
        assert_eq!(cfg.egress_network, None);
        assert_eq!(cfg.network_policy.as_deref(), Some("deny"));
        assert_eq!(cfg.management_timeout, default_management_timeout());
        let cfg = build_container_config(
            "img",
            "m",
            Some(900),
            None,
            &[],
            Some("proxy"),
            None,
            BTreeMap::new(),
            Some(Duration::from_secs(3)),
        );
        assert_eq!(cfg.exec_timeout, Some(900.0));
        assert_eq!(cfg.egress_network.as_deref(), Some("proxy"));
        assert_eq!(cfg.management_timeout, Some(Duration::from_secs(3)));
    }

    // --- policy bridge -----------------------------------------------------------------

    fn policy(text: &str) -> WorkPolicy {
        let text = format!("[source]\nkind = \"directory\"\n{text}");
        load_policy_str(&text, Path::new("/tmp/flywheel.toml")).unwrap()
    }

    const CONTAINER_POLICY: &str = r#"
[sandbox]
backend = "container"

[sandbox.container]
image = "flywheel-agent:latest"
auth = "oauth"
exec_timeout = 0

[sandbox.network]
policy = "deny"
allow_hosts = ["api.github.com"]

[deadlines]
docker_management_seconds = 7.5
"#;

    #[test]
    fn container_backend_resolves_from_policy() {
        let p = policy(CONTAINER_POLICY);
        let cfg = container_config_from_policy(
            &p,
            Some("claude-sonnet-4-6"),
            &env(&[(OAUTH_TOKEN_ENV, "tok")]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(cfg.image, "flywheel-agent:latest");
        assert_eq!(cfg.model, "claude-sonnet-4-6");
        assert!(cfg.auth.as_ref().unwrap().is_subscription());
        assert_eq!(cfg.exec_timeout, None);
        assert_eq!(cfg.network_policy.as_deref(), Some("deny"));
        assert_eq!(cfg.allow_hosts, vec!["api.github.com"]);
        assert_eq!(cfg.egress_network, None);
        assert_eq!(cfg.management_timeout, Some(Duration::from_secs_f64(7.5)));
        assert!(cfg.preflight);
    }

    #[test]
    fn container_model_in_policy_wins_over_fallback() {
        let p = policy(&CONTAINER_POLICY.replace(
            "auth = \"oauth\"",
            "auth = \"none\"\nmodel = \"claude-opus-4-8\"",
        ));
        let cfg = container_config_from_policy(&p, Some("fallback"), &BTreeMap::new())
            .unwrap()
            .unwrap();
        assert_eq!(cfg.model, "claude-opus-4-8");
        assert!(cfg.auth.is_none());
    }

    #[test]
    fn missing_model_is_an_error() {
        let p = policy(&CONTAINER_POLICY.replace("auth = \"oauth\"", "auth = \"none\""));
        assert_eq!(
            container_config_from_policy(&p, None, &BTreeMap::new()).unwrap_err(),
            ConfigError::MissingModel
        );
    }

    #[test]
    fn missing_token_surfaces_through_the_bridge() {
        let p = policy(CONTAINER_POLICY);
        let err = container_config_from_policy(&p, Some("m"), &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, ConfigError::MissingToken { .. }));
    }

    #[test]
    fn worktree_backend_resolves_to_none_and_wraps_nothing() {
        let p = policy("[sandbox]\nbackend = \"worktree\"\n");
        assert!(
            container_config_from_policy(&p, Some("m"), &BTreeMap::new())
                .unwrap()
                .is_none()
        );
        let inner: Arc<dyn SubmitStrategy> = Arc::new(PlainDirectoryStrategy::new("/tmp/x"));
        let out = maybe_wrap_for_backend(
            Arc::clone(&inner),
            Some(&p),
            Some("m"),
            &BTreeMap::new(),
            builder(),
            quiet(),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&inner, &out));
        let out = maybe_wrap_for_backend(
            Arc::clone(&inner),
            None,
            None,
            &BTreeMap::new(),
            builder(),
            quiet(),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&inner, &out));
    }

    #[test]
    fn container_backend_wraps_inner_and_logs_selection() {
        let p = policy(CONTAINER_POLICY);
        let inner: Arc<dyn SubmitStrategy> = Arc::new(PlainDirectoryStrategy::new("/tmp/x"));
        let lines: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let sink = Arc::clone(&lines);
        let out = maybe_wrap_for_backend(
            Arc::clone(&inner),
            Some(&p),
            Some("claude-sonnet-4-6"),
            &env(&[(OAUTH_TOKEN_ENV, "tok")]),
            builder(),
            Arc::new(move |line| sink.lock().unwrap().push(line.to_owned())),
        )
        .unwrap();
        assert!(!Arc::ptr_eq(&inner, &out));
        assert_eq!(
            lines.lock().unwrap().as_slice(),
            [
                "backend=container image=flywheel-agent:latest model=claude-sonnet-4-6 auth=oauth network=deny"
            ]
        );
    }

    #[test]
    fn subscription_auth_with_api_key_in_container_env_is_rejected() {
        // The bridge never threads the process env into the container, so a
        // host ANTHROPIC_API_KEY does not conflict; an explicit container env
        // carrying it does.
        let p = policy(CONTAINER_POLICY);
        let host_env = env(&[(OAUTH_TOKEN_ENV, "tok"), (API_KEY_ENV, "sk-host")]);
        let cfg = container_config_from_policy(&p, Some("m"), &host_env)
            .unwrap()
            .unwrap();
        assert!(cfg.env.is_empty());
        let inner: Arc<dyn SubmitStrategy> = Arc::new(PlainDirectoryStrategy::new("/tmp/x"));
        ContainerSubmitStrategy::new(Arc::clone(&inner), cfg.clone(), builder()).unwrap();
        let mut conflicting = cfg;
        conflicting
            .env
            .insert(API_KEY_ENV.into(), "sk-container".into());
        assert_eq!(
            ContainerSubmitStrategy::new(inner, conflicting, builder())
                .err()
                .unwrap(),
            AuthError::ApiKeyConflict
        );
    }
}
