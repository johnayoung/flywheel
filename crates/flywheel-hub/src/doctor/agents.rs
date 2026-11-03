//! Agents: `claude` and `codex` on the hub's `PATH`, and the hook settings
//! the hub hands Claude.

use super::{Check, Level, PROBE_TIMEOUT};
use crate::agent::{self, SessionKind};
use crate::review::runner::{self, Spec};
use std::ffi::OsStr;
use std::path::Path;

/// Run `program args` (bounded), from `/`.
pub(crate) fn bounded(program: &Path, args: &[&str]) -> Option<runner::Output> {
    let program = program.to_string_lossy();
    runner::run(&Spec {
        program: &program,
        dir: Path::new("/"),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        stdin: None,
        timeout: PROBE_TIMEOUT,
        limit: 64 * 1024,
    })
    .ok()
}

pub(crate) fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_owned()
}

pub(crate) fn agent_check(
    kind: SessionKind,
    path: Option<&OsStr>,
    hub_path: Option<&OsStr>,
) -> Check {
    let name = kind.as_str();
    let (id, install) = match kind {
        SessionKind::Codex => ("codex", "npm install -g @openai/codex"),
        _ => (
            "claude",
            "install Claude Code (https://docs.claude.com/en/docs/claude-code)",
        ),
    };
    let Some(program) = agent::resolve_program(name, path) else {
        // Codex is optional; Claude Code is the main agent.
        let level = if kind == SessionKind::Codex {
            Level::Info
        } else {
            Level::Warn
        };
        return Check::new(
            id,
            level,
            format!("`{name}` not found on PATH: {name} sessions cannot start"),
        )
        .fix(format!("{install}, or ignore it if you do not run {name}"));
    };
    let mut check = match bounded(&program, &["--version"]) {
        Some(out) if out.success() => Check::new(
            id,
            Level::Ok,
            format!("{} ({})", program.display(), first_line(&out.stdout_text())),
        ),
        Some(out) => Check::new(
            id,
            Level::Fail,
            format!(
                "{} --version failed: {}",
                program.display(),
                first_line(&out.error_text())
            ),
        )
        .fix(format!("reinstall it ({install})")),
        None => Check::new(
            id,
            Level::Fail,
            format!("{} could not be run", program.display()),
        )
        .fix(format!("reinstall it ({install})")),
    };
    // The running hub resolves agents with its own environment.
    if let Some(hub_path) = hub_path {
        match agent::resolve_program(name, Some(hub_path)) {
            None => {
                check.level = check.level.max(Level::Warn);
                check = check
                    .detail(format!("the running hub's PATH has no `{name}`"))
                    .fix(
                        "restart the hub from a shell where it is on PATH (`fw hub stop`, then \
                         `fw hub start`; sessions are restored)",
                    );
            }
            Some(theirs) if theirs != program => {
                check = check.detail(format!("the running hub runs {}", theirs.display()));
            }
            Some(_) => {}
        }
    }
    check
}

/// Whether Claude Code takes the hub's `--settings` (status hooks), and
/// whether the hooks' command answers.
pub(crate) fn hooks_check(claude: Option<&Path>, exe: Option<&Path>) -> Check {
    let id = "claude hooks";
    let Some(exe) = exe else {
        return Check::new(id, Level::Warn, "cannot locate this program for the hooks");
    };
    let report = vec![exe.to_string_lossy().into_owned(), "hub".to_owned()];
    let reporter = bounded(exe, &["hub", "report", "--help"]);
    if !reporter.as_ref().is_some_and(runner::Output::success) {
        return Check::new(
            id,
            Level::Fail,
            format!(
                "the hooks' command `{} hub report` does not run",
                exe.display()
            ),
        )
        .fix("reinstall flywheel (`cargo install --path crates/flywheel`)");
    }
    let Some(claude) = claude else {
        return Check::new(id, Level::Info, "skipped (no claude)");
    };
    let value = agent::claude_settings(&report);
    if let Err(problem) = hooks_shape(&value) {
        return Check::new(
            id,
            Level::Fail,
            format!("the hub's --settings are malformed: {problem}"),
        )
        .fix("a flywheel bug: reinstall a released build and report it");
    }
    let events = value["hooks"].as_object().map_or(0, serde_json::Map::len);
    // `--version` makes claude read --settings as JSON and stop; it checks
    // hook entries only when a session starts, and has no dry run. So this
    // shows the JSON is taken and well-formed, not that every hook loads.
    match bounded(claude, &["--settings", &value.to_string(), "--version"]) {
        Some(out) if out.success() => Check::new(
            id,
            Level::Ok,
            format!(
                "claude takes the hub's --settings JSON ({events} hook events, each running `{} \
                 hub report`)",
                exe.display()
            ),
        )
        .detail(
            "claude checks hook entries only when a session starts: a Claude session's dot \
             turning to working on its first prompt confirms them",
        ),
        Some(out) => Check::new(
            id,
            Level::Fail,
            format!(
                "claude refused the hub's --settings: {}",
                first_line(&out.error_text())
            ),
        )
        .fix("update Claude Code (`claude update`); status would fall back to screen reading"),
        None => Check::new(id, Level::Warn, "claude --settings did not answer in time")
            .fix("run `claude --version` by hand: a first run may wait on a prompt or an update"),
    }
}

/// The shape Claude Code's settings give hooks: `hooks` maps each event to
/// matcher groups, each a `hooks` list of `{"type":"command","command"}`.
pub fn hooks_shape(settings: &serde_json::Value) -> Result<(), String> {
    let hooks = settings["hooks"]
        .as_object()
        .ok_or("`hooks` is not an object")?;
    if hooks.is_empty() {
        return Err("no hook events".into());
    }
    for (event, groups) in hooks {
        let groups = groups
            .as_array()
            .ok_or(format!("`{event}` is not a list"))?;
        for group in groups {
            if group.get("matcher").is_some_and(|m| !m.is_string()) {
                return Err(format!("a `{event}` matcher is not a string"));
            }
            let entries = group["hooks"]
                .as_array()
                .filter(|e| !e.is_empty())
                .ok_or(format!("a `{event}` group has no `hooks` list"))?;
            for entry in entries {
                if entry["type"] != "command" {
                    return Err(format!("a `{event}` hook is not of type command"));
                }
                if entry["command"].as_str().is_none_or(str::is_empty) {
                    return Err(format!("a `{event}` hook has no command"));
                }
                if entry.get("args").is_some_and(|a| {
                    !a.as_array()
                        .is_some_and(|a| a.iter().all(serde_json::Value::is_string))
                }) {
                    return Err(format!("a `{event}` hook's args are not strings"));
                }
                if entry.get("timeout").is_some_and(|t| !t.is_u64()) {
                    return Err(format!("a `{event}` hook's timeout is not a number"));
                }
            }
        }
    }
    Ok(())
}
