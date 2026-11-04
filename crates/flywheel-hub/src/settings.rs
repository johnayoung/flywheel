//! The web UI's preferences, kept by the hub so they follow you across
//! devices: appearance, terminal, keyboard (every shortcut, rebindable),
//! notifications, agent launch defaults. `<state-dir>/settings.json`
//! (0600, atomic writes, revisioned like layouts); a browser may override
//! any of them for itself (kept in its local storage; Settings marks
//! those "this device").
//!
//! The server owns the schema: [`PREFS`] (each preference's type, range,
//! and default) and [`keys::ACTIONS`] (every shortcut and its default)
//! are served to the client, and every write is validated here (`PUT
//! /api/settings`), including the keymap's conflict rules.

pub mod keys;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// The settings file in the state dir.
pub const SETTINGS_FILE: &str = "settings.json";
/// Its schema version.
pub const SETTINGS_VERSION: u32 = 1;
/// Largest settings file read.
const MAX_SETTINGS_BYTES: u64 = 256 * 1024;

/// A preference's type.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PrefKind {
    Choice {
        choices: &'static [&'static str],
    },
    Int {
        min: i64,
        max: i64,
    },
    Bool,
    /// An agent model id (`claude --model` / `codex -m`).
    Model,
}

/// One preference.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PrefSpec {
    pub id: &'static str,
    pub section: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    #[serde(flatten)]
    pub kind: PrefKind,
    /// `null` for an unset model / font size (the client's own choice).
    #[serde(serialize_with = "default_value")]
    pub default: PrefDefault,
    /// Kept per device only (a browser permission, a screen size).
    pub device_only: bool,
}

