//! `fw hub serve`: open the hub, restore, listen, and shut down cleanly.

#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn serve_cmd(
    args: ServeArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
    extensions: &HubExtensions,
) -> i32 {
    // Detached (`fw hub start`): everything this process writes to stdout
    // and stderr goes to the rotating log from here on. Declared first so
    // it is dropped last, after the final message.
    #[cfg(unix)]
    let _log = match &args.log_file {
        Some(path) => match crate::daemon::LogRedirect::install(path) {
            Ok(redirect) => Some(redirect),
            Err(e) => {
                let _ = writeln!(err, "fw hub: cannot log to {}: {e}", path.display());
                return 1;
            }
        },
        None => None,
    };
    #[cfg(not(unix))]
    if args.log_file.is_some() {
        let _ = writeln!(err, "fw hub: running detached is supported on unix only");
        return 1;
    }
    let detached = args.log_file.is_some();
    if detached {
        let _ = writeln!(
            out,
            "=== flywheel hub (pid {}) starting at {} ===",
            std::process::id(),
            Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
        );
    }
    let hub_args = args.hub;
    let state_dir = match resolve_state_dir(&hub_args.state) {
        Ok(dir) => dir,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            return 1;
        }
    };
    if let Err(message) = hub_args.public_url_origins() {
        let _ = writeln!(err, "fw hub: {message}");
        return 2;
    }
    if let Err(e) = crate::paths::ensure_private_dir(&state_dir) {
        let _ = writeln!(err, "fw hub: state dir {}: {e}", state_dir.display());
        return 1;
    }
    // `<state-dir>/hub.toml` under the flags given (flag > file > default).
    let config = match crate::config::ConfigWatch::open(
        &state_dir,
        hub_args.flags(),
        crate::config::home_dir(),
    ) {
        Ok(config) => Arc::new(config),
        Err(message) => {
            let _ = writeln!(
                err,
                "fw hub: {message}\nfw hub: fix it (`fw hub config check`) or move it aside; not starting"
            );
            return 2;
        }
    };
    let effective = config.started().clone();
    let public_urls = effective.public_urls.clone();
    crate::install_crypto_provider();
    let notify = match NotifyConfig::load(&state_dir, hub_args.notify_url.as_deref()) {
        Ok(config) => config,
        Err(message) => {
            let _ = writeln!(err, "fw hub: push notifications: {message}");
            return 2;
        }
    };
    let mut options = HubOptions::new(state_dir);
    options.scrollback_lines = effective.scrollback_lines;
    options.report_command = report_command();
    options.agent_resume = effective.agent_resume;
    options.worktree_branch_prefix = effective.worktree_branch_prefix.clone();
    options.import_root = effective.import_root.clone();
    options.agent_defaults = effective.agent_defaults.clone();
    options.record_limits = effective.record_limits;
    options.share_policy = effective.share_policy;
    options.reviewer = effective.reviewer;
    options.queue_typing_quiet = effective.queue_typing_quiet;
    options.login_days = effective.login_days;
    // One hub per state dir: the hub holds `<state-dir>/serve.lock` for
    // its whole life (a second would overwrite the first's runtime file and
    // report socket, leaving `url` / `token --rotate` and agent hooks unable
    // to reach the first).
    let serve_args = hub_args.to_argv(
        &std::path::absolute(&options.state_dir).unwrap_or_else(|_| options.state_dir.clone()),
    );
    let mut taken_over = None;
    let opened = if args.handoff {
        let step = args.handoff_timeout.map(Duration::from_secs);
        match crate::upgrade::take_over(options, step, err) {
            Ok((hub, taken)) => {
                taken_over = Some(taken);
                Ok(hub)
            }
            Err(code) => return code,
        }
    } else {
        Hub::open(options)
    };
    let hub = match opened {
        Ok(hub) => hub,
        Err(HubError::InUse(dir)) => {
            let at = RuntimeInfo::read(&dir)
                .ok()
                .flatten()
                .map(|info| format!(" (pid {}, {})", info.pid, info.listen))
                .unwrap_or_default();
            let _ = writeln!(
                err,
                "fw hub: another hub is already serving state dir {}{at}; stop it or pass a different --state-dir",
                dir.display()
            );
            return 1;
        }
        Err(e) => {
            let _ = writeln!(err, "fw hub: {e}");
            return 1;
        }
    };
    // Hub actions become audit rows in their projects' stores.
    if let Some(sink) = &extensions.audit
        && let Err(e) = hub.set_audit_sink(Arc::clone(sink))
    {
        let _ = writeln!(err, "fw hub: hub actions will not be audited: {e}");
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(runtime_workers())
        .thread_name("hub-http")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = writeln!(err, "fw hub: failed to start the async runtime: {e}");
            return 1;
        }
    };
    let bind = SocketAddr::new(effective.bind, effective.port);
    let listening = match taken_over.as_mut() {
        Some(taken) => taken.listener(&runtime),
        None => runtime.block_on(tokio::net::TcpListener::bind(bind)),
    };
    let listener = match listening {
        Ok(listener) => listener,
        Err(e) => {
            let _ = writeln!(err, "fw hub: cannot listen on {bind}: {e}");
            return 1;
        }
    };
    let local = listener.local_addr().unwrap_or(bind);
    if !effective.bind.is_loopback() {
        let advice = if effective.tls.is_on() {
            "Traffic from the network is https (--tls); plain http from it reaches only /ca."
        } else {
            "Traffic is plain http: prefer the default loopback bind plus `tailscale serve`, \
             or add --tls auto."
        };
        let _ = writeln!(
            err,
            "fw hub: WARNING: listening on {local}, reachable beyond this machine. \
             Anyone with the token gets a shell. {advice}"
        );
    }
    // Taken before anything is restored: a SIGTERM from here on (`fw hub
    // start` giving up, `systemctl stop`) shuts down gracefully -- every
    // restored session recorded stopped again -- instead of killing the
    // hub mid-restore and leaving them recorded running (a crash).
    let shutdown = {
        let _enter = runtime.enter();
        match ShutdownSignal::install() {
            Ok(signal) => signal,
            Err(e) => {
                let _ = writeln!(err, "fw hub: cannot handle shutdown signals: {e}");
                return 1;
            }
        }
    };
    // A handoff completes first: the old hub is frozen until then.
    let handed_over = match taken_over.take() {
        Some(taken) => match taken.finish(&hub) {
            Ok(said) => Some(said),
            Err(message) => {
                let _ = writeln!(err, "fw hub: {message}");
                return 1;
            }
        },
        None => None,
    };
    let tailscale = tailscale_program().and_then(|p| detect_tailscale(&p, local.port()));
    // Built-in TLS: the CA and a leaf for every name this hub is reached
    // at, before anything listens for it.
    let tls = match &effective.tls {
        crate::tls::TlsMode::Off => None,
        mode => {
            let names = crate::tls::hub_names(
                local,
                &crate::machines::host_name(),
                &interface_addresses(),
                tailscale.as_ref(),
                &effective.tls_names,
            );
            match crate::tls::TlsServing::start(
                mode,
                hub.state_dir(),
                local,
                &names,
                &effective.tls_names,
            ) {
                Ok(tls) => tls,
                Err(e) => {
                    let _ = writeln!(err, "fw hub: tls: {e}");
                    return 1;
                }
            }
        }
    };
    let served = match (&tls, &effective.tls) {
        (None, _) => Served::Http,
        (Some(_), crate::tls::TlsMode::Auto) => Served::Https { ca_page: true },
        (Some(_), _) => Served::Https { ca_page: false },
    };
    let base = format!("http://{}", browse_host(local));
    if local.ip().is_unspecified() {
        let _ = writeln!(
            out,
            "flywheel hub listening on {local} (all interfaces; {base} from this machine)"
        );
    } else {
        let _ = writeln!(out, "flywheel hub listening on {base}");
    }
    let _ = writeln!(out, "state dir: {}", hub.state_dir().display());
    if let Some(tls) = &tls {
        print_tls(out, err, tls, local);
    }
    if let Some(said) = handed_over {
        // Sessions came along running: nothing to restore.
        let _ = writeln!(out, "{said}");
    } else if !effective.restore {
        match hub.forgo_restore() {
            Ok(0) => {
                let _ = writeln!(out, "session restore off (--no-restore / sessions.restore)");
            }
            Ok(n) => {
                let _ = writeln!(
                    out,
                    "session restore off (--no-restore / sessions.restore): {n} session{} stay{} stopped until relaunched",
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "s" } else { "" }
                );
            }
            Err(e) => {
                let _ = writeln!(err, "fw hub: cannot record --no-restore: {e}");
            }
        }
    } else {
        report_restore(&hub.restore_sessions(), out, err);
    }
    if detached || args.no_login_code {
        // One-time codes stay out of log files and journals: `fw hub
        // start` printed one to the operator's terminal.
        let _ = writeln!(
            out,
            "log in with a one-time URL from `fw hub url` (or `fw hub url --qr` for a phone)"
        );
    } else {
        let targets = login_targets_served(
            local,
            &public_urls,
            tailscale.as_ref(),
            &local_interfaces(local),
            served,
        );
        match hub.mint_login_code() {
            Ok(code) => print_login(out, err, &targets, &code, auto_qr(&targets)),
            Err(e) => {
                let _ = writeln!(err, "fw hub: cannot mint a login code: {e}");
            }
        }
        print_tailscale_hint(err, tailscale.as_ref(), &targets, local.port());
    }
    let _ = out.flush();

    let gate = crate::handoff::Gate::new();
    let handoff_server = {
        use std::os::fd::AsRawFd;
        crate::handoff::HandoffServer::start(crate::handoff::Source {
            hub: Arc::clone(&hub),
            gate: gate.clone(),
            listener_fd: listener.as_raw_fd(),
            listen: local,
            serve_args,
            log_file: args.log_file.clone(),
        })
    };
    let handoff_server = match handoff_server {
        Ok(server) => Some(server),
        Err(e) => {
            let _ = writeln!(
                err,
                "fw hub: live handoff off (`fw hub upgrade` will restart instead): {e}"
            );
            None
        }
    };
    // Written last: `fw hub start` treats it as the hub being ready.
    let runtime_info = RuntimeInfo {
        pid: std::process::id(),
        listen: local,
        public_urls: public_urls.clone(),
        started_at: Some(Utc::now()),
        log_file: args.log_file.clone(),
        tls: match served {
            Served::Http => None,
            Served::Https { ca_page: true } => Some("auto".to_owned()),
            Served::Https { ca_page: false } => Some("files".to_owned()),
        },
    };
    if let Err(e) = runtime_info.write(hub.state_dir()) {
        let _ = writeln!(
            err,
            "fw hub: cannot write the runtime file (`fw hub url`, `status`, `stop`, and the \
             scripting verbs will not find this hub): {e}"
        );
    }
    // Where a push's or digest's link opens the hub: where a phone reaches it.
    let link_base = public_urls
        .first()
        .or_else(|| tailscale.as_ref().and_then(|ts| ts.serve_urls.first()))
        .cloned();
    let notifier = match notify {
        Some(mut config) => {
            config.click_base.clone_from(&link_base);
            match Notifier::new(config) {
                Ok(notifier) => {
                    let _enter = runtime.enter();
                    notifier.watch(&hub);
                    let config = notifier.config();
                    let _ = writeln!(
                        out,
                        "push notifications: {} -> {} (debounce {}s)",
                        config.events().join(", "),
                        config.host(),
                        config.debounce.as_secs()
                    );
                    Some(notifier)
                }
                Err(e) => {
                    let _ = writeln!(err, "fw hub: push notifications off: {e}");
                    None
                }
            }
        }
        None => None,
    };
    let mut allowed_origins = effective.allow_origins.clone();
    allowed_origins.extend(public_urls);
    if let Some(ts) = &tailscale {
        allowed_origins.extend(ts.serve_urls.iter().cloned());
    }
    let flywheel = match (&extensions.flywheel, effective.flywheel) {
        (Some(provider), true) => {
            let monitor = FlywheelMonitor::new(Arc::clone(provider), MonitorTiming::default());
            match monitor.watch(&hub) {
                Ok(()) => Some(monitor),
                Err(e) => {
                    let _ = writeln!(err, "fw hub: Flywheel panel off: {e}");
                    None
                }
            }
        }
        _ => None,
    };
    let review_tools = crate::review::Tools::default();
    hub.pr_watch().set_tools(review_tools.clone());
    let fanouts =
        crate::fanout::Fanouts::new(&hub, extensions.graders.clone(), review_tools.clone());
    let fanouts = match fanouts.start() {
        Ok(()) => Some(fanouts),
        Err(e) => {
            let _ = writeln!(err, "fw hub: fan-outs off: {e}");
            None
        }
    };
    // Plugins: local executables on hub events and as palette actions.
    let plugins = Arc::new(crate::plugins::PluginHost::new(
        hub.state_dir(),
        crate::plugins::HostOptions {
            max_concurrent: effective.plugins_max_concurrent,
            base_env: std::env::vars_os().collect(),
            enabled: effective.plugins_enabled,
        },
    ));
    {
        let _enter = runtime.enter();
        plugins.watch(&hub);
    }
    // Scheduled digests: hub.toml [digest], read on every look.
    let digest = crate::digest::DigestService::new(
        hub.state_dir(),
        {
            let config = Arc::clone(&config);
            move || config.current().digest
        },
        notifier.clone(),
        flywheel.clone(),
        link_base,
    );
    digest.set_plugins(Arc::clone(&plugins));
    {
        let _enter = runtime.enter();
        digest.start(&hub);
    }
    let schedule = digest.config();
    if schedule.enabled() {
        let _ = writeln!(
            out,
            "digests: {} {} -> {}",
            schedule.times_text().join(", "),
            schedule.timezone.name(),
            schedule.channels.names().join(", ")
        );
    }
    // hub.toml edits: live keys in force at once, the UI told.
    if let Err(e) = config.attach(&hub, Some(Arc::clone(&plugins))) {
        let _ = writeln!(
            err,
            "fw hub: {} will not be re-read while running: {e}",
            config.path().display()
        );
    }
    let server_options = ServerOptions {
        allowed_origins,
        trusted_proxies: effective.trusted_proxies.clone(),
        notifier,
        digest: Some(digest),
        flywheel,
        review_tools,
        fanouts,
        config: Some(Arc::clone(&config)),
        plugins: Some(plugins),
        tls: tls.clone(),
        ws_send_timeout: None,
    };
    if let Some(tls) = &tls {
        let _enter = runtime.enter();
        tls.watch();
    }
    let state_dir = hub.state_dir().to_path_buf();
    let result = runtime.block_on(serve_gated(
        crate::handoff::GatedListener::new(listener, gate),
        Arc::clone(&hub),
        server_options,
        shutdown.wait(),
    ));
    if let Some(server) = &handoff_server {
        server.remove();
    }
    // Still under the serve lock (the hub holds it until dropped).
    RuntimeInfo::remove_if_owned(&state_dir, runtime_info.pid);
    drop(hub);
    match result {
        Ok(()) => {
            let _ = writeln!(
                out,
                "flywheel hub stopped at {}",
                Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
            );
            0
        }
        Err(e) => {
            let _ = writeln!(err, "fw hub: server error: {e}");
            1
        }
    }
}

