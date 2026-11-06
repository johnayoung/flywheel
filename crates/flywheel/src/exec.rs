//! `fw exec`: one agent call, nothing else.
//!
//! Drives a single run through [`AgentRuntime`] under a `LocalHost` and
//! prints the folded [`CompletedRun`]. No task, no store, no worktree, no
//! config: the verb never resolves a project and never writes anything, so
//! any project can shell out to it.
//!
//! Exit codes: 0 when the agent stopped cleanly (terminal envelope seen, no
//! `failure`, no `fault`, `stop.is_error` false); 1 when the run failed
//! (the result is still printed); 2 on a usage error (stderr only, stdout
//! empty). A start-up failure (agent CLI missing, a rejected adapter
//! option) is a run failure: exit 1 with `failure.error_type` set to the
//! agent error code. `--timeout` and SIGINT/SIGTERM cancel the run, which
//! terminates the agent's whole process group; the result carries
//! `failure.error_type` `timeout` or `cancelled`.

use clap::Parser;
use flywheel_agents::{
    AgentConfiguration, AgentRuntime, CompletedRun, RunFailure, RunOptions, RunRequest, claude_code,
};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const EXEC_VERB: &str = "exec";

/// Adapter option the `--system-prompt` flag sets.
pub const SYSTEM_PROMPT_OPTION: &str = "system_prompt";
/// Adapter option the args after `--` set.
pub const EXTRA_CLI_ARGS_OPTION: &str = "extra_cli_args";

const AFTER_HELP: &str = "\
PROMPT '-' or omitted reads the prompt from stdin.

--option KEY=VALUE sets one adapter option (repeatable); VALUE is parsed
as JSON, falling back to a plain string. --system-prompt sets the
'system_prompt' option and args after '--' set 'extra_cli_args'; giving
either one both ways is a usage error.

Exit: 0 clean stop, 1 run failed (result still printed), 2 usage error.";

#[derive(Debug, Parser)]
#[command(
    name = "fw exec",
    about = "Run one agent call: no tasks, store, worktree, or config",
    after_help = AFTER_HELP
)]
struct ExecArgs {
    /// Adapter id (claude-code, codex).
    #[arg(long, value_name = "ID", default_value = claude_code::AGENT_ID)]
    agent: String,
    /// Agent model id.
    #[arg(long, value_name = "M")]
    model: Option<String>,
    /// Agent turn ceiling.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    max_turns: Option<u32>,
    /// Wall-clock ceiling; on expiry the agent's process group is killed.
    #[arg(long, value_name = "SECS", value_parser = parse_timeout)]
    timeout: Option<f64>,
    /// Working directory for the agent [default: the current directory].
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// System prompt (the adapter's `system_prompt` option).
    #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
    system_prompt: Option<String>,
    /// Adapter option KEY=VALUE (VALUE parsed as JSON, else a string).
    #[arg(long = "option", value_name = "KEY=JSON", allow_hyphen_values = true)]
    options: Vec<String>,
    /// Replace the agent executable; repeat to build an argv prefix.
    #[arg(long = "command", value_name = "ARG", allow_hyphen_values = true)]
    command: Vec<String>,
    /// Print the CompletedRun as JSON instead of the final text.
    #[arg(long)]
    json: bool,
    /// The prompt; '-' or omitted reads stdin.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,
    /// Extra agent CLI args (after `--`).
    #[arg(last = true, value_name = "AGENT_ARGS")]
    extra: Vec<String>,
}

fn parse_timeout(raw: &str) -> Result<f64, String> {
    match raw.trim().parse::<f64>() {
        Ok(secs) if secs.is_finite() && secs > 0.0 => Ok(secs),
        _ => Err(format!(
            "expected a positive number of seconds, got '{raw}'"
        )),
    }
}

/// Split `KEY=VALUE`; VALUE is JSON when it parses, else a plain string.
fn parse_option(raw: &str) -> Result<(String, serde_json::Value), String> {
    let Some((key, value)) = raw.split_once('=') else {
        return Err(format!("--option expects KEY=VALUE, got '{raw}'"));
    };
    let key = key.trim();
    if key.is_empty() {
        return Err(format!("--option has an empty key: '{raw}'"));
    }
    let value =
        serde_json::from_str(value).unwrap_or_else(|_| serde_json::Value::String(value.to_owned()));
    Ok((key.to_owned(), value))
}