/// A preference's default (what an unset one reads as).
#[derive(Debug, Clone, Copy)]
pub enum PrefDefault {
    Str(&'static str),
    Int(i64),
    Bool(bool),
    None,
}

fn default_value<S: serde::Serializer>(value: &PrefDefault, s: S) -> Result<S::Ok, S::Error> {
    to_json(*value).serialize(s)
}

fn to_json(value: PrefDefault) -> Value {
    match value {
        PrefDefault::Str(v) => json!(v),
        PrefDefault::Int(v) => json!(v),
        PrefDefault::Bool(v) => json!(v),
        PrefDefault::None => Value::Null,
    }
}

/// Claude's permission modes plus "" (the CLI's own default).
const CLAUDE_MODES: &[&str] = &[
    "",
    "manual",
    "acceptEdits",
    "plan",
    "auto",
    "dontAsk",
    "bypassPermissions",
];
const APPROVALS: &[&str] = &["", "untrusted", "on-failure", "on-request", "never"];
const SANDBOXES: &[&str] = &["", "read-only", "workspace-write", "danger-full-access"];

/// Every preference, in the order Settings shows them.
pub const PREFS: &[PrefSpec] = &[
    PrefSpec {
        id: "appearance.theme",
        section: "appearance",
        label: "Theme",
        help: "auto follows the OS",
        kind: PrefKind::Choice {
            choices: &["auto", "dark", "light"],
        },
        default: PrefDefault::Str("auto"),
        device_only: false,
    },
    PrefSpec {
        id: "appearance.term_font",
        section: "appearance",
        label: "Terminal font",
        help: "JetBrains Mono and Martian Mono are built in",
        kind: PrefKind::Choice {
            choices: &["jetbrains", "martian", "system"],
        },
        default: PrefDefault::Str("jetbrains"),
        device_only: false,
    },
    PrefSpec {
        id: "appearance.font_size",
        section: "appearance",
        label: "Terminal text size",
        help: "pixels; unset: 13, 11 on phones",
        kind: PrefKind::Int { min: 9, max: 28 },
        default: PrefDefault::None,
        device_only: false,
    },
    PrefSpec {
        id: "appearance.density",
        section: "appearance",
        label: "Density",
        help: "compact tightens the sidebar and title bars",
        kind: PrefKind::Choice {
            choices: &["comfortable", "compact"],
        },
        default: PrefDefault::Str("comfortable"),
        device_only: false,
    },
    PrefSpec {
        id: "terminal.scrollback",
        section: "terminal",
        label: "Scrollback in view",
        help: "lines each pane keeps to scroll back through (the hub keeps its own, see hub.toml)",
        kind: PrefKind::Int {
            min: 1000,
            max: 100_000,
        },
        default: PrefDefault::Int(10_000),
        device_only: false,
    },
    PrefSpec {
        id: "terminal.cursor_style",
        section: "terminal",
        label: "Cursor",
        help: "programs may set their own",
        kind: PrefKind::Choice {
            choices: &["block", "bar", "underline"],
        },
        default: PrefDefault::Str("block"),
        device_only: false,
    },
    PrefSpec {
        id: "terminal.cursor_blink",
        section: "terminal",
        label: "Blinking cursor",
        help: "",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
        device_only: false,
    },
    PrefSpec {
        id: "terminal.transcript_ligatures",
        section: "terminal",
        label: "Ligatures in terminals, transcripts and commands",
        help: "off keeps -> != === and ``` exactly as typed",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(false),
        device_only: false,
    },
    PrefSpec {
        id: "terminal.lite",
        section: "terminal",
        label: "Lite mode",
        help: "for slow links: paced screen updates, hidden panes paused, side fetches wait; auto: when the link measures slow",
        kind: PrefKind::Choice {
            choices: &["auto", "on", "off"],
        },
        default: PrefDefault::Str("auto"),
        device_only: true,
    },
    PrefSpec {
        id: "keyboard.pane_prefix",
        section: "keyboard",
        label: "Pane prefix (tmux-style)",
        help: "the prefix key, then a key below",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(false),
        device_only: false,
    },
    PrefSpec {
        id: "keyboard.key_bar",
        section: "keyboard",
        label: "Key bar",
        help: "esc, tab, ctrl, arrows above the keyboard; auto: touch screens",
        kind: PrefKind::Choice {
            choices: &["auto", "on", "off"],
        },
        default: PrefDefault::Str("auto"),
        device_only: false,
    },
    PrefSpec {
        id: "keyboard.companion",
        section: "keyboard",
        label: "Companion shell key",
        help: "split: beside the agent on wide screens; swap: in place",
        kind: PrefKind::Choice {
            choices: &["split", "swap"],
        },
        default: PrefDefault::Str("split"),
        device_only: false,
    },
    PrefSpec {
        id: "keyboard.tour",
        section: "keyboard",
        label: "Shortcut tour",
        help: "offer the 30-second tour of the key shortcuts on the start screen (finishing or \
               dismissing it turns this off)",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
        device_only: false,
    },
    PrefSpec {
        id: "notifications.alerts",
        section: "notifications",
        label: "System notifications",
        help: "when an agent is blocked or done while this page is hidden (asks the browser)",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(false),
        device_only: true,
    },
    PrefSpec {
        id: "notifications.toasts",
        section: "notifications",
        label: "Command toasts",
        help: "a toast when an agent in view suggests a shell command",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
        device_only: false,
    },
    PrefSpec {
        id: "agents.claude.model",
        section: "agents",
        label: "Claude model",
        help: "pre-filled in the new-session form",
        kind: PrefKind::Model,
        default: PrefDefault::None,
        device_only: false,
    },
    PrefSpec {
        id: "agents.claude.permission_mode",
        section: "agents",
        label: "Claude permission mode",
        help: "",
        kind: PrefKind::Choice {
            choices: CLAUDE_MODES,
        },
        default: PrefDefault::Str(""),
        device_only: false,
    },
    PrefSpec {
        id: "agents.codex.model",
        section: "agents",
        label: "Codex model",
        help: "pre-filled in the new-session form",
        kind: PrefKind::Model,
        default: PrefDefault::None,
        device_only: false,
    },
    PrefSpec {
        id: "agents.codex.approval",
        section: "agents",
        label: "Codex approvals",
        help: "",
        kind: PrefKind::Choice { choices: APPROVALS },
        default: PrefDefault::Str(""),
        device_only: false,
    },
    PrefSpec {
        id: "agents.codex.sandbox",
        section: "agents",
        label: "Codex sandbox",
        help: "",
        kind: PrefKind::Choice { choices: SANDBOXES },
        default: PrefDefault::Str(""),
        device_only: false,
    },
];

pub fn pref(id: &str) -> Option<&'static PrefSpec> {
    PREFS.iter().find(|p| p.id == id)
}

