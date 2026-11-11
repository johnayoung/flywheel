//! `fw hub capture install | uninstall | status`: the user-level Claude
//! Code hooks that report runs the hub did not start (`claude -p`, Agent
//! SDK runs, a session in a plain terminal) to the hub on this machine.
//!
//! The hooks go in Claude Code's user settings
//! (`$CLAUDE_CONFIG_DIR/settings.json`, default `~/.claude/settings.json`),
//! beside whatever else is there: install replaces only the hub's own
//! capture hooks (those POSTing to [`CAPTURE_PATH`]), uninstall removes
//! only them, and the file is backed up before either writes it.

use crate::agent::{BATCH_ENV, BUDGET_ENV, CAPTURE_PATH};
use crate::cli::{StateDirArg, resolve_state_dir};
use crate::endpoint::Endpoint;
use serde_json::{Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Beside the settings file: its content before the last write.
const BACKUP_SUFFIX: &str = ".fw-hub-backup";

#[derive(Debug, clap::Args)]
pub(crate) struct CaptureArgs {
    #[command(subcommand)]
    command: CaptureCommand,
}

#[derive(Debug, clap::Subcommand)]
enum CaptureCommand {
    /// Add the capture hooks to Claude Code's user settings: every Claude
    /// Code run on this machine then reports to this hub (replaces an
    /// earlier install; the file is backed up first).
    Install(CaptureTarget),
    /// Remove the capture hooks from Claude Code's user settings (nothing
    /// else in the file changes).
    Uninstall(CaptureTarget),
    /// Whether the capture hooks are installed, and where they report.
    Status(CaptureTarget),
}

#[derive(Debug, clap::Args)]
struct CaptureTarget {
    /// Claude Code's settings file [default: $CLAUDE_CONFIG_DIR/settings.json,
    /// i.e. ~/.claude/settings.json].
    #[arg(long, value_name = "FILE")]
    settings: Option<PathBuf>,
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn main(args: &CaptureArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = match &args.command {
        CaptureCommand::Install(target) => install(target, out),
        CaptureCommand::Uninstall(target) => uninstall(target, out),
        CaptureCommand::Status(target) => status(target, out),
    };
    match result {
        Ok(code) => code,
        Err(message) => {
            let _ = writeln!(err, "fw hub capture: {message}");
            1
        }
    }
}

/// Claude Code's user settings file.
fn settings_path(target: &CaptureTarget) -> Result<PathBuf, String> {
    if let Some(path) = &target.settings {
        return Ok(path.clone());
    }
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("settings.json"));
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|home| PathBuf::from(home).join(".claude").join("settings.json"))
        .ok_or_else(|| "cannot find Claude Code's settings: set HOME or pass --settings".into())
}

