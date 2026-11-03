//! `fw hub plugin ls|enable|disable|logs`: the plugins in
//! `<state-dir>/plugins/`, which are enabled (`plugins.json`), and their
//! run logs. Works with or without a running hub (it re-reads both
//! within seconds).

use crate::cli::{StateDirArg, resolve_state_dir};
use crate::plugins;
use crate::util::fail;
use serde_json::{Value, json};
use std::io::Write;

#[derive(Debug, clap::Args)]
pub(crate) struct PluginArgs {
    #[command(subcommand)]
    command: PluginCommand,
}

#[derive(Debug, clap::Subcommand)]
enum PluginCommand {
    /// The plugins found, enabled or not, with their events, actions,
    /// and last run.
    Ls(LsArgs),
    /// Let a plugin run (on its events and as palette actions).
    Enable(NameArgs),
    /// Stop a plugin from running.
    Disable(NameArgs),
    /// A plugin's newest runs: trigger, exit, time, error, output.
    Logs(LogsArgs),
}

#[derive(Debug, clap::Args)]
struct LsArgs {
    /// Print JSON: {"dir","plugins":[...]}.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

#[derive(Debug, clap::Args)]
struct NameArgs {
    /// The plugin (its directory name).
    name: String,
    #[command(flatten)]
    state: StateDirArg,
}

#[derive(Debug, clap::Args)]
struct LogsArgs {
    /// The plugin (its directory name).
    name: String,
    /// How many runs (newest last).
    #[arg(short = 'n', long, default_value_t = 20)]
    lines: usize,
    /// Print JSON lines (one run each).
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn main(args: &PluginArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let state = match &args.command {
        PluginCommand::Ls(a) => &a.state,
        PluginCommand::Enable(a) | PluginCommand::Disable(a) => &a.state,
        PluginCommand::Logs(a) => &a.state,
    };
    let dir = match resolve_state_dir(state) {
        Ok(dir) => dir,
        Err(message) => return fail(err, &message),
    };
    match &args.command {
        PluginCommand::Ls(a) => {
            let found = plugins::scan(&dir);
            let views: Vec<Value> = found
                .iter()
                .map(|p| plugins::plugin_view(&dir, p))
                .collect();
            if a.json {
                let doc = json!({"dir": dir.join(plugins::PLUGINS_DIR).to_string_lossy(), "plugins": views});
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(&doc).unwrap_or_default()
                );
                return 0;
            }
            if views.is_empty() {
                let _ = writeln!(
                    out,
                    "no plugins in {} (one per directory, each with a plugin.toml)",
                    dir.join(plugins::PLUGINS_DIR).display()
                );
                return 0;
            }
            for view in views {
                print_plugin(&view, out);
            }
            0
        }
        PluginCommand::Enable(a) | PluginCommand::Disable(a) => {
            let on = matches!(args.command, PluginCommand::Enable(_));
            let found = plugins::scan(&dir);
            let Some(plugin) = found.iter().find(|p| p.name == a.name) else {
                return fail(
                    err,
                    &format!(
                        "no plugin {:?} in {}",
                        a.name,
                        dir.join(plugins::PLUGINS_DIR).display()
                    ),
                );
            };
            if on && let Err(e) = &plugin.manifest {
                let _ = writeln!(
                    err,
                    "fw hub: warning: {}'s plugin.toml is invalid ({e}); it runs once fixed",
                    a.name
                );
            }
            if let Err(message) = plugins::set_enabled(&dir, &a.name, on) {
                return fail(err, &message);
            }
            let _ = writeln!(
                out,
                "{} {} (a running hub picks it up within 2 s)",
                a.name,
                if on { "enabled" } else { "disabled" }
            );
            0
        }
        PluginCommand::Logs(a) => {
            if !plugins::valid_name(&a.name) {
                return fail(err, &format!("{:?} is not a plugin name", a.name));
            }
            let runs = plugins::read_logs(&dir, &a.name, a.lines.max(1));
            if a.json {
                for run in runs {
                    let _ = writeln!(out, "{}", serde_json::to_string(&run).unwrap_or_default());
                }
                return 0;
            }
            if runs.is_empty() {
                let _ = writeln!(out, "{}: no runs yet", a.name);
                return 0;
            }
            for run in runs {
                let how = if run.timed_out {
                    "timed out".to_owned()
                } else {
                    match (run.exit_code, run.signal) {
                        (Some(code), _) => format!("exit {code}"),
                        (None, Some(sig)) => format!("signal {sig}"),
                        _ => "not run".to_owned(),
                    }
                };
                let _ = writeln!(
                    out,
                    "{}  {}  {how}  {} ms{}",
                    run.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    run.trigger,
                    run.duration_ms,
                    run.error
                        .as_deref()
                        .map(|e| format!("  error: {e}"))
                        .unwrap_or_default()
                );
                for (label, text) in [("stdout", &run.stdout), ("stderr", &run.stderr)] {
                    for line in text.lines().take(10) {
                        let _ = writeln!(out, "    {label}: {line}");
                    }
                }
            }
            0
        }
    }
}

fn print_plugin(view: &Value, out: &mut dyn Write) {
    let name = view["name"].as_str().unwrap_or("");
    let state = if view["valid"].as_bool() != Some(true) {
        "invalid"
    } else if view["enabled"].as_bool() == Some(true) {
        "enabled"
    } else {
        "disabled"
    };
    let description = view["description"].as_str().unwrap_or("");
    let _ = writeln!(out, "{name}  [{state}]  {description}");
    if let Some(error) = view["error"].as_str() {
        let _ = writeln!(out, "    plugin.toml: {error}");
    }
    let events: Vec<String> = view["events"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let statuses: Vec<&str> = e["statuses"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let on = e["on"].as_str().unwrap_or("");
            if statuses.is_empty() {
                on.to_owned()
            } else {
                format!("{on}({})", statuses.join("|"))
            }
        })
        .collect();
    if !events.is_empty() {
        let _ = writeln!(out, "    events: {}", events.join(", "));
    }
    let actions: Vec<String> = view["actions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| {
            format!(
                "{} ({})",
                a["title"].as_str().unwrap_or(""),
                a["id"].as_str().unwrap_or("")
            )
        })
        .collect();
    if !actions.is_empty() {
        let _ = writeln!(out, "    actions: {}", actions.join(", "));
    }
    if let Some(run) = view["last_run"].as_object() {
        let _ = writeln!(
            out,
            "    last run: {} {} {}",
            run.get("at").and_then(Value::as_str).unwrap_or(""),
            run.get("trigger").and_then(Value::as_str).unwrap_or(""),
            if run.get("ok").and_then(Value::as_bool) == Some(true) {
                "ok".to_owned()
            } else {
                format!(
                    "failed: {}",
                    run.get("error").and_then(Value::as_str).unwrap_or("")
                )
            }
        );
    }
}