/// Check one value for `spec`: the value stored, or why not.
pub fn validate_pref(spec: &PrefSpec, value: &Value) -> Result<Value, String> {
    if spec.device_only {
        return Err(format!(
            "{} is kept per device (this browser's local storage)",
            spec.id
        ));
    }
    match spec.kind {
        PrefKind::Choice { choices } => match value.as_str() {
            Some(v) if choices.contains(&v) => Ok(json!(v)),
            _ => Err(format!(
                "{} must be one of {}",
                spec.id,
                choices
                    .iter()
                    .map(|c| if c.is_empty() { "\"\"" } else { c })
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        },
        PrefKind::Int { min, max } => match value.as_i64() {
            Some(n) if (min..=max).contains(&n) => Ok(json!(n)),
            _ => Err(format!("{} must be a whole number {min}..={max}", spec.id)),
        },
        PrefKind::Bool => value
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| format!("{} must be true or false", spec.id)),
        PrefKind::Model => match value.as_str().map(str::trim) {
            Some("") => Ok(json!("")),
            Some(model) => crate::agent::validate_model(model)
                .map(|()| json!(model))
                .map_err(|e| format!("{}: {e}", spec.id)),
            None => Err(format!("{} must be a model id (text)", spec.id)),
        },
    }
}

/// The stored settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub version: u32,
    /// Bumped on every save (0: never saved).
    pub rev: u64,
    /// Preference id -> value (absent: the default).
    #[serde(default)]
    pub values: BTreeMap<String, Value>,
    /// Action id -> bindings, for actions not on their defaults.
    #[serde(default)]
    pub keys: BTreeMap<String, Vec<String>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            rev: 0,
            values: BTreeMap::new(),
            keys: BTreeMap::new(),
        }
    }
}

/// A change: preference values and action bindings to set (`null`
/// resets one to its default), written against `rev`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub rev: u64,
    #[serde(default)]
    pub values: BTreeMap<String, Value>,
    #[serde(default)]
    pub keys: BTreeMap<String, Option<Vec<String>>>,
}

/// Why a save did not happen.
#[derive(Debug, Clone, PartialEq)]
pub enum SaveError {
    /// Written against an older revision; the current settings.
    Stale(Box<Settings>),
    /// What is wrong, per field (and every keymap conflict).
    Invalid(Vec<keys::Finding>),
    Io(String),
}

/// What a save did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Saved {
    pub settings: Settings,
    /// Bindings that work but cost the terminal something.
    pub warnings: Vec<keys::Finding>,
}

/// `settings.json`, loaded once and kept in memory.
#[derive(Debug)]
pub struct SettingsStore {
    path: PathBuf,
    current: Mutex<Settings>,
    /// Why the file on disk was not loaded (it is left for the operator).
    load_error: Option<String>,
}