/// The settings file as a JSON object (`{}` when there is none). One that
/// does not read as an object is refused, never overwritten.
fn read_settings(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Some(Map::new())),
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => Ok(Some(map)),
            Ok(_) => Err(format!(
                "{} is not a JSON object: left as is",
                path.display()
            )),
            Err(e) => Err(format!(
                "{} does not read as JSON ({e}): left as is",
                path.display()
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Whether a hook handler is one of the hub's capture hooks.
fn is_capture(handler: &Value) -> bool {
    handler
        .get("url")
        .and_then(Value::as_str)
        .is_some_and(|url| url.ends_with(CAPTURE_PATH))
}

/// Remove the capture hooks from `settings`; how many handlers went.
/// Groups and events left empty go too (and `hooks` itself, when nothing
/// else was in it).
fn strip(settings: &mut Map<String, Value>) -> usize {
    let Some(Value::Object(hooks)) = settings.get_mut("hooks") else {
        return 0;
    };
    let mut removed = 0;
    hooks.retain(|_, groups| {
        let Value::Array(list) = groups else {
            return true;
        };
        list.retain_mut(|group| {
            let Some(Value::Array(handlers)) = group.get_mut("hooks") else {
                return true;
            };
            let before = handlers.len();
            handlers.retain(|h| !is_capture(h));
            removed += before - handlers.len();
            !(handlers.is_empty() && before > 0)
        });
        !list.is_empty()
    });
    if hooks.is_empty() {
        settings.remove("hooks");
    }
    removed
}

/// Add `ours` (`{"hooks": {EVENT: [group]}}`) to `settings`' hooks.
fn merge(settings: &mut Map<String, Value>, ours: &Value) -> Result<(), String> {
    let theirs = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(theirs) = theirs else {
        return Err("the settings' \"hooks\" is not an object: left as is".into());
    };
    let Some(Value::Object(ours)) = ours.get("hooks") else {
        return Err("the hub sent no hooks".into());
    };
    for (event, groups) in ours {
        let list = theirs
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(list) = list else {
            return Err(format!(
                "the settings' hooks.{event} is not a list: left as is"
            ));
        };
        if let Value::Array(groups) = groups {
            list.extend(groups.iter().cloned());
        }
    }
    Ok(())
}

/// Write `settings` to `path` (pretty, a newline at the end) through a
/// temporary file, after copying what was there to the backup.
fn write_settings(path: &Path, settings: &Map<String, Value>) -> Result<(), String> {
    if path.exists() {
        let backup = PathBuf::from(format!("{}{BACKUP_SUFFIX}", path.display()));
        std::fs::copy(path, &backup).map_err(|e| {
            format!(
                "cannot back up {} to {}: {e}",
                path.display(),
                backup.display()
            )
        })?;
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(settings.clone()))
        .map_err(|e| format!("cannot write the settings: {e}"))?;
    text.push('\n');
    let temp = PathBuf::from(format!("{}.fw-hub-tmp", path.display()));
    std::fs::write(&temp, text).map_err(|e| format!("cannot write {}: {e}", temp.display()))?;
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&temp, meta.permissions());
    }
    std::fs::rename(&temp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

fn install(target: &CaptureTarget, out: &mut dyn Write) -> Result<i32, String> {
    let path = settings_path(target)?;
    let state_dir = resolve_state_dir(&target.state)?;
    let hub = Endpoint::local(&state_dir)?;
    let ours = hub.blocking_call("GET", "/api/telemetry/capture/hooks", None)?;
    let mut settings = read_settings(&path)?.unwrap_or_default();
    let replaced = strip(&mut settings);
    merge(&mut settings, &ours)?;
    write_settings(&path, &settings)?;
    let url = first_url(&settings).unwrap_or_default();
    let _ = writeln!(
        out,
        "{} capture hooks in {}: every Claude Code run that reads it now reports to {url}.",
        if replaced > 0 {
            "Replaced the"
        } else {
            "Installed"
        },
        path.display()
    );
    let _ = writeln!(
        out,
        "A run started inside a hub session is counted under it. A launcher names its batch \
         with {BATCH_ENV}=LABEL and what it may spend with {BUDGET_ENV}=USD."
    );
    let _ = writeln!(out, "Undo: fw hub capture uninstall");
    Ok(0)
}

fn uninstall(target: &CaptureTarget, out: &mut dyn Write) -> Result<i32, String> {
    let path = settings_path(target)?;
    let Some(mut settings) = read_settings(&path)? else {
        let _ = writeln!(out, "No capture hooks: {} does not exist.", path.display());
        return Ok(0);
    };
    let removed = strip(&mut settings);
    if removed == 0 {
        let _ = writeln!(out, "No capture hooks in {}.", path.display());
        return Ok(0);
    }
    write_settings(&path, &settings)?;
    let _ = writeln!(out, "Removed the capture hooks from {}.", path.display());
    Ok(0)
}

fn status(target: &CaptureTarget, out: &mut dyn Write) -> Result<i32, String> {
    let path = settings_path(target)?;
    let settings = read_settings(&path)?.unwrap_or_default();
    match first_url(&settings) {
        Some(url) => {
            let _ = writeln!(
                out,
                "Installed in {}: runs report to {url}.",
                path.display()
            );
            Ok(0)
        }
        None => {
            let _ = writeln!(
                out,
                "Not installed in {}: `fw hub capture install` adds them.",
                path.display()
            );
            Ok(1)
        }
    }
}

/// Where the installed capture hooks report.
fn first_url(settings: &Map<String, Value>) -> Option<String> {
    settings
        .get("hooks")?
        .as_object()?
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .find(|h| is_capture(h))
        .and_then(|h| h.get("url").and_then(Value::as_str))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn install_keeps_the_users_hooks_and_replaces_only_the_hubs() {
        let ours = crate::agent::capture_settings("http://127.0.0.1:7420", "t");
        let mut settings = json!({
            "model": "opus",
            "hooks": {
                "Stop": [{ "hooks": [{ "type": "command", "command": "notify-send done" }] }]
            }
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(strip(&mut settings), 0);
        merge(&mut settings, &ours).unwrap();
        // Installed twice: the earlier capture hooks are replaced.
        let again = strip(&mut settings);
        assert!(again > 0);
        merge(&mut settings, &ours).unwrap();
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "{stop:?}");
        assert_eq!(stop[0]["hooks"][0]["command"], "notify-send done");
        assert!(is_capture(&stop[1]["hooks"][0]));
        assert!(settings["hooks"].get("SessionStart").is_none());
        assert_eq!(
            first_url(&settings).as_deref(),
            Some("http://127.0.0.1:7420/api/telemetry/capture")
        );
        // Uninstalled: the user's own hook and settings are all that is left.
        assert_eq!(strip(&mut settings), again);
        assert_eq!(
            Value::Object(settings),
            json!({
                "model": "opus",
                "hooks": {
                    "Stop": [{ "hooks": [{ "type": "command", "command": "notify-send done" }] }]
                }
            })
        );
    }

    #[test]
    fn uninstall_drops_an_emptied_hooks_object() {
        let ours = crate::agent::capture_settings("http://127.0.0.1:7420", "t");
        let mut settings = Map::new();
        merge(&mut settings, &ours).unwrap();
        strip(&mut settings);
        assert!(settings.is_empty(), "{settings:?}");
    }

    #[test]
    fn a_settings_file_that_is_not_an_object_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "[1, 2]").unwrap();
        assert!(read_settings(&path).is_err());
        std::fs::write(&path, "{ not json").unwrap();
        assert!(read_settings(&path).is_err());
        assert_eq!(read_settings(&dir.path().join("none.json")).unwrap(), None);
    }

    #[test]
    fn writing_backs_up_the_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{\"a\": 1}\n").unwrap();
        let mut settings = Map::new();
        settings.insert("b".into(), json!(2));
        write_settings(&path, &settings).unwrap();
        let backup = dir.path().join(format!("settings.json{BACKUP_SUFFIX}"));
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "{\"a\": 1}\n");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"b\": 2\n}\n"
        );
    }
}
