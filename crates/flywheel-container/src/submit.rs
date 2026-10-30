//! The container submit strategy (spec 00044 G5, rebased on flywheel-agents).
//!
//! Assembles the G3 lifecycle primitives onto the `SandboxHandle` seam.
//! [`ContainerSubmitStrategy`] *composes* an inner landing strategy (e.g. the
//! git-worktree merge submitter) rather than depending on it: the inner
//! strategy provisions the worktree and lands the result host-side
//! (unchanged; the worktree is bind-mounted, so the agent's edits are already
//! on the host), while this layer starts the container, runs the agent CLI
//! inside it, and tears the container down.
//!
//! The agent invocation rides the flywheel-agents claude-code CLI transport
//! (`docs/agent-harness.md` section 15.4) over a
//! [`flywheel_agents::hosts::DockerExecHost`]. The host-to-`InvokeFn` bridge
//! lives in core (`flywheel_core::agents_invoke`), above this crate's
//! dependency line, so the strategy takes it as a constructor parameter
//! ([`HostInvokeBuilder`]): the worker wires the core builder in when it
//! selects `backend = "container"` (see [`crate::config::maybe_wrap_for_backend`]).
//! Every normalized event reaches the harness observer, so the hang watchdog
//! and per-message telemetry are live on this path.
//!
//! SDK-free: the agent runs as its own CLI inside the image; this module
//! never links a vendor SDK (the flywheel-agents CLI transport shells out to
//! the vendor executable via `docker exec`).

use crate::auth::{AuthError, ClaudeAuth};
use crate::docker::{
    CleanupRegistry, DEFAULT_AGENT_HOME, DEFAULT_START_TIMEOUT, DEFAULT_WORKDIR, Docker,
    DockerError, RunSpec, VolumeMount, default_management_timeout,
};
use crate::network::{DEFAULT_INTERNAL_NETWORK, resolve_network};
use flywheel_agents::hosts::{DockerExecHost, ExecutionHost};
use flywheel_orchestrator::strategy::{
    InvokeFn, InvokeWrapper, LandabilityVerdict, PrepareError, SandboxHandle, SandboxRequest,
    SubmitOutcome, SubmitRequest, SubmitStrategy, Teardown,
};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Builds the per-run `InvokeFn` from the execution host the strategy
/// provides (a `DockerExecHost` over the started container) and the
/// agent-level parameters the strategy resolved. The worker supplies the
/// core `agents_invoke` builder; tests supply their own.
pub type HostInvokeBuilder =
    Arc<dyn Fn(Arc<dyn ExecutionHost>, &ContainerInvokeSpec) -> InvokeFn + Send + Sync>;

/// Where the strategy reports best-effort failures (teardown timeouts, a
/// start-failure cleanup that itself failed). Defaults to stderr.
pub type ContainerLog = Arc<dyn Fn(&str) + Send + Sync>;

/// The agent-level parameters of one in-container invocation.
///
/// `docker exec` runs the agent as direct argv (no `sh -c`) with the prompt
/// piped on stdin, in `working_directory` (the in-container workdir the
/// worktree is mounted at). `timeout_seconds` becomes the runtime's
/// wall-clock ceiling: a run that exceeds it folds into a structured failure
/// with `error_type = "timeout"`; a nonzero agent exit folds into
/// `error_type = "agent_exit"`.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerInvokeSpec {
    pub agent_id: String,
    pub working_directory: PathBuf,
    pub model: String,
    /// `"auto"` (headless, no TTY to approve tool use) or `"supervised"`.
    pub permission_policy: String,
    pub timeout_seconds: Option<f64>,
}

/// Everything [`ContainerSubmitStrategy::new`] needs; a plain data shape so
/// the policy bridge ([`crate::config`]) and tests build it directly.
#[derive(Debug, Clone)]
pub struct ContainerConfig {
    pub image: String,
    /// Deployment config: the host SDK invoker is bypassed entirely, so the
    /// model cannot be inferred from there.
    pub model: String,
    /// Defaults true (mirroring the SDK path's `bypassPermissions`): a
    /// headless `--print` run has no TTY to approve tool use, so without it
    /// the CLI blocks forever the first time the agent writes a file.
    pub dangerously_skip_permissions: bool,
    /// `None` = the host's uid.
    pub container_uid: Option<u32>,
    /// `None` = the host's gid.
    pub container_gid: Option<u32>,
    /// Raw `--network` override, used only when `network_policy` is unset.
    pub network: Vec<String>,
    /// `[sandbox.network] policy`; when set it drives `--network` (G6).
    pub network_policy: Option<String>,
    pub allow_hosts: Vec<String>,
    pub egress_network: Option<String>,
    pub internal_network: String,
    pub mounts: Vec<VolumeMount>,
    pub env: BTreeMap<String, String>,
    pub auth: Option<ClaudeAuth>,
    pub cpus: Option<f64>,
    pub workdir: String,
    /// Wall-clock ceiling for the whole in-container run; `None` unbounded.
    pub exec_timeout: Option<f64>,
    /// Ceiling for every docker management call; `None` unbounded.
    pub management_timeout: Option<Duration>,
    /// Run the image/UID pre-flight before each start.
    pub preflight: bool,
    /// Command override appended after the image (a generic base image kept
    /// alive with `sleep infinity`); empty relies on the image `ENTRYPOINT`.
    pub command: Vec<String>,
}

