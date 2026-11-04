//! `fw hub config check|show`: validate `<state-dir>/hub.toml`, and print
//! the settings in force with where each came from.

use crate::cli::{StateDirArg, resolve_state_dir};
use crate::config::{self, CONFIG_FILE};
use crate::endpoint::Endpoint;
use crate::util::{fail, print_json};
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, clap::Args)]
pub(crate) struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, clap::Subcommand)]
enum ConfigCommand {
    /// Validate hub.toml (or --file): exit 0 valid or absent, 1 invalid
    /// (or a --file that does not exist).
    Check(CheckArgs),
    /// The settings in force: every key, its value, and whether it came
    /// from a flag, hub.toml, or the default (the running hub's view;
    /// without one, the file over the defaults).
    Show(ShowArgs),
}

#[derive(Debug, clap::Args)]
struct CheckArgs {
    /// A file to check instead of <state-dir>/hub.toml.
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    #[command(flatten)]
    state: StateDirArg,
}

#[derive(Debug, clap::Args)]
struct ShowArgs {
    /// Print JSON: {"file":{"path","exists","error"},"values":[{"key",
    /// "value","source","apply"}],"pending_restart":[keys],"running"}.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn main(args: &ConfigArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match &args.command {
        ConfigCommand::Check(args) => check(args, out, err),
        ConfigCommand::Show(args) => show(args, out, err),
    }
}

fn check(args: &CheckArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let path = match &args.file {
        Some(path) => path.clone(),
        None => match resolve_state_dir(&args.state) {
            Ok(dir) => dir.join(CONFIG_FILE),
            Err(message) => return fail(err, &message),
        },
    };
    let file = match config::read_file(&path) {
        Ok(Some(file)) => file,
        Ok(None) if args.file.is_some() => {
            // Named on purpose: a missing file is a mistyped path.
            let _ = writeln!(err, "{}: no such file", path.display());
            return 1;
        }
        Ok(None) => {
            let _ = writeln!(
                out,
                "{}: none (every setting is its default)",
                path.display()
            );
            return 0;
        }
        Err(message) => {
            let _ = writeln!(err, "{}: {message}", path.display());
            return 1;
        }
    };
    let home = config::home_dir();
    match config::resolve(&file, &config::Flags::default(), home.as_deref()) {
        Ok(effective) => {
            let set = effective
                .entries()
                .iter()
                .filter(|e| e.source == config::Source::File)
                .count();
            let _ = writeln!(
                out,
                "{}: ok ({set} setting{} from the file)",
                path.display(),
                if set == 1 { "" } else { "s" }
            );
            0
        }
        Err(errors) => {
            for e in errors {
                let _ = writeln!(err, "{}: {e}", path.display());
            }
            1
        }
    }
}

fn show(args: &ShowArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let dir = match resolve_state_dir(&args.state) {
        Ok(dir) => dir,
        Err(message) => return fail(err, &message),
    };
    let running = match Endpoint::running(&dir) {
        Ok(Some(hub)) => Some(hub.with_timeout(Duration::from_secs(5)).blocking_call(
            "GET",
            "/api/config",
            None,
        )),
        Ok(None) => None,
        Err(message) => return fail(err, &message),
    };
    let view = match running {
        Some(Ok(view)) => view,
        Some(Err(message)) => return fail(err, &format!("the running hub: {message}")),
        None => match config::offline_view(&dir, config::home_dir().as_deref()) {
            Ok(view) => view,
            Err(message) => return fail(err, &message),
        },
    };
    if args.json {
        print_json(out, &view);
        return 0;
    }
    print_view(&view, &dir, out);
    0
}

fn print_view(view: &Value, dir: &std::path::Path, out: &mut dyn Write) {
    let file = &view["file"];
    let path = file["path"].as_str().unwrap_or("");
    let state = if !file["error"].is_null() {
        format!(
            "INVALID, the last good settings stay: {}",
            file["error"].as_str().unwrap_or("")
        )
    } else if file["exists"].as_bool() == Some(true) {
        "loaded".to_owned()
    } else {
        "none (defaults)".to_owned()
    };
    let _ = writeln!(out, "config: {path} ({state})");
    if view["running"].as_bool() != Some(true) {
        let _ = writeln!(
            out,
            "{}; showing the file over the defaults (a start's flags are not known)",
            crate::endpoint::no_hub_message(dir)
        );
    }
    let rows: Vec<(String, String, String, String)> = view["values"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let value = match &e["value"] {
                Value::String(s) => format!("{s:?}"),
                Value::Null => "(unset)".to_owned(),
                other => other.to_string(),
            };
            (
                e["key"].as_str().unwrap_or("").to_owned(),
                value,
                e["source"].as_str().unwrap_or("").to_owned(),
                e["apply"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    let key_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let val_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).min(48);
    for (key, value, source, apply) in rows {
        let when = if apply == "restart" {
            "  (at restart)"
        } else {
            ""
        };
        let line = format!("  {key:key_w$}  {value:val_w$}  {source:7}{when}");
        let _ = writeln!(out, "{}", line.trim_end());
    }
    let pending: Vec<&str> = view["pending_restart"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !pending.is_empty() {
        let _ = writeln!(
            out,
            "waiting for a restart (the file differs from what runs): {}",
            pending.join(", ")
        );
    }
    let _ = writeln!(
        out,
        "also: notify.toml (push), agent-detection/ (screen rules), plugins/ (fw hub plugin ls)"
    );
}