/// Say what restoring the last clean shutdown's sessions did.
pub(super) fn report_restore(
    outcomes: &[RestoreOutcome],
    out: &mut dyn Write,
    err: &mut dyn Write,
) {
    for outcome in outcomes {
        match &outcome.result {
            Ok(how) => {
                let note = outcome
                    .note
                    .as_deref()
                    .map(|n| format!("; {n}"))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "restored session {:?} ({}) in {} ({}{note})",
                    outcome.name,
                    outcome.id,
                    outcome.cwd.display(),
                    how.as_str()
                );
            }
            Err(message) => {
                let _ = writeln!(
                    err,
                    "fw hub: cannot restore session {:?} ({}): {message}; it stays stopped",
                    outcome.name, outcome.id
                );
            }
        }
    }
}

/// Ctrl-C or SIGTERM, hooked when installed (so either one arriving
/// before the server runs is held, not fatal) and awaited by the server.
pub(super) struct ShutdownSignal {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
}

impl ShutdownSignal {
    /// Hook the signals; call inside the runtime.
    fn install() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                term: signal(SignalKind::terminate())?,
                interrupt: signal(SignalKind::interrupt())?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    /// Resolves on the first of them (including one that arrived earlier).
    async fn wait(self) {
        #[cfg(unix)]
        {
            let Self {
                mut term,
                mut interrupt,
            } = self;
            tokio::select! {
                _ = term.recv() => {}
                _ = interrupt.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self;
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