impl ContainerConfig {
    pub fn new(image: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            model: model.into(),
            dangerously_skip_permissions: true,
            container_uid: None,
            container_gid: None,
            network: Vec::new(),
            network_policy: None,
            allow_hosts: Vec::new(),
            egress_network: None,
            internal_network: DEFAULT_INTERNAL_NETWORK.to_owned(),
            mounts: Vec::new(),
            env: BTreeMap::new(),
            auth: None,
            cpus: None,
            workdir: DEFAULT_WORKDIR.to_owned(),
            exec_timeout: None,
            management_timeout: default_management_timeout(),
            preflight: true,
            command: Vec::new(),
        }
    }
}

/// The per-run container name: `flywheel-<task_id>-<8 hex>`.
///
/// Docker names must match `[a-zA-Z0-9][a-zA-Z0-9_.-]+`, but a task id may
/// be a nested path (`phase1/login`, see
/// `flywheel_core::task::validate_id_path_safe`), so every character
/// outside that set is mapped to `_`. The random suffix keeps two ids that
/// sanitize alike (`a/b`, `a_b`) from colliding.
pub fn new_container_name(task_id: &str) -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let id: String = task_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("flywheel-{id}-{}", &suffix[..8])
}

fn host_uid() -> u32 {
    // SAFETY: getuid never fails and touches no memory.
    unsafe { libc::getuid() }
}

fn host_gid() -> u32 {
    // SAFETY: getgid never fails and touches no memory.
    unsafe { libc::getgid() }
}

/// Run the task agent inside a Docker container (a [`SubmitStrategy`]).
///
/// `inner` provisions the worktree and lands the terminal status; this
/// strategy bind-mounts that worktree into a container, runs the agent CLI
/// in it, and disposes the container via the handle's teardown.
pub struct ContainerSubmitStrategy {
    inner: Arc<dyn SubmitStrategy>,
    image: String,
    model: String,
    dangerously_skip_permissions: bool,
    uid: u32,
    gid: u32,
    network: Vec<String>,
    network_policy: Option<String>,
    allow_hosts: Vec<String>,
    egress_network: Option<String>,
    internal_network: String,
    mounts: Vec<VolumeMount>,
    env: BTreeMap<String, String>,
    cpus: Option<f64>,
    workdir: String,
    exec_timeout: Option<f64>,
    management_timeout: Option<Duration>,
    preflight: bool,
    command: Vec<String>,
    docker: Docker,
    registry: Arc<CleanupRegistry>,
    invoke_builder: HostInvokeBuilder,
    log: ContainerLog,
}

impl fmt::Debug for ContainerSubmitStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContainerSubmitStrategy")
            .field("image", &self.image)
            .field("model", &self.model)
            .field("user", &format!("{}:{}", self.uid, self.gid))
            .field("network_policy", &self.network_policy)
            .field("workdir", &self.workdir)
            .field("exec_timeout", &self.exec_timeout)
            .field("management_timeout", &self.management_timeout)
            .field("preflight", &self.preflight)
            .finish_non_exhaustive()
    }
}

impl ContainerSubmitStrategy {
    /// Wrap `inner`. Fails when `config.auth` is a subscription mode and the
    /// operator env already carries `ANTHROPIC_API_KEY` (the key would
    /// silently win over the session).
    pub fn new(
        inner: Arc<dyn SubmitStrategy>,
        config: ContainerConfig,
        invoke_builder: HostInvokeBuilder,
    ) -> Result<Self, AuthError> {
        let mut env = config.env;
        // The agent CLI resolves its config/cache/credentials under $HOME
        // (~/.claude). A `docker run --user <uid>` numeric override leaves
        // HOME unset (or "/"), which the agent cannot write to; it stalls or
        // fails. Always point HOME at the agent home unless the operator set
        // it.
        env.entry("HOME".to_owned())
            .or_insert_with(|| DEFAULT_AGENT_HOME.to_owned());
        let mut mounts = config.mounts;
        if let Some(auth) = &config.auth {
            // Auth env/mounts are validated against the operator-supplied env
            // (a subscription mode rejects a stray ANTHROPIC_API_KEY) and then
            // folded in, so the container starts already authenticated.
            let (auth_env, auth_mounts) = auth.resolve(&env)?;
            env.extend(auth_env);
            mounts.extend(auth_mounts);
        }
        Ok(Self {
            inner,
            image: config.image,
            model: config.model,
            dangerously_skip_permissions: config.dangerously_skip_permissions,
            uid: config.container_uid.unwrap_or_else(host_uid),
            gid: config.container_gid.unwrap_or_else(host_gid),
            network: config.network,
            network_policy: config.network_policy,
            allow_hosts: config.allow_hosts,
            egress_network: config.egress_network,
            internal_network: config.internal_network,
            mounts,
            env,
            cpus: config.cpus,
            workdir: config.workdir,
            exec_timeout: config.exec_timeout,
            management_timeout: config.management_timeout,
            preflight: config.preflight,
            command: config.command,
            docker: Docker::new(),
            registry: CleanupRegistry::global(),
            invoke_builder,
            log: Arc::new(|line| eprintln!("{line}")),
        })
    }

    /// Use `docker` for every management call and as the `docker` argv
    /// prefix of the `DockerExecHost` the invoke runs through.
    pub fn with_docker(mut self, docker: Docker) -> Self {
        self.docker = docker;
        self
    }

    /// Register started containers on `registry` instead of the global one.
    pub fn with_registry(mut self, registry: Arc<CleanupRegistry>) -> Self {
        self.registry = registry;
        self
    }

    pub fn with_log(mut self, log: ContainerLog) -> Self {
        self.log = log;
        self
    }

    pub fn management_timeout(&self) -> Option<Duration> {
        self.management_timeout
    }

    pub fn exec_timeout(&self) -> Option<f64> {
        self.exec_timeout
    }