/// Validate the parsed argv into a run request. Every `Err` is a usage
/// error (exit 2). `stdin` is read only when the prompt is `-` or absent.
fn build_request(
    args: ExecArgs,
    runtime: &AgentRuntime,
    current_dir: &Path,
    stdin: &mut dyn Read,
) -> Result<RunRequest, String> {
    runtime
        .adapter(&args.agent)
        .map_err(|e| format!("unknown agent: {e}"))?;

    let cwd = match &args.cwd {
        Some(dir) if dir.is_absolute() => dir.clone(),
        Some(dir) => current_dir.join(dir),
        None => current_dir.to_path_buf(),
    };
    if !cwd.is_dir() {
        return Err(format!("--cwd {} is not a directory", cwd.display()));
    }
    let cwd = cwd
        .canonicalize()
        .map_err(|e| format!("--cwd {}: {e}", cwd.display()))?;

    let mut configuration = AgentConfiguration::new(args.agent);
    configuration.model_id = args.model.filter(|m| !m.is_empty());
    configuration.max_turns = args.max_turns;
    if !args.command.is_empty() {
        configuration.command_override = Some(args.command);
    }
    for raw in &args.options {
        let (key, value) = parse_option(raw)?;
        configuration.adapter_options.insert(key, value);
    }
    if let Some(system_prompt) = args.system_prompt {
        if configuration
            .adapter_options
            .contains_key(SYSTEM_PROMPT_OPTION)
        {
            return Err(format!(
                "--system-prompt conflicts with --option {SYSTEM_PROMPT_OPTION}=...; give one"
            ));
        }
        configuration.adapter_options.insert(
            SYSTEM_PROMPT_OPTION.to_owned(),
            serde_json::Value::String(system_prompt),
        );
    }
    if !args.extra.is_empty() {
        if configuration
            .adapter_options
            .contains_key(EXTRA_CLI_ARGS_OPTION)
        {
            return Err(format!(
                "args after '--' conflict with --option {EXTRA_CLI_ARGS_OPTION}=...; give one"
            ));
        }
        configuration.adapter_options.insert(
            EXTRA_CLI_ARGS_OPTION.to_owned(),
            serde_json::Value::from(args.extra),
        );
    }

    let prompt = match args.prompt {
        Some(prompt) if prompt != "-" => prompt,
        _ => {
            let mut buffer = String::new();
            stdin
                .read_to_string(&mut buffer)
                .map_err(|e| format!("reading the prompt from stdin: {e}"))?;
            buffer
        }
    };
    if prompt.trim().is_empty() {
        return Err("empty prompt".to_owned());
    }

    Ok(RunRequest {
        prompt,
        working_directory: cwd,
        configuration,
        timeout_seconds: args.timeout,
    })
}

/// A run that never started: the agent error folded into `failure`, so the
/// caller still gets one CompletedRun on stdout.
fn startup_failure(error_type: &str, message: String) -> CompletedRun {
    CompletedRun {
        final_text: String::new(),
        stop: Default::default(),
        usage: None,
        total_cost_usd: None,
        num_turns: None,
        native_session_id: None,
        tool_interactions: Vec::new(),
        permission_denials: Vec::new(),
        rate_limit_events: Vec::new(),
        fault: None,
        failure: Some(RunFailure {
            error_type: error_type.to_owned(),
            message,
            exit_code: None,
            stderr: None,
        }),
        exit: Default::default(),
        event_count: 0,
    }
}

/// Start the run and wait for it; SIGINT/SIGTERM cancel it (the runtime
/// then terminates the agent's process group and folds a `cancelled`
/// failure).
async fn drive(runtime: AgentRuntime, request: RunRequest) -> CompletedRun {
    // Listen before the agent exists: a signal landing mid-start is queued,
    // not lost, and no longer kills fw ahead of the agent.
    let mut shutdown = ShutdownSignals::install();
    let active = match runtime.start(request, RunOptions::default()).await {
        Ok(active) => active,
        Err(e) => return startup_failure(e.code(), e.to_string()),
    };
    let cancel = active.cancel.clone();
    let finish = active.finish();
    tokio::pin!(finish);
    let mut cancelled = false;
    loop {
        tokio::select! {
            run = &mut finish => return run,
            () = shutdown.recv(), if !cancelled => {
                cancelled = true;
                cancel.cancel();
            }
        }
    }
}

