//! Container (Docker) sandbox backend for flywheel-orchestrator.
//!
//! Runs the task agent inside a Docker container against a bind-mounted
//! worktree (spec 00044). SDK-free: the agent runs as its own CLI inside the
//! image, so this crate shells out to `docker` and never links a vendor SDK.
//! The invocation itself rides the flywheel-agents claude-code CLI transport
//! under [`flywheel_agents::hosts::DockerExecHost`] (`docs/agent-harness.md`
//! section 15.4).
//!
//! Public surface: the Docker lifecycle primitives ([`docker`]), the
//! [`ClaudeAuth`] auth-mode passthrough ([`auth`]), the `[sandbox.network]`
//! policy resolver ([`network`]), [`ContainerSubmitStrategy`] (which composes
//! an inner landing strategy, [`submit`]), and the policy-to-strategy bridge
//! ([`config`]). Activated via `[sandbox] backend = "container"`.

pub mod auth;
pub mod config;
pub mod docker;
pub mod network;
pub mod submit;

pub use auth::{API_KEY_ENV, AuthError, ClaudeAuth, OAUTH_TOKEN_ENV};
pub use config::{
    ConfigError, build_container_config, container_config_from_policy, maybe_wrap_for_backend,
    resolve_auth,
};
pub use docker::{
    CleanupRegistry, DEFAULT_AGENT_HOME, DEFAULT_MAX_TAIL_CHARS, DEFAULT_START_TIMEOUT,
    DEFAULT_WORKDIR, Docker, DockerError, ExecOptions, ExecResult, OWNER_LABEL,
    OWNER_LABEL_SELECTOR, OWNER_LABEL_VALUE, OrphanReap, RunSpec, VolumeMount,
    active_container_names, build_exec_argv, build_run_argv, default_management_timeout,
    flush_cleanup_registry, reap_container_orphans, register_container_cleanup,
    spawn_container_orphan_reap,
};
pub use network::{DEFAULT_INTERNAL_NETWORK, NetworkError, ResolvedNetwork, resolve_network};
pub use submit::{
    ContainerConfig, ContainerInvokeSpec, ContainerLog, ContainerSubmitStrategy, HostInvokeBuilder,
    new_container_name,
};