    /// The agent-level parameters every in-container invocation uses.
    pub fn invoke_spec(&self) -> ContainerInvokeSpec {
        ContainerInvokeSpec {
            agent_id: "claude-code".to_owned(),
            working_directory: PathBuf::from(&self.workdir),
            model: self.model.clone(),
            permission_policy: if self.dangerously_skip_permissions {
                "auto".to_owned()
            } else {
                "supervised".to_owned()
            },
            timeout_seconds: self.exec_timeout,
        }
    }

    /// The container's `--network`: `[sandbox.network]` policy when
    /// configured (G6), otherwise the raw `network` override.
    fn resolve_networks(&self) -> Result<Vec<String>, PrepareError> {
        let Some(policy) = &self.network_policy else {
            return Ok(self.network.clone());
        };
        let resolved = resolve_network(
            policy,
            &self.allow_hosts,
            self.egress_network.as_deref(),
            &self.internal_network,
        )
        .map_err(|err| PrepareError::with_source(err.to_string(), err))?;
        match resolved.name {
            Some(name) => {
                if resolved.ensure_internal {
                    self.docker
                        .ensure_internal_network(&name, self.management_timeout)
                        .map_err(docker_prepare_error)?;
                }
                Ok(vec![name])
            }
            None => Ok(Vec::new()),
        }
    }

    /// Pre-flight, start, and register the container for `worktree`;
    /// returns its name. A failed `docker run` force-removes the name so a
    /// half-created container never lingers until the next orphan reap.
    fn start_for(&self, task_id: &str, worktree: &Path) -> Result<String, PrepareError> {
        if self.preflight {
            self.docker
                .check_image_uid(&self.image, self.uid, self.management_timeout)
                .map_err(docker_prepare_error)?;
        }
        let networks = self.resolve_networks()?;
        let name = new_container_name(task_id);
        let mut mounts = vec![VolumeMount::new(
            worktree.to_string_lossy().into_owned(),
            self.workdir.clone(),
        )];
        mounts.extend(self.mounts.iter().cloned());
        let spec = RunSpec {
            name: name.clone(),
            image: self.image.clone(),
            env: self.env.clone(),
            mounts,
            workdir: Some(self.workdir.clone()),
            user: Some(format!("{}:{}", self.uid, self.gid)),
            networks,
            groups: Vec::new(),
            devices: Vec::new(),
            cpus: self.cpus,
            labels: BTreeMap::new(),
            command: self.command.clone(),
        };
        if let Err(err) = self
            .docker
            .start_container(&spec, Some(DEFAULT_START_TIMEOUT))
        {
            if let Err(cleanup) = self
                .docker
                .force_remove_container(&name, self.management_timeout)
            {
                (self.log)(&format!(
                    "Container {name} could not be removed after a failed start ({cleanup})"
                ));
            }
            return Err(docker_prepare_error(err));
        }
        self.registry
            .register(name.clone(), self.management_timeout);
        Ok(name)
    }

    fn make_invoke_wrapper(&self, container_name: &str) -> InvokeWrapper {
        let builder = Arc::clone(&self.invoke_builder);
        let spec = self.invoke_spec();
        let host: Arc<dyn ExecutionHost> = Arc::new(
            DockerExecHost::new(container_name).with_docker_argv(self.docker.program().to_vec()),
        );
        // The host SDK invoker is replaced wholesale: the agent runs in the
        // container, not the worker process. The base invoke (`None` in
        // normal operation) is intentionally ignored.
        Arc::new(move |_base: Option<InvokeFn>| builder(Arc::clone(&host), &spec))
    }

    fn make_teardown(&self, container_name: String, inner_handle: SandboxHandle) -> Teardown {
        let docker = self.docker.clone();
        let registry = Arc::clone(&self.registry);
        let timeout = self.management_timeout;
        let log = Arc::clone(&self.log);
        Arc::new(move || {
            if let Err(err) = docker.remove_container(&container_name, timeout) {
                log(&format!(
                    "Container {container_name} teardown failed ({err})"
                ));
            }
            registry.unregister(&container_name);
            // The inner strategy's own teardown (if any) runs after the
            // container is gone, since the container mounted its path.
            if let Err(panic) = inner_handle.run_teardown() {
                log(&format!(
                    "Inner sandbox teardown for {container_name} panicked: {panic}"
                ));
            }
        })
    }
}

fn docker_prepare_error(err: DockerError) -> PrepareError {
    PrepareError::with_source(err.to_string(), err)
}

impl SubmitStrategy for ContainerSubmitStrategy {
    fn prepare_sandbox(&self, request: &SandboxRequest) -> Result<SandboxHandle, PrepareError> {
        let inner_handle = self.inner.prepare_sandbox(request)?;
        let worktree = inner_handle.path.clone();
        let name = match self.start_for(&request.task_id, &worktree) {
            Ok(name) => name,
            Err(err) => {
                // The inner strategy already provisioned; give it back its
                // sandbox rather than stranding a worktree behind a failed
                // container start.
                if let Err(panic) = inner_handle.run_teardown() {
                    (self.log)(&format!(
                        "Inner sandbox teardown for {} panicked after a failed container start: \
                         {panic}",
                        request.task_id
                    ));
                }
                return Err(err);
            }
        };
        let invoke_wrapper = self.make_invoke_wrapper(&name);
        let env_contribution = inner_handle.env_contribution.clone();
        let teardown = self.make_teardown(name, inner_handle);
        Ok(SandboxHandle::new(worktree)
            .with_env(env_contribution)
            .with_invoke_wrapper(invoke_wrapper)
            .with_teardown(teardown))
    }

    fn submit(&self, request: &SubmitRequest) -> SubmitOutcome {
        // Landing is host-side and backend-agnostic: the worktree the inner
        // strategy provisioned was bind-mounted, so the agent's edits are
        // already on it. Delegate the merge/park unchanged.
        self.inner.submit(request)
    }