/// SIGINT/SIGTERM listeners, registered at construction.
#[cfg(unix)]
struct ShutdownSignals {
    interrupt: Option<tokio::signal::unix::Signal>,
    terminate: Option<tokio::signal::unix::Signal>,
}

#[cfg(unix)]
impl ShutdownSignals {
    fn install() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        Self {
            interrupt: signal(SignalKind::interrupt()).ok(),
            terminate: signal(SignalKind::terminate()).ok(),
        }
    }

    async fn recv(&mut self) {
        async fn next(signal: &mut Option<tokio::signal::unix::Signal>) {
            let received = match signal {
                Some(signal) => signal.recv().await.is_some(),
                None => false,
            };
            if !received {
                // No listener (or its stream closed): never fire.
                std::future::pending::<()>().await;
            }
        }
        tokio::select! {
            () = next(&mut self.interrupt) => {}
            () = next(&mut self.terminate) => {}
        }
    }
}

#[cfg(not(unix))]
struct ShutdownSignals;

#[cfg(not(unix))]
impl ShutdownSignals {
    fn install() -> Self {
        Self
    }

    async fn recv(&mut self) {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// A clean stop: the agent's terminal envelope arrived and nothing failed.
fn run_succeeded(run: &CompletedRun) -> bool {
    run.failure.is_none() && run.fault.is_none() && !run.stop.is_error && run.stop.finished
}

/// One-line stderr diagnostic for a failed run.
fn failure_summary(run: &CompletedRun) -> String {
    if let Some(failure) = &run.failure {
        return format!("{}: {}", failure.error_type, failure.message);
    }
    if let Some(fault) = &run.fault {
        return format!("agent fault {}: {}", fault.kind, fault.message);
    }
    format!(
        "agent stopped with an error (reason {}, subtype {})",
        run.stop.reason,
        run.stop.native_subtype.as_deref().unwrap_or("none")
    )
}

/// `fw exec` against the process's stdin and working directory.
pub fn run_with(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let current_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            let _ = writeln!(err, "fw exec: cannot resolve the current directory: {e}");
            return 2;
        }
    };
    run_in(args, &current_dir, &mut std::io::stdin(), out, err)
}