impl SettingsStore {
    /// Load `<state-dir>/settings.json` (missing: defaults). A file that
    /// cannot be read is left untouched and reported; saves then go to it
    /// only after the operator fixes or removes it.
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(SETTINGS_FILE);
        let (current, load_error) = match read(&path) {
            Ok(settings) => (settings, None),
            Err(e) => {
                eprintln!("fw hub: {e}; using default settings");
                (Settings::default(), Some(e))
            }
        };
        Self {
            path,
            current: Mutex::new(current),
            load_error,
        }
    }

    pub fn get(&self) -> Settings {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Apply `patch` if it is valid and current; persist it.
    pub fn save(&self, patch: Patch) -> Result<Saved, SaveError> {
        if let Some(e) = &self.load_error {
            return Err(SaveError::Io(format!(
                "not saving over {}: {e} (fix or remove it, then restart the hub)",
                self.path.display()
            )));
        }
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if patch.rev != current.rev {
            return Err(SaveError::Stale(Box::new(current.clone())));
        }
        let mut next = current.clone();
        let mut errors = Vec::new();
        for (id, value) in &patch.values {
            let Some(spec) = pref(id) else {
                errors.push(keys::Finding {
                    action: id.clone(),
                    key: String::new(),
                    message: format!("{id:?} is not a setting"),
                });
                continue;
            };
            if value.is_null() {
                next.values.remove(id);
                continue;
            }
            match validate_pref(spec, value) {
                Ok(value) => {
                    next.values.insert(id.clone(), value);
                }
                Err(message) => errors.push(keys::Finding {
                    action: id.clone(),
                    key: String::new(),
                    message,
                }),
            }
        }
        for (id, bindings) in &patch.keys {
            match bindings {
                None => {
                    next.keys.remove(id);
                }
                Some(bindings) => {
                    next.keys.insert(id.clone(), bindings.clone());
                }
            }
        }
        let check = keys::check(&next.keys);
        errors.extend(check.errors);
        if !errors.is_empty() {
            return Err(SaveError::Invalid(errors));
        }
        // Stored canonical, and only where it differs from the default.
        let defaults = keys::defaults();
        next.keys = next
            .keys
            .keys()
            .filter_map(|id| {
                let keys = check.keys.get(id)?;
                (defaults.get(id) != Some(keys)).then(|| (id.clone(), keys.clone()))
            })
            .collect();
        next.rev = current.rev + 1;
        next.version = SETTINGS_VERSION;
        let bytes = serde_json::to_vec_pretty(&next).map_err(|e| SaveError::Io(e.to_string()))?;
        crate::auth::write_private(&self.path, &bytes)
            .map_err(|e| SaveError::Io(format!("cannot write {}: {e}", self.path.display())))?;
        *current = next.clone();
        Ok(Saved {
            settings: next,
            warnings: check.warnings,
        })
    }

    /// The `GET /api/settings` answer: the stored settings, the schema
    /// (preferences, shortcuts with their defaults), the keymap in force,
    /// and the key rules the client checks a capture against.
    pub fn view(&self) -> Value {
        let settings = self.get();
        let check = keys::check(&settings.keys);
        json!({
            "rev": settings.rev,
            "values": settings.values,
            "keys": settings.keys,
            "keymap": check.keys,
            "prefs": PREFS,
            "actions": keys::ACTIONS,
            "rules": {
                "reserved": keys::RESERVED.iter().map(|(k, why)| json!({"key": k, "why": why})).collect::<Vec<_>>(),
                "terminal": keys::TERMINAL.iter().map(|(k, why)| json!({"key": k, "why": why})).collect::<Vec<_>>(),
                "shadowed": keys::SHADOWED.iter().map(|(k, why)| json!({"key": k, "why": why})).collect::<Vec<_>>(),
                "desktop": keys::DESKTOP.iter().map(|(k, why)| json!({"key": k, "why": why})).collect::<Vec<_>>(),
            },
            "load_error": self.load_error,
            "file": self.path.to_string_lossy(),
        })
    }
}

pub(crate) fn read(path: &Path) -> Result<Settings, String> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    if meta.len() > MAX_SETTINGS_BYTES {
        return Err(format!("{} is larger than 256 KiB", path.display()));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let settings: Settings = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not valid settings: {e}", path.display()))?;
    if settings.version > SETTINGS_VERSION {
        return Err(format!(
            "{} is version {} (this hub reads up to {SETTINGS_VERSION})",
            path.display(),
            settings.version
        ));
    }
    Ok(settings)
}

// The choice lists mirror the launcher's own (kept in step by a test).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{CLAUDE_PERMISSION_MODES, CODEX_APPROVALS, CODEX_SANDBOXES};

    #[test]
    fn agent_choices_match_the_launcher() {
        assert_eq!(&CLAUDE_MODES[1..], CLAUDE_PERMISSION_MODES);
        assert_eq!(&APPROVALS[1..], CODEX_APPROVALS);
        assert_eq!(&SANDBOXES[1..], CODEX_SANDBOXES);
    }

    #[test]
    fn every_default_is_valid_for_its_kind() {
        for spec in PREFS {
            let default = to_json(spec.default);
            if default.is_null() || spec.device_only {
                continue;
            }
            assert!(validate_pref(spec, &default).is_ok(), "{}", spec.id);
        }
        let mut ids: Vec<_> = PREFS.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), PREFS.len());
    }

    #[test]
    fn values_are_type_checked() {
        let theme = pref("appearance.theme").unwrap();
        assert!(validate_pref(theme, &json!("dark")).is_ok());
        assert!(validate_pref(theme, &json!("neon")).is_err());
        let size = pref("appearance.font_size").unwrap();
        assert!(validate_pref(size, &json!(14)).is_ok());
        assert!(validate_pref(size, &json!(40)).is_err());
        assert!(validate_pref(size, &json!("14")).is_err());
        let model = pref("agents.claude.model").unwrap();
        assert!(validate_pref(model, &json!("claude-opus-4-1")).is_ok());
        assert!(validate_pref(model, &json!("--dangerous")).is_err());
        let alerts = pref("notifications.alerts").unwrap();
        assert!(validate_pref(alerts, &json!(true)).is_err(), "device only");
    }
}