    fn is_landable(&self, request: &SubmitRequest) -> LandabilityVerdict {
        self.inner.is_landable(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{API_KEY_ENV, OAUTH_TOKEN_ENV};
    use crate::docker::testing::{
        HANG_BODY, HAPPY_BODY, live_lock, recorded_calls, require_docker, scripted_docker,
    };
    use flywheel_agents::hosts::{ProcessPlan, StdinPlan};
    use flywheel_core::envelope::EnvelopeResult;
    use flywheel_core::invoker::{InvocationSignals, IterationResult};
    use flywheel_core::lifecycle::Status;
    use flywheel_core::task::{CommandGrader, Grader, Task};
    use flywheel_orchestrator::strategy::{
        CallableStrategy, InvocationRequest, PlainDirectoryStrategy, SandboxMode,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    const WALL_BUDGET: Duration = Duration::from_secs(5);

    fn request(task_id: &str) -> SandboxRequest {
        SandboxRequest {
            task_id: task_id.into(),
            task_file: None,
            run_id: None,
            mode: SandboxMode::Fresh,
            source_ref: String::new(),
        }
    }

    fn invocation() -> InvocationRequest {
        InvocationRequest {
            attempt_number: 1,
            iteration_number: 1,
            ..InvocationRequest::new("do the thing")
        }
    }

    /// A builder that records the spec it was handed and returns an invoke
    /// producing a fixed result; the host is never spawned.
    struct RecordingBuilder {
        specs: Mutex<Vec<ContainerInvokeSpec>>,
    }

    impl RecordingBuilder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                specs: Mutex::new(Vec::new()),
            })
        }