/// [`run_with`] against an injected working directory and stdin.
pub fn run_in(
    args: &[String],
    current_dir: &Path,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let argv = std::iter::once("fw exec".to_owned()).chain(args.iter().cloned());
    let parsed = match ExecArgs::try_parse_from(argv) {
        Ok(parsed) => parsed,
        Err(e) => {
            let rendered = e.render().to_string();
            if e.use_stderr() {
                let _ = err.write_all(rendered.as_bytes());
                return 2;
            }
            let _ = out.write_all(rendered.as_bytes());
            return 0;
        }
    };
    let json = parsed.json;
    // The default registry under a LocalHost: never a container backend.
    let runtime = AgentRuntime::new();
    let request = match build_request(parsed, &runtime, current_dir, stdin) {
        Ok(request) => request,
        Err(message) => {
            let _ = writeln!(err, "fw exec: {message}");
            return 2;
        }
    };

    let tokio_runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = writeln!(err, "fw exec: cannot start the async runtime: {e}");
            return 1;
        }
    };
    let mut run = tokio_runtime.block_on(drive(runtime, request));
    if run.failure.is_none() && run.fault.is_none() && !run.stop.finished {
        run.failure = Some(RunFailure {
            error_type: "no_result".to_owned(),
            message: "agent exited without its terminal result envelope".to_owned(),
            exit_code: run.exit.returncode,
            stderr: run.exit.stderr_tail.clone(),
        });
    }

    if json {
        match serde_json::to_string(&run) {
            Ok(rendered) => {
                let _ = writeln!(out, "{rendered}");
            }
            Err(e) => {
                let _ = writeln!(err, "fw exec: cannot serialize the run: {e}");
                return 1;
            }
        }
    } else if !run.final_text.is_empty() {
        let _ = out.write_all(run.final_text.as_bytes());
        if !run.final_text.ends_with('\n') {
            let _ = out.write_all(b"\n");
        }
    }
    let _ = out.flush();

    if run_succeeded(&run) {
        0
    } else {
        let _ = writeln!(err, "fw exec: {}", failure_summary(&run));
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(args: &[&str]) -> ExecArgs {
        ExecArgs::try_parse_from(std::iter::once("fw exec").chain(args.iter().copied())).unwrap()
    }

    fn request(args: &[&str], stdin: &str) -> Result<RunRequest, String> {
        let dir = std::env::temp_dir();
        build_request(
            parse(args),
            &AgentRuntime::new(),
            &dir,
            &mut stdin.as_bytes(),
        )
    }

    #[test]
    fn option_values_parse_as_json_then_fall_back_to_string() {
        assert_eq!(parse_option("n=3").unwrap(), ("n".into(), json!(3)));
        assert_eq!(
            parse_option("list=[\"a\",1]").unwrap(),
            ("list".into(), json!(["a", 1]))
        );
        assert_eq!(
            parse_option("effort=high").unwrap(),
            ("effort".into(), json!("high"))
        );
        assert_eq!(parse_option("eq=a=b").unwrap(), ("eq".into(), json!("a=b")));
        assert_eq!(parse_option("empty=").unwrap(), ("empty".into(), json!("")));
        assert!(parse_option("novalue").is_err());
        assert!(parse_option("=1").is_err());
    }

    #[test]
    fn flags_map_onto_the_configuration() {
        let req = request(
            &[
                "--model",
                "m1",
                "--max-turns",
                "3",
                "--timeout",
                "2.5",
                "--system-prompt",
                "be terse",
                "--option",
                "effort=high",
                "--command",
                "/bin/sh",
                "--command",
                "fake.sh",
                "do it",
                "--",
                "--tools",
                "",
            ],
            "",
        )
        .unwrap();
        let cfg = &req.configuration;
        assert_eq!(cfg.agent_id, "claude-code");
        assert_eq!(cfg.model_id.as_deref(), Some("m1"));
        assert_eq!(cfg.max_turns, Some(3));
        assert_eq!(req.timeout_seconds, Some(2.5));
        assert_eq!(cfg.option_str("system_prompt"), Some("be terse"));
        assert_eq!(cfg.option_str("effort"), Some("high"));
        assert_eq!(cfg.option("extra_cli_args"), Some(&json!(["--tools", ""])));
        assert_eq!(
            cfg.command_override,
            Some(vec!["/bin/sh".to_owned(), "fake.sh".to_owned()])
        );
        assert_eq!(req.prompt, "do it");
        assert!(req.working_directory.is_absolute());
    }

    #[test]
    fn prompt_reads_stdin_for_dash_or_absent() {
        assert_eq!(request(&["-"], "from stdin").unwrap().prompt, "from stdin");
        assert_eq!(request(&[], "from stdin").unwrap().prompt, "from stdin");
        assert_eq!(request(&["inline"], "ignored").unwrap().prompt, "inline");
    }

    #[test]
    fn usage_errors() {
        assert!(request(&[], "  \n").unwrap_err().contains("empty prompt"));
        assert!(
            request(&["--agent", "nope", "p"], "")
                .unwrap_err()
                .contains("unknown agent")
        );
        assert!(
            request(&["--cwd", "/definitely/not/here", "p"], "")
                .unwrap_err()
                .contains("not a directory")
        );
        assert!(request(&["--option", "bad", "p"], "").is_err());
        assert!(
            request(
                &["--system-prompt", "a", "--option", "system_prompt=b", "p"],
                ""
            )
            .unwrap_err()
            .contains("conflicts")
        );
        assert!(
            request(&["--option", "extra_cli_args=[]", "p", "--", "-x"], "")
                .unwrap_err()
                .contains("conflict")
        );
        for bad in [
            &["--max-turns", "0", "p"][..],
            &["--max-turns", "x", "p"],
            &["--timeout", "-1", "p"],
            &["--timeout", "inf", "p"],
            &["--timeout", "abc", "p"],
        ] {
            let argv = std::iter::once("fw exec").chain(bad.iter().copied());
            assert!(ExecArgs::try_parse_from(argv).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn success_requires_a_clean_finished_stop() {
        let mut run = startup_failure("x", "y".into());
        assert!(!run_succeeded(&run));
        run.failure = None;
        assert!(!run_succeeded(&run), "unfinished stop is not clean");
        run.stop.finished = true;
        assert!(run_succeeded(&run));
        run.stop.is_error = true;
        assert!(!run_succeeded(&run));
    }
}