        fn as_builder(self: &Arc<Self>) -> HostInvokeBuilder {
            let me = Arc::clone(self);
            Arc::new(move |_host, spec| {
                me.specs.lock().unwrap().push(spec.clone());
                Arc::new(|request: InvocationRequest| {
                    Box::pin(async move {
                        IterationResult {
                            transcript: format!("recorded:{}", request.prompt),
                            messages: Vec::new(),
                            envelope: EnvelopeResult::Missing,
                            signals: InvocationSignals::default(),
                            failure: None,
                            usage: None,
                        }
                    })
                })
            })
        }
    }

    fn config() -> ContainerConfig {
        ContainerConfig {
            container_uid: Some(1000),
            container_gid: Some(1000),
            preflight: false,
            ..ContainerConfig::new("flywheel/agent:test", "claude-opus-4-8")
        }
    }

    struct Harness {
        strategy: ContainerSubmitStrategy,
        log: PathBuf,
        registry: Arc<CleanupRegistry>,
        lines: Arc<Mutex<Vec<String>>>,
    }

    fn harness(
        dir: &Path,
        inner: Arc<dyn SubmitStrategy>,
        config: ContainerConfig,
        body: &str,
    ) -> Harness {
        harness_with_builder(
            dir,
            inner,
            config,
            body,
            RecordingBuilder::new().as_builder(),
        )
    }

    fn harness_with_builder(
        dir: &Path,
        inner: Arc<dyn SubmitStrategy>,
        config: ContainerConfig,
        body: &str,
        builder: HostInvokeBuilder,
    ) -> Harness {
        let (docker, log) = scripted_docker(dir, body);
        let registry = Arc::new(CleanupRegistry::new());
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let strategy = ContainerSubmitStrategy::new(inner, config, builder)
            .unwrap()
            .with_docker(docker)
            .with_registry(Arc::clone(&registry))
            .with_log(Arc::new(move |line| {
                sink.lock().unwrap().push(line.to_owned())
            }));
        Harness {
            strategy,
            log,
            registry,
            lines,
        }
    }

    fn plain(dir: &Path) -> (Arc<dyn SubmitStrategy>, PathBuf) {
        let root = dir.join("sandboxes");
        (Arc::new(PlainDirectoryStrategy::new(&root)), root)
    }

    fn run_call(log: &Path) -> Vec<String> {
        recorded_calls(log)
            .into_iter()
            .find(|call| call.first().is_some_and(|sub| sub == "run"))
            .expect("no docker run recorded")
    }

    fn flag_values<'a>(argv: &'a [String], flag: &str) -> Vec<&'a str> {
        argv.iter()
            .enumerate()
            .filter(|(_, token)| *token == flag)
            .map(|(index, _)| argv[index + 1].as_str())
            .collect()
    }

    fn env_of(argv: &[String]) -> BTreeMap<String, String> {
        flag_values(argv, "-e")
            .into_iter()
            .map(|pair| {
                let (k, v) = pair.split_once('=').unwrap();
                (k.to_owned(), v.to_owned())
            })
            .collect()
    }

    // --- prepare / teardown over a real inner strategy ------------------------

    #[test]
    fn prepare_bind_mounts_worktree_and_returns_handle() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, root) = plain(dir.path());
        let h = harness(dir.path(), inner, config(), HAPPY_BODY);
        let handle = h.strategy.prepare_sandbox(&request("alpha")).unwrap();

        let worktree = root.join("alpha");
        assert!(worktree.is_dir(), "inner strategy provisioned the sandbox");
        assert_eq!(handle.path, worktree);
        assert!(handle.invoke_wrapper.is_some());
        assert!(handle.teardown.is_some());

        let run = run_call(&h.log);
        assert_eq!(&run[..3], ["run", "-d", "--name"]);
        assert_eq!(flag_values(&run, "--user"), vec!["1000:1000"]);
        assert_eq!(
            flag_values(&run, "-v")[0],
            format!("{}:{DEFAULT_WORKDIR}", worktree.display())
        );
        assert_eq!(flag_values(&run, "-w"), vec![DEFAULT_WORKDIR]);
        let name = flag_values(&run, "--name")[0];
        assert!(name.starts_with("flywheel-alpha-"));
        assert_eq!(name.len(), "flywheel-alpha-".len() + 8);
        assert!(flag_values(&run, "--label").contains(&"flywheel.owner=1"));
        assert_eq!(*run.last().unwrap(), "flywheel/agent:test");
        assert_eq!(h.registry.active_names().len(), 1);
    }

    /// Docker's container-name grammar: `[a-zA-Z0-9][a-zA-Z0-9_.-]+`.
    fn is_valid_docker_name(name: &str) -> bool {
        let mut chars = name.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
            && name.len() > 1
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    }

    #[test]
    fn nested_task_id_yields_a_valid_docker_container_name() {
        // `validate_id_path_safe` admits `/`-separated ids; docker rejects
        // `/` in --name ("Invalid container name"), so the run never starts.
        let name = new_container_name("phase1/login");
        assert!(is_valid_docker_name(&name), "invalid docker name {name:?}");
        assert!(name.starts_with("flywheel-phase1_login-"), "{name}");
        assert_eq!(name.len(), "flywheel-phase1_login-".len() + 8);

        let dir = tempfile::tempdir().unwrap();
        let (inner, root) = plain(dir.path());
        let h = harness(dir.path(), inner, config(), HAPPY_BODY);
        let handle = h
            .strategy
            .prepare_sandbox(&request("phase1/login"))
            .unwrap();
        assert_eq!(handle.path, root.join("phase1/login"));
        let run = run_call(&h.log);
        let name = flag_values(&run, "--name")[0];
        assert!(is_valid_docker_name(name), "invalid docker name {name:?}");
        assert_eq!(
            h.registry.active_names(),
            std::collections::BTreeSet::from([name.to_owned()])
        );
    }

    #[test]
    fn container_always_gets_home_env() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let h = harness(dir.path(), inner, config(), HAPPY_BODY);
        h.strategy.prepare_sandbox(&request("t")).unwrap();
        assert_eq!(env_of(&run_call(&h.log))["HOME"], DEFAULT_AGENT_HOME);
    }

    #[test]
    fn operator_home_override_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let mut cfg = config();
        cfg.env.insert("HOME".into(), "/custom/home".into());
        let h = harness(dir.path(), inner, cfg, HAPPY_BODY);
        h.strategy.prepare_sandbox(&request("t")).unwrap();
        assert_eq!(env_of(&run_call(&h.log))["HOME"], "/custom/home");
    }

    #[test]
    fn command_override_rides_after_the_image() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let mut cfg = config();
        cfg.command = vec!["sleep".into(), "infinity".into()];
        let h = harness(dir.path(), inner, cfg, HAPPY_BODY);
        h.strategy.prepare_sandbox(&request("t")).unwrap();
        let run = run_call(&h.log);
        assert_eq!(
            &run[run.len() - 3..],
            ["flywheel/agent:test", "sleep", "infinity"]
        );
    }

    #[test]
    fn teardown_removes_container_and_unregisters() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let h = harness(dir.path(), inner, config(), HAPPY_BODY);
        let handle = h.strategy.prepare_sandbox(&request("alpha")).unwrap();
        let name = h.registry.active_names().into_iter().next().unwrap();
        handle.run_teardown().unwrap();
        let calls = recorded_calls(&h.log);
        assert!(calls.contains(&vec!["stop".to_owned(), name.clone()]));
        assert!(calls.contains(&vec!["rm".to_owned(), name.clone()]));
        assert!(h.registry.active_names().is_empty());
        assert!(h.lines.lock().unwrap().is_empty());
    }

    #[test]
    fn inner_handle_env_and_teardown_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("wt");
        std::fs::create_dir(&sandbox).unwrap();
        let inner_teardowns = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&inner_teardowns);
        let sandbox_for_prepare = sandbox.clone();
        let inner: Arc<dyn SubmitStrategy> = Arc::new(CallableStrategy::new(
            Arc::new(move |_req| {
                let counter = Arc::clone(&counter);
                Ok(SandboxHandle::new(&sandbox_for_prepare)
                    .with_env(BTreeMap::from([("FOO".to_owned(), "bar".to_owned())]))
                    .with_teardown(Arc::new(move || {
                        counter.fetch_add(1, Ordering::SeqCst);
                    })))
            }),
            Arc::new(|_req| SubmitOutcome::no_op("test")),
        ));
        let h = harness(dir.path(), inner, config(), HAPPY_BODY);
        let handle = h.strategy.prepare_sandbox(&request("t")).unwrap();
        assert_eq!(handle.path, sandbox);
        assert_eq!(handle.env_contribution["FOO"], "bar");
        handle.run_teardown().unwrap();
        // Container removal happens first, then the inner teardown.
        assert_eq!(inner_teardowns.load(Ordering::SeqCst), 1);
        let calls = recorded_calls(&h.log);
        assert!(calls.iter().any(|c| c[0] == "stop"));
    }

    #[test]
    fn start_failure_force_removes_name_and_tears_down_inner() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("wt");
        std::fs::create_dir(&sandbox).unwrap();
        let inner_teardowns = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&inner_teardowns);
        let inner: Arc<dyn SubmitStrategy> = Arc::new(CallableStrategy::new(
            Arc::new(move |_req| {
                let counter = Arc::clone(&counter);
                Ok(
                    SandboxHandle::new(&sandbox).with_teardown(Arc::new(move || {
                        counter.fetch_add(1, Ordering::SeqCst);
                    })),
                )
            }),
            Arc::new(|_req| SubmitOutcome::no_op("test")),
        ));
        let body = r#"case "$1" in run) echo "no such image" >&2; exit 125 ;; esac"#;
        let h = harness(dir.path(), inner, config(), body);
        let err = h.strategy.prepare_sandbox(&request("t")).unwrap_err();
        assert!(
            err.message
                .contains("docker run failed (exit 125): no such image")
        );
        let calls = recorded_calls(&h.log);
        let name = flag_values(&calls[0], "--name")[0].to_owned();
        assert_eq!(calls[1], vec!["rm".to_owned(), "-f".to_owned(), name]);
        assert_eq!(inner_teardowns.load(Ordering::SeqCst), 1);
        assert!(h.registry.active_names().is_empty());
    }

    #[test]
    fn inner_prepare_failure_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn SubmitStrategy> = Arc::new(CallableStrategy::new(
            Arc::new(|_req| Err(PrepareError::new("inner refused"))),
            Arc::new(|_req| SubmitOutcome::no_op("test")),
        ));
        let h = harness(dir.path(), inner, config(), HAPPY_BODY);
        let err = h.strategy.prepare_sandbox(&request("t")).unwrap_err();
        assert_eq!(err.message, "inner refused");
        assert!(recorded_calls(&h.log).is_empty());
    }

    // --- invoke wrapper ----------------------------------------------------------

    #[test]
    fn invoke_wrapper_builds_from_spec_and_ignores_base() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let builder = RecordingBuilder::new();
        let mut cfg = config();
        cfg.exec_timeout = Some(900.0);
        let h = harness_with_builder(dir.path(), inner, cfg, HAPPY_BODY, builder.as_builder());
        let handle = h.strategy.prepare_sandbox(&request("t")).unwrap();

        let base: InvokeFn = Arc::new(|_req| {
            Box::pin(async { panic!("base invoke must not run in the container path") })
        });
        let invoke = handle.wrap_invoke(Some(base)).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(invoke(invocation()));
        assert_eq!(result.transcript, "recorded:do the thing");

        let specs = builder.specs.lock().unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(
            specs[0],
            ContainerInvokeSpec {
                agent_id: "claude-code".into(),
                working_directory: PathBuf::from(DEFAULT_WORKDIR),
                model: "claude-opus-4-8".into(),
                permission_policy: "auto".into(),
                timeout_seconds: Some(900.0),
            }
        );
    }

    #[test]
    fn supervised_permission_policy_when_not_skipping() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let mut cfg = config();
        cfg.dangerously_skip_permissions = false;
        let h = harness(dir.path(), inner, cfg, HAPPY_BODY);
        assert_eq!(h.strategy.invoke_spec().permission_policy, "supervised");
    }

    #[test]
    fn invoke_host_execs_into_the_started_container() {
        // The builder receives a DockerExecHost bound to the container the
        // strategy started and to the strategy's docker program, proven by
        // spawning a plan through it and reading the recorded argv.
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let builder: HostInvokeBuilder = Arc::new(move |host, spec| {
            let sink = Arc::clone(&sink);
            let cwd = spec.working_directory.clone();
            Arc::new(move |request: InvocationRequest| {
                let host = Arc::clone(&host);
                let sink = Arc::clone(&sink);
                let cwd = cwd.clone();
                Box::pin(async move {
                    let mut plan = ProcessPlan::new(vec!["claude".into(), "--print".into()]);
                    plan.cwd = Some(cwd);
                    plan.stdin = StdinPlan::Payload(request.prompt.clone());
                    let mut process = host.spawn(plan).await.unwrap();
                    while let Some(line) = process.next_line().await.unwrap() {
                        sink.lock().unwrap().push(line);
                    }
                    process.wait().await;
                    IterationResult {
                        transcript: String::new(),
                        messages: Vec::new(),
                        envelope: EnvelopeResult::Missing,
                        signals: InvocationSignals::default(),
                        failure: None,
                        usage: None,
                    }
                })
            })
        });
        // The scripted docker echoes exec argv back on stdout.
        let body = r#"case "$1" in exec) echo "$@"; cat >/dev/null ;; run) echo cid ;; esac"#;
        let h = harness_with_builder(dir.path(), inner, config(), body, builder);
        let handle = h.strategy.prepare_sandbox(&request("t")).unwrap();
        let name = h.registry.active_names().into_iter().next().unwrap();
        let invoke = handle.wrap_invoke(None).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(invoke(invocation()));
        let echoed = seen.lock().unwrap().join("\n");
        // The agent runs under the pgid-recording wrapper so a cancel can
        // reach it inside the container.
        assert!(
            echoed.starts_with(&format!("exec -i -w {DEFAULT_WORKDIR} {name} /bin/sh -c ")),
            "{echoed}"
        );
        assert!(echoed.contains(" /tmp/flywheel-agent-"), "{echoed}");
        assert!(echoed.ends_with(".pgid claude --print"), "{echoed}");
    }

    // --- submit delegates -----------------------------------------------------------

    #[test]
    fn submit_and_landability_delegate_to_inner() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, root) = plain(dir.path());
        let h = harness(dir.path(), Arc::clone(&inner), config(), HAPPY_BODY);
        let task = Task::new(
            "goal",
            vec![Grader::Command(CommandGrader::new("true").unwrap())],
        );
        let req = SubmitRequest::new(task, "r1", Status::Done, root.join("alpha"));
        assert_eq!(h.strategy.submit(&req), inner.submit(&req));
        assert!(!h.strategy.submit(&req).is_landed());
        assert_eq!(h.strategy.is_landable(&req), inner.is_landable(&req));
    }

    // --- auth wiring ------------------------------------------------------------------

    #[test]
    fn oauth_auth_reaches_container_start_env() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let mut cfg = config();
        cfg.auth = Some(ClaudeAuth::oauth_token("tok-xyz").unwrap());
        let h = harness(dir.path(), inner, cfg, HAPPY_BODY);
        h.strategy.prepare_sandbox(&request("t")).unwrap();
        let env = env_of(&run_call(&h.log));
        assert_eq!(env[OAUTH_TOKEN_ENV], "tok-xyz");
        assert!(!env.contains_key(API_KEY_ENV));
    }

    #[test]
    fn session_auth_adds_credentials_mount() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let mut cfg = config();
        cfg.auth = Some(ClaudeAuth::session(Some("/host/creds")));
        let h = harness(dir.path(), inner, cfg, HAPPY_BODY);
        h.strategy.prepare_sandbox(&request("t")).unwrap();
        let run = run_call(&h.log);
        let mounts = flag_values(&run, "-v");
        assert_eq!(mounts.len(), 2);
        assert_eq!(
            mounts[1],
            format!("/host/creds:{DEFAULT_AGENT_HOME}/.claude")
        );
        assert_eq!(env_of(&run)["HOME"], DEFAULT_AGENT_HOME);
    }

    #[test]
    fn rejects_subscription_auth_with_api_key_env() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let mut cfg = config();
        cfg.env.insert(API_KEY_ENV.into(), "sk-ant-x".into());
        cfg.auth = Some(ClaudeAuth::oauth_token("tok").unwrap());
        let err = ContainerSubmitStrategy::new(inner, cfg, RecordingBuilder::new().as_builder())
            .err()
            .unwrap();
        assert_eq!(err, AuthError::ApiKeyConflict);
    }

    // --- network wiring ------------------------------------------------------------

    #[test]
    fn deny_policy_creates_and_attaches_internal_network() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let mut cfg = config();
        cfg.network_policy = Some("deny".into());
        let h = harness(dir.path(), inner, cfg, HAPPY_BODY);
        h.strategy.prepare_sandbox(&request("t")).unwrap();
        let calls = recorded_calls(&h.log);
        assert_eq!(
            calls[0],
            [
                "network",
                "inspect",
                DEFAULT_INTERNAL_NETWORK,
                "--format",
                "{{.Id}}"
            ]
        );
        assert_eq!(
            calls[1],
            ["network", "create", "--internal", DEFAULT_INTERNAL_NETWORK]
        );
        assert_eq!(
            flag_values(&run_call(&h.log), "--network"),
            vec![DEFAULT_INTERNAL_NETWORK]
        );
    }

    #[test]
    fn allow_policy_uses_default_bridge_no_creation() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let mut cfg = config();
        cfg.network_policy = Some("allow".into());
        cfg.network = vec!["ignored-raw-override".into()];
        let h = harness(dir.path(), inner, cfg, HAPPY_BODY);
        h.strategy.prepare_sandbox(&request("t")).unwrap();
        let calls = recorded_calls(&h.log);
        assert!(calls.iter().all(|c| c[0] != "network"));
        assert!(flag_values(&run_call(&h.log), "--network").is_empty());
    }

    #[test]
    fn raw_network_override_applies_without_a_policy() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let mut cfg = config();
        cfg.network = vec!["netA".into(), "netB".into()];
        let h = harness(dir.path(), inner, cfg, HAPPY_BODY);
        h.strategy.prepare_sandbox(&request("t")).unwrap();
        assert_eq!(
            flag_values(&run_call(&h.log), "--network"),
            vec!["netA", "netB"]
        );
    }

    #[test]
    fn deny_with_egress_attaches_proxy_network() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let mut cfg = config();
        cfg.network_policy = Some("deny".into());
        cfg.allow_hosts = vec!["api.github.com".into()];
        cfg.egress_network = Some("proxy-net".into());
        let h = harness(dir.path(), inner, cfg, HAPPY_BODY);
        h.strategy.prepare_sandbox(&request("t")).unwrap();
        let calls = recorded_calls(&h.log);
        assert!(calls.iter().all(|c| c[0] != "network"));
        assert_eq!(
            flag_values(&run_call(&h.log), "--network"),
            vec!["proxy-net"]
        );
    }

    #[test]
    fn deny_with_hosts_and_no_egress_fails_prepare_before_starting() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let mut cfg = config();
        cfg.network_policy = Some("deny".into());
        cfg.allow_hosts = vec!["api.github.com".into()];
        let h = harness(dir.path(), inner, cfg, HAPPY_BODY);
        let err = h.strategy.prepare_sandbox(&request("t")).unwrap_err();
        assert!(err.message.contains("egress_network"));
        assert!(recorded_calls(&h.log).is_empty());
    }

    // --- management ceiling reaches every seam (spec 00066 criterion 5) -----------

    #[test]
    fn configured_ceiling_bounds_the_preflight_against_a_hung_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let mut cfg = config();
        cfg.preflight = true;
        cfg.management_timeout = Some(Duration::from_millis(200));
        let h = harness(dir.path(), inner, cfg, HANG_BODY);
        let started = Instant::now();
        let err = h.strategy.prepare_sandbox(&request("t")).unwrap_err();
        assert!(started.elapsed() < WALL_BUDGET);
        assert!(err.message.contains("timed out after 0.2s"));
        assert!(h.registry.active_names().is_empty());
    }

    #[test]
    fn configured_ceiling_bounds_network_ensure_and_teardown() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        // run succeeds; everything else (network inspect, stop) hangs.
        let body = r#"case "$1" in run) echo cid ;; *) exec sleep 30 ;; esac"#;
        let mut cfg = config();
        cfg.management_timeout = Some(Duration::from_millis(200));
        let h = harness(dir.path(), inner, cfg.clone(), body);
        let started = Instant::now();
        let handle = h.strategy.prepare_sandbox(&request("t")).unwrap();
        handle.run_teardown().unwrap();
        assert!(started.elapsed() < WALL_BUDGET);
        let lines = h.lines.lock().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("teardown failed (docker stop timed out after 0.2s)"));
        assert!(h.registry.active_names().is_empty());
        drop(lines);

        let (inner2, _) = plain(dir.path());
        cfg.network_policy = Some("deny".into());
        let h2 = harness(dir.path(), inner2, cfg, body);
        let started = Instant::now();
        let err = h2.strategy.prepare_sandbox(&request("t")).unwrap_err();
        assert!(started.elapsed() < WALL_BUDGET);
        assert!(err.message.contains("docker network timed out"));
    }

    #[test]
    fn exit_time_flush_honors_the_configured_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let body = r#"case "$1" in run) echo cid ;; rm) exec sleep 30 ;; esac"#;
        let mut cfg = config();
        cfg.management_timeout = Some(Duration::from_millis(200));
        let h = harness(dir.path(), inner, cfg, body);
        h.strategy.prepare_sandbox(&request("t")).unwrap();
        let flush_dir = dir.path().join("flush");
        std::fs::create_dir(&flush_dir).unwrap();
        let (docker, _) = scripted_docker(&flush_dir, body);
        let started = Instant::now();
        let outcomes = h.registry.flush(&docker);
        assert!(started.elapsed() < WALL_BUDGET);
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].1.as_ref().unwrap_err().is_timeout());
    }

    #[test]
    fn unbounded_opt_out_flows_through_prepare() {
        let dir = tempfile::tempdir().unwrap();
        let (inner, _) = plain(dir.path());
        let mut cfg = config();
        cfg.preflight = true;
        cfg.network_policy = Some("deny".into());
        cfg.management_timeout = None;
        let h = harness(dir.path(), inner, cfg, HAPPY_BODY);
        assert_eq!(h.strategy.management_timeout(), None);
        let handle = h.strategy.prepare_sandbox(&request("t")).unwrap();
        handle.run_teardown().unwrap();
        let subs: Vec<String> = recorded_calls(&h.log)
            .into_iter()
            .map(|call| call[0].clone())
            .collect();
        assert_eq!(subs, ["image", "network", "network", "run", "stop", "rm"]);
    }

    // --- live: the whole seam against the daemon -------------------------------------

    #[test]
    fn live_prepare_runs_invoke_inside_the_container_and_tears_down() {
        require_docker!();
        let _daemon = live_lock();
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let (inner, root) = plain(dir.path());
        let observed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&observed);
        let builder: HostInvokeBuilder = Arc::new(move |host, spec| {
            let sink = Arc::clone(&sink);
            let cwd = spec.working_directory.clone();
            Arc::new(move |request: InvocationRequest| {
                let host = Arc::clone(&host);
                let sink = Arc::clone(&sink);
                let cwd = cwd.clone();
                Box::pin(async move {
                    let mut plan = ProcessPlan::new(vec![
                        "sh".into(),
                        "-c".into(),
                        "cat; echo; pwd; id -u; echo touched > marker.txt".into(),
                    ]);
                    plan.cwd = Some(cwd);
                    plan.stdin = StdinPlan::Payload(request.prompt.clone());
                    let mut process = host.spawn(plan).await.unwrap();
                    while let Some(line) = process.next_line().await.unwrap() {
                        sink.lock().unwrap().push(line);
                    }
                    let exit = process.wait().await;
                    IterationResult {
                        transcript: sink.lock().unwrap().join("\n"),
                        messages: Vec::new(),
                        envelope: EnvelopeResult::Missing,
                        signals: InvocationSignals::default(),
                        failure: exit.returncode.filter(|code| *code != 0).map(|code| {
                            flywheel_core::invoker::InvocationFailure {
                                error_type: "agent_exit".into(),
                                message: format!("exit {code}"),
                                exit_code: Some(code),
                                stderr: exit.stderr_tail.clone(),
                            }
                        }),
                        usage: None,
                    }
                })
            })
        });
        // SAFETY: getuid/getgid are always-safe libc queries.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let cfg = ContainerConfig {
            container_uid: Some(uid),
            container_gid: Some(gid),
            command: vec!["sleep".into(), "infinity".into()],
            network_policy: Some("deny".into()),
            ..ContainerConfig::new("alpine:latest", "claude-opus-4-8")
        };
        let registry = Arc::new(CleanupRegistry::new());
        let strategy = ContainerSubmitStrategy::new(inner, cfg, builder)
            .unwrap()
            .with_registry(Arc::clone(&registry));
        let handle = strategy.prepare_sandbox(&request("live")).unwrap();
        let name = registry.active_names().into_iter().next().unwrap();
        let docker = Docker::new();
        assert!(docker.list_owned_containers(None).unwrap().contains(&name));

        let invoke = handle.wrap_invoke(None).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(invoke(invocation()));
        assert!(result.failure.is_none(), "{:?}", result.failure);
        let lines = observed.lock().unwrap().clone();
        assert_eq!(
            lines[0], "do the thing",
            "prompt reached the agent on stdin"
        );
        assert_eq!(lines[1], DEFAULT_WORKDIR, "exec ran in the mounted workdir");
        assert_eq!(lines[2], uid.to_string(), "exec ran as the host uid");
        let marker = root.join("live").join("marker.txt");
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "touched");
        assert_eq!(std::fs::metadata(&marker).unwrap().uid(), uid);

        handle.run_teardown().unwrap();
        assert!(!docker.list_owned_containers(None).unwrap().contains(&name));
        assert!(registry.active_names().is_empty());
        let _ = docker.remove_container(&name, None);
    }
}
