//! Keyboard shortcuts: the catalog of rebindable actions, the binding
//! syntax, and the checks a binding must pass.
//!
//! A binding is `mod+...+key`, lowercase: modifiers in the order `mod`
//! (Ctrl, or Cmd on macOS), `ctrl`, `alt` (the hub chord: Alt, or
//! Ctrl+Option on macOS), `shift`, `meta`; then a key name from the
//! physical key (`a`-`z`, `0`-`9`, `f1`-`f24`, `left` `right` `up` `down`,
//! `pageup` `pagedown` `home` `end` `insert` `delete` `backspace` `tab`
//! `enter` `escape` `space`, and `` ` - = [ ] \ ; ' , . / ``). An indexed
//! action (jump to the nth session) takes the range `1..9` as its key.
//! Pane-prefix actions (after the prefix key) bind the character typed
//! instead: any single character (`%`, `"`, `z`), or a named key, with an
//! optional `ctrl`.
//!
//! The web client matches events against the canonical strings these
//! checks return; the server is the one place that decides what a valid,
//! conflict-free keymap is (`PUT /api/settings`, `POST
//! /api/settings/keys/check`).

use serde::Serialize;
use std::collections::BTreeMap;

/// Most bindings one action keeps.
pub const MAX_BINDINGS: usize = 4;
/// Longest binding string accepted.
pub const MAX_BINDING_CHARS: usize = 40;

/// Where an action's keys are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Anywhere in the app (captured before the terminal sees them).
    Global,
    /// The key after the pane prefix (palette "Pane prefix").
    Prefix,
}

/// A rebindable action.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ActionSpec {
    pub id: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub scope: Scope,
    /// Takes the `1..9` range (the nth session / tab).
    pub indexed: bool,
    pub defaults: &'static [&'static str],
}

const fn global(
    id: &'static str,
    group: &'static str,
    label: &'static str,
    defaults: &'static [&'static str],
) -> ActionSpec {
    ActionSpec {
        id,
        group,
        label,
        scope: Scope::Global,
        indexed: false,
        defaults,
    }
}

const fn prefix(
    id: &'static str,
    label: &'static str,
    defaults: &'static [&'static str],
) -> ActionSpec {
    ActionSpec {
        id,
        group: "Pane prefix",
        label,
        scope: Scope::Prefix,
        indexed: false,
        defaults,
    }
}

/// Every rebindable action, in the order Settings lists them.
pub const ACTIONS: &[ActionSpec] = &[
    global(
        "palette.open",
        "General",
        "Command palette (pressed again inside it: the key goes to the terminal)",
        &["mod+k"],
    ),
    global(
        "search.open",
        "General",
        "Search every session",
        &["mod+shift+f"],
    ),
    global("settings.open", "General", "Settings", &["alt+,"]),
    global("keys.show", "General", "Keyboard shortcuts", &[]),
    global("session.new", "Sessions", "New session", &["alt+n"]),
    global(
        "workspace.new",
        "Sessions",
        "New workspace",
        &["alt+shift+n"],
    ),
    ActionSpec {
        id: "session.jump",
        group: "Sessions",
        label: "Jump to the nth session",
        scope: Scope::Global,
        indexed: true,
        defaults: &["alt+1..9"],
    },
    global("session.prev", "Sessions", "Previous session", &["alt+up"]),
    global("session.next", "Sessions", "Next session", &["alt+down"]),
    global(
        "session.rename",
        "Sessions",
        "Rename the attached session",
        &["alt+r"],
    ),
    global(
        "session.close",
        "Sessions",
        "Close the attached session",
        &["alt+w"],
    ),
    global(
        "share.open",
        "Sessions",
        "Share the attached session (read-only link)",
        &[],
    ),
    global(
        "record.toggle",
        "Sessions",
        "Start or stop recording the attached session",
        &[],
    ),
    global(
        "commands.toggle",
        "Agents",
        "Suggested commands drawer",
        &["alt+c"],
    ),
    global(
        "commands.copy_last",
        "Agents",
        "Copy the last suggested shell command",
        &["alt+shift+c"],
    ),
    global(
        "companion.flip",
        "Agents",
        "Flip between an agent and its companion shell",
        &["alt+`"],
    ),
    global("flywheel.toggle", "Agents", "Flywheel panel", &["alt+f"]),
    global(
        "review.toggle",
        "Agents",
        "Review the attached session's git work",
        &["alt+g"],
    ),
    global(
        "handoff.send",
        "Agents",
        "Send the terminal selection to another agent",
        &[],
    ),
    global(
        "review.ask",
        "Agents",
        "Ask an agent of the other kind to review the attached session",
        &[],
    ),
    global("fanout.open", "Agents", "Verified fan-outs", &[]),
    global("fanout.new", "Agents", "New verified fan-out", &[]),
    global(
        "pane.split_right",
        "Panes",
        "Split right with a new shell",
        &["alt+\\"],
    ),
    global(
        "pane.split_down",
        "Panes",
        "Split down with a new shell",
        &["alt+-"],
    ),
    global(
        "pane.focus_left",
        "Panes",
        "Focus the pane to the left",
        &["alt+shift+left"],
    ),
    global(
        "pane.focus_down",
        "Panes",
        "Focus the pane below",
        &["alt+shift+down"],
    ),
    global(
        "pane.focus_up",
        "Panes",
        "Focus the pane above",
        &["alt+shift+up"],
    ),
    global(
        "pane.focus_right",
        "Panes",
        "Focus the pane to the right",
        &["alt+shift+right"],
    ),
    global(
        "pane.resize_left",
        "Panes",
        "Move the nearest divider left",
        &["alt+shift+h"],
    ),
    global(
        "pane.resize_down",
        "Panes",
        "Move the nearest divider down",
        &["alt+shift+j"],
    ),
    global(
        "pane.resize_up",
        "Panes",
        "Move the nearest divider up",
        &["alt+shift+k"],
    ),
    global(
        "pane.resize_right",
        "Panes",
        "Move the nearest divider right",
        &["alt+shift+l"],
    ),
    global("pane.zoom", "Panes", "Zoom the focused pane", &["alt+z"]),
    global("pane.close", "Panes", "Close the focused pane", &["alt+x"]),
    global("tab.new", "Panes", "New tab", &["alt+t"]),
    global("tab.prev", "Panes", "Previous tab", &["alt+pageup"]),
    global("tab.next", "Panes", "Next tab", &["alt+pagedown"]),
    global(
        "prefix.arm",
        "Pane prefix",
        "The pane prefix key (with the prefix on)",
        &["ctrl+b"],
    ),
    prefix("prefix.split_right", "Split right", &["%"]),
    prefix("prefix.split_down", "Split down", &["\""]),
    prefix("prefix.focus_left", "Focus left", &["left"]),
    prefix("prefix.focus_down", "Focus down", &["down"]),
    prefix("prefix.focus_up", "Focus up", &["up"]),
    prefix("prefix.focus_right", "Focus right", &["right"]),
    prefix(
        "prefix.resize_left",
        "Move the divider left",
        &["ctrl+left"],
    ),
    prefix(
        "prefix.resize_down",
        "Move the divider down",
        &["ctrl+down"],
    ),
    prefix("prefix.resize_up", "Move the divider up", &["ctrl+up"]),
    prefix(
        "prefix.resize_right",
        "Move the divider right",
        &["ctrl+right"],
    ),
    prefix("prefix.zoom", "Zoom", &["z"]),
    prefix("prefix.close_pane", "Close the pane", &["x"]),
    prefix("prefix.new_tab", "New tab", &["c"]),
    prefix("prefix.next_tab", "Next tab", &["n"]),
    prefix("prefix.prev_tab", "Previous tab", &["p"]),
    ActionSpec {
        id: "prefix.tab",
        group: "Pane prefix",
        label: "The nth tab",
        scope: Scope::Prefix,
        indexed: true,
        defaults: &["1..9"],
    },
    prefix("prefix.rename_tab", "Rename the tab", &[","]),
    prefix("prefix.close_tab", "Close the tab", &["&"]),
    prefix("prefix.next_pane", "Next pane", &["o"]),
    prefix("prefix.help", "Keyboard shortcuts", &["?"]),
];

pub fn action(id: &str) -> Option<&'static ActionSpec> {
    ACTIONS.iter().find(|a| a.id == id)
}

/// Keys a page can never have: the browser or the OS acts on them before
/// (or instead of) the page.
pub const RESERVED: &[(&str, &str)] = &[
    ("ctrl+w", "the browser closes the tab"),
    ("ctrl+t", "the browser opens a tab"),
    ("ctrl+n", "the browser opens a window"),
    ("ctrl+q", "the browser quits"),
    ("ctrl+shift+n", "the browser opens a private window"),
    ("ctrl+shift+t", "the browser reopens a closed tab"),
    ("ctrl+shift+w", "the browser closes the window"),
    ("ctrl+shift+q", "the browser quits"),
    ("ctrl+tab", "the browser switches tabs"),
    ("ctrl+shift+tab", "the browser switches tabs"),
    ("ctrl+pageup", "the browser switches tabs"),
    ("ctrl+pagedown", "the browser switches tabs"),
    ("ctrl+shift+i", "the browser opens its developer tools"),
    ("ctrl+shift+j", "the browser opens its developer tools"),
    ("ctrl+shift+c", "the browser opens its element inspector"),
    ("ctrl+shift+delete", "the browser clears browsing data"),
    ("ctrl+alt+delete", "the OS takes Ctrl Alt Delete"),
    ("ctrl+alt+backspace", "some X11 desktops kill the session"),
    ("alt+f4", "the OS closes the window"),
    ("alt+tab", "the OS switches windows"),
    ("alt+shift+tab", "the OS switches windows"),
    ("alt+space", "the OS opens the window menu"),
    ("f11", "the browser goes full screen"),
];

/// Keys that may never reach the page on some desktops or setups (it
/// depends on the machine, so they are warnings, not refusals).
pub const DESKTOP: &[(&str, &str)] = &[
    (
        "ctrl+alt+t",
        "Ubuntu and other Linux desktops open a terminal on it",
    ),
    ("ctrl+alt+l", "Linux desktops lock the screen on it"),
    ("ctrl+alt+d", "some Linux desktops show the desktop on it"),
    ("ctrl+alt+left", "Linux desktops switch workspaces on it"),
    ("ctrl+alt+right", "Linux desktops switch workspaces on it"),
    ("ctrl+alt+up", "Linux desktops switch workspaces on it"),
    ("ctrl+alt+down", "Linux desktops switch workspaces on it"),
    (
        "ctrl+space",
        "input-method switching takes it on many systems, and terminals lose ^@ (NUL: set-mark in emacs and shells)",
    ),
    (
        "ctrl+shift+space",
        "input-method switching takes it on some systems",
    ),
    (
        "ctrl+shift+v",
        "the browser pastes plain text on it, and terminals paste with it",
    ),
    ("ctrl+shift+b", "the browser toggles its bookmarks bar"),
];

/// Keys a terminal needs: binding one globally takes it from every
/// session.
pub const TERMINAL: &[(&str, &str)] = &[
    ("ctrl+c", "interrupt (SIGINT) in every terminal"),
    ("ctrl+d", "end of input in every terminal"),
    ("ctrl+z", "suspend (SIGTSTP) in every terminal"),
    ("ctrl+\\", "quit (SIGQUIT) in every terminal"),
    ("ctrl+[", "Escape in every terminal"),
    ("ctrl+m", "Enter in every terminal"),
    ("ctrl+i", "Tab in every terminal"),
    ("ctrl+h", "Backspace in every terminal"),
];

/// Keys that work as a hub shortcut but cost the terminal something.
pub const SHADOWED: &[(&str, &str)] = &[
    ("alt+b", "readline and zsh: back one word"),
    ("alt+f", "readline and zsh: forward one word"),
    ("alt+d", "readline and zsh: delete the next word"),
    (
        "alt+backspace",
        "readline and zsh: delete the previous word",
    ),
    ("alt+.", "readline and zsh: last argument"),
    ("alt+u", "readline: uppercase word"),
    ("alt+l", "readline: lowercase word"),
    ("alt+c", "readline: capitalize word"),
    ("alt+t", "readline: transpose words"),
    ("alt+y", "readline: yank-pop"),
    ("ctrl+left", "word left in shells and editors"),
    ("ctrl+right", "word right in shells and editors"),
    ("alt+left", "word left in some terminals' shells"),
    ("alt+right", "word right in some terminals' shells"),
];

const MODIFIERS: [&str; 5] = ["mod", "ctrl", "alt", "shift", "meta"];

const NAMED: &[&str] = &[
    "left",
    "right",
    "up",
    "down",
    "pageup",
    "pagedown",
    "home",
    "end",
    "insert",
    "delete",
    "backspace",
    "tab",
    "enter",
    "escape",
    "space",
];

const PUNCTUATION: &str = "`-=[]\\;',./";

fn is_function_key(key: &str) -> bool {
    key.strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=24).contains(&n))
}

/// A global binding's key name (the physical key).
fn global_key_ok(key: &str) -> bool {
    key == "1..9"
        || (key.len() == 1
            && key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || PUNCTUATION.contains(c)))
        || NAMED.contains(&key)
        || is_function_key(key)
}

/// One binding, canonical: modifiers in order, then the key.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Parsed {
    mods: Vec<&'static str>,
    key: String,
}

impl Parsed {
    fn canonical(&self) -> String {
        let mut parts: Vec<&str> = self.mods.clone();
        parts.push(&self.key);
        parts.join("+")
    }

    fn has(&self, modifier: &str) -> bool {
        self.mods.contains(&modifier)
    }

    /// The same chord with `mod` read as Ctrl (how the reserved lists
    /// are written).
    fn as_ctrl(&self) -> String {
        let mut mods: Vec<&str> = self
            .mods
            .iter()
            .map(|m| if *m == "mod" { "ctrl" } else { *m })
            .collect();
        mods.dedup();
        let mut parts = mods;
        parts.push(&self.key);
        parts.join("+")
    }
}

/// Parse `raw` for `scope`: the canonical form, or why it is not a key.
fn parse(raw: &str, scope: Scope) -> Result<Parsed, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty binding".into());
    }
    if raw.chars().count() > MAX_BINDING_CHARS {
        return Err(format!("longer than {MAX_BINDING_CHARS} characters"));
    }
    if scope == Scope::Prefix && raw.chars().count() == 1 {
        let c = raw.chars().next().unwrap_or(' ');
        if c.is_control() || c == ' ' {
            return Err("a prefix key must be a printable character or a named key".into());
        }
        return Ok(Parsed {
            mods: Vec::new(),
            key: raw.to_owned(),
        });
    }
    // `+` itself as the key: `ctrl++`.
    let (head, key) = match raw.strip_suffix("++") {
        Some(head) => (head, "+".to_owned()),
        None => match raw.rsplit_once('+') {
            Some((head, key)) => (head, key.to_ascii_lowercase()),
            None => ("", raw.to_ascii_lowercase()),
        },
    };
    let mut mods: Vec<&'static str> = Vec::new();
    if !head.is_empty() {
        for part in head.split('+') {
            let part = part.trim().to_ascii_lowercase();
            let part = match part.as_str() {
                "control" => "ctrl",
                "option" | "opt" => "alt",
                "cmd" | "super" | "win" => "meta",
                other => other,
            };
            let Some(known) = MODIFIERS.iter().find(|m| **m == part) else {
                return Err(format!(
                    "{part:?} is not a modifier (mod, ctrl, alt, shift, meta)"
                ));
            };
            if mods.contains(known) {
                return Err(format!("{known} twice"));
            }
            mods.push(known);
        }
    }
    mods.sort_by_key(|m| MODIFIERS.iter().position(|x| x == m));
    match scope {
        Scope::Global => {
            if !global_key_ok(&key) {
                return Err(format!(
                    "{key:?} is not a key name (a-z, 0-9, f1-f24, arrows, pageup, ..., or `-=[]\\;',./)"
                ));
            }
        }
        Scope::Prefix => {
            let single = key.chars().count() == 1;
            if !(single || NAMED.contains(&key.as_str()) || is_function_key(&key) || key == "1..9")
            {
                return Err(format!("{key:?} is not a character or a key name"));
            }
            if mods.iter().any(|m| *m != "ctrl") {
                return Err("after the prefix only ctrl combines with a key".into());
            }
        }
    }
    Ok(Parsed { mods, key })
}

/// A finding about one binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub action: String,
    /// The binding as given (canonical when it parsed).
    pub key: String,
    pub message: String,
}

/// What checking a keymap found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Check {
    /// Every action's bindings, canonical (unbound actions: `[]`).
    pub keys: BTreeMap<String, Vec<String>>,
    /// Refused: the keymap cannot be saved with these.
    pub errors: Vec<Finding>,
    /// Allowed, but worth knowing (a key the terminal loses).
    pub warnings: Vec<Finding>,
}

/// The default keymap.
pub fn defaults() -> BTreeMap<String, Vec<String>> {
    ACTIONS
        .iter()
        .map(|a| {
            (
                a.id.to_owned(),
                a.defaults.iter().map(|k| (*k).to_owned()).collect(),
            )
        })
        .collect()
}

/// Check `overrides` (action -> bindings; others keep their defaults) as
/// a whole keymap: every binding parses for its action's scope, no
/// reserved or terminal-essential key is taken, no key does two things in
/// one scope. Returns the keymap in force, canonical, with what is wrong.
pub fn check(overrides: &BTreeMap<String, Vec<String>>) -> Check {
    let mut out = Check {
        keys: defaults(),
        ..Check::default()
    };
    let finding = |action: &str, key: &str, message: String| Finding {
        action: action.to_owned(),
        key: key.to_owned(),
        message,
    };
    for (id, bindings) in overrides {
        let Some(spec) = action(id) else {
            out.errors
                .push(finding(id, "", format!("{id:?} is not an action")));
            continue;
        };
        if bindings.len() > MAX_BINDINGS {
            out.errors.push(finding(
                id,
                "",
                format!("at most {MAX_BINDINGS} keys per action"),
            ));
            continue;
        }
        let mut canonical = Vec::new();
        for raw in bindings {
            match parse(raw, spec.scope) {
                Ok(parsed) => {
                    let ranged = parsed.key == "1..9";
                    if ranged != spec.indexed {
                        out.errors.push(finding(
                            id,
                            raw,
                            if spec.indexed {
                                "takes the range 1..9 as its key (e.g. alt+1..9)".into()
                            } else {
                                "only the nth-session/tab actions take a range".into()
                            },
                        ));
                        continue;
                    }
                    let key = parsed.canonical();
                    if !canonical.contains(&key) {
                        canonical.push(key);
                    }
                }
                Err(message) => out.errors.push(finding(id, raw, message)),
            }
        }
        out.keys.insert(id.clone(), canonical);
    }
    // Per-key rules, for every binding in force (defaults included: an
    // override elsewhere can collide with one).
    let mut taken: BTreeMap<(&'static str, String), Vec<&'static str>> = BTreeMap::new();
    for spec in ACTIONS {
        let scope_name = match spec.scope {
            Scope::Global => "global",
            Scope::Prefix => "prefix",
        };
        let user = overrides.contains_key(spec.id);
        for key in out.keys.get(spec.id).cloned().unwrap_or_default() {
            let Ok(parsed) = parse(&key, spec.scope) else {
                continue;
            };
            // A range covers each of its digits.
            let expanded: Vec<String> = if parsed.key == "1..9" {
                (1..=9)
                    .map(|n| {
                        Parsed {
                            mods: parsed.mods.clone(),
                            key: n.to_string(),
                        }
                        .canonical()
                    })
                    .collect()
            } else {
                vec![key.clone()]
            };
            for single in &expanded {
                // `mod` is Ctrl (Cmd on macOS): `mod+k` and `ctrl+k` are
                // one key on most machines.
                let slot = match spec.scope {
                    Scope::Global => parse(single, Scope::Global)
                        .map(|p| p.as_ctrl())
                        .unwrap_or_else(|_| single.clone()),
                    Scope::Prefix => single.clone(),
                };
                taken.entry((scope_name, slot)).or_default().push(spec.id);
            }
            if !user || spec.scope == Scope::Prefix {
                continue;
            }
            let ctrl_form = parsed.as_ctrl();
            let samples: Vec<String> = if parsed.key == "1..9" {
                expanded
                    .iter()
                    .map(|k| {
                        parse(k, Scope::Global)
                            .map(|p| p.as_ctrl())
                            .unwrap_or_default()
                    })
                    .collect()
            } else {
                vec![ctrl_form.clone()]
            };
            if let Some((_, why)) = RESERVED
                .iter()
                .find(|(k, _)| samples.iter().any(|s| s == k))
            {
                out.errors
                    .push(finding(spec.id, &key, format!("reserved: {why}")));
                continue;
            }
            if parsed.has("meta") {
                out.errors.push(finding(
                    spec.id,
                    &key,
                    "reserved: the OS keeps Cmd/Super chords (use mod for Ctrl, or Cmd on macOS)"
                        .into(),
                ));
                continue;
            }
            let modified = parsed.has("mod") || parsed.has("ctrl") || parsed.has("alt");
            if !modified && !is_function_key(&parsed.key) {
                out.errors.push(finding(
                    spec.id,
                    &key,
                    "the terminal needs it: a shortcut without ctrl, alt, or mod would take typing"
                        .into(),
                ));
                continue;
            }
            if let Some((_, why)) = TERMINAL
                .iter()
                .find(|(k, _)| samples.iter().any(|s| s == k))
            {
                out.errors.push(finding(
                    spec.id,
                    &key,
                    format!("the terminal needs it: {why}"),
                ));
                continue;
            }
            if let Some((_, why)) = DESKTOP.iter().find(|(k, _)| samples.iter().any(|s| s == k)) {
                out.warnings
                    .push(finding(spec.id, &key, format!("may never arrive: {why}")));
            } else if let Some((_, why)) = SHADOWED
                .iter()
                .find(|(k, _)| samples.iter().any(|s| s == k))
            {
                out.warnings
                    .push(finding(spec.id, &key, format!("terminals lose it: {why}")));
            } else if (parsed.has("ctrl") || parsed.has("mod"))
                && !parsed.has("alt")
                && !parsed.has("shift")
                && parsed.key.len() == 1
            {
                out.warnings.push(finding(
                    spec.id,
                    &key,
                    format!(
                        "terminals lose ^{}: programs bind Ctrl keys",
                        parsed.key.to_ascii_uppercase()
                    ),
                ));
            }
            if !modified && parsed.key == "f12" {
                out.warnings.push(finding(
                    spec.id,
                    &key,
                    "browsers open their developer tools on F12".into(),
                ));
            }
        }
    }
    // After the prefix, the prefix key itself is taken: pressed twice it
    // goes to the terminal.
    let arm: Vec<String> = out
        .keys
        .get("prefix.arm")
        .into_iter()
        .flatten()
        .filter_map(|k| parse(k, Scope::Global).ok().map(|p| p.as_ctrl()))
        .collect();
    for spec in ACTIONS.iter().filter(|a| a.scope == Scope::Prefix) {
        for key in out.keys.get(spec.id).into_iter().flatten() {
            if arm.contains(key) {
                out.errors.push(finding(
                    spec.id,
                    key,
                    format!(
                        "{key} is the pane prefix key: pressed after the prefix it goes to the terminal"
                    ),
                ));
            }
        }
    }
    for ((_, key), ids) in &taken {
        if ids.len() < 2 {
            continue;
        }
        let names: Vec<&str> = ids
            .iter()
            .map(|id| action(id).map_or(*id, |a| a.label))
            .collect();
        // Report on the action(s) the user changed (else the first).
        let blamed: Vec<&&str> = ids
            .iter()
            .filter(|id| overrides.contains_key(**id))
            .collect();
        let target = blamed.first().map_or(ids[0], |id| **id);
        out.errors.push(finding(
            target,
            key,
            format!("{key} is bound twice: {}", names.join(" and ")),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(id, keys)| {
                (
                    (*id).to_owned(),
                    keys.iter().map(|k| (*k).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn the_defaults_are_a_valid_keymap() {
        let check = check(&BTreeMap::new());
        assert!(check.errors.is_empty(), "{:?}", check.errors);
        assert_eq!(check.keys["pane.zoom"], vec!["alt+z".to_owned()]);
        // Action ids are unique.
        let mut ids: Vec<_> = ACTIONS.iter().map(|a| a.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ACTIONS.len());
    }

    #[test]
    fn bindings_are_canonicalized() {
        let check = check(&map(&[
            ("pane.zoom", &["Shift+Alt+M", "option+Z"]),
            ("prefix.zoom", &["Z"]),
        ]));
        assert!(check.errors.is_empty(), "{:?}", check.errors);
        assert_eq!(
            check.keys["pane.zoom"],
            vec!["alt+shift+m".to_owned(), "alt+z".to_owned()]
        );
        // A prefix key is the character typed: case matters.
        assert_eq!(check.keys["prefix.zoom"], vec!["Z".to_owned()]);
    }

    #[test]
    fn reserved_and_terminal_keys_are_refused() {
        for (key, needle) in [
            ("ctrl+w", "reserved"),
            ("mod+t", "reserved"),
            ("ctrl+tab", "reserved"),
            ("meta+k", "reserved"),
            ("ctrl+c", "terminal needs it"),
            ("ctrl+d", "terminal needs it"),
            ("k", "terminal needs it"),
            ("shift+k", "terminal needs it"),
            ("enter", "terminal needs it"),
            ("hyper+k", "not a modifier"),
            ("alt+banana", "not a key name"),
        ] {
            let check = check(&map(&[("pane.zoom", &[key])]));
            assert!(
                check
                    .errors
                    .iter()
                    .any(|f| f.action == "pane.zoom" && f.message.contains(needle)),
                "{key}: {:?}",
                check.errors
            );
        }
        for key in ["ctrl+shift+c", "mod+shift+c", "ctrl+alt+delete"] {
            let check = check(&map(&[("palette.open", &[key])]));
            assert!(
                check
                    .errors
                    .iter()
                    .any(|f| f.action == "palette.open" && f.message.contains("reserved")),
                "{key}: {:?}",
                check.errors
            );
        }
        // A function key alone is fine.
        assert!(check(&map(&[("pane.zoom", &["f8"])])).errors.is_empty());
    }

    #[test]
    fn a_key_bound_twice_in_one_scope_is_a_conflict() {
        let check = check(&map(&[("pane.zoom", &["alt+n"])]));
        let conflict = check
            .errors
            .iter()
            .find(|f| f.message.contains("bound twice"))
            .expect("conflict");
        assert_eq!(conflict.action, "pane.zoom");
        assert!(conflict.message.contains("New session"), "{conflict:?}");
        // The default range covers alt+3.
        let check = super::check(&map(&[("pane.close", &["alt+3"])]));
        assert!(
            check
                .errors
                .iter()
                .any(|f| f.message.contains("bound twice"))
        );
        // Moving the other one away resolves it.
        let check = super::check(&map(&[
            ("pane.zoom", &["alt+n"]),
            ("session.new", &["alt+shift+o"]),
        ]));
        assert!(check.errors.is_empty(), "{:?}", check.errors);
        // mod is ctrl.
        let check = super::check(&map(&[("pane.zoom", &["ctrl+k"])]));
        assert!(
            check
                .errors
                .iter()
                .any(|f| f.message.contains("bound twice"))
        );
        // Scopes do not collide: `z` after the prefix is not alt+z.
        let check = super::check(&map(&[
            ("prefix.zoom", &["x"]),
            ("prefix.close_pane", &["z"]),
        ]));
        assert!(check.errors.is_empty(), "{:?}", check.errors);
        let check = super::check(&map(&[("prefix.zoom", &["x"])]));
        assert!(
            check
                .errors
                .iter()
                .any(|f| f.message.contains("bound twice"))
        );
    }

    #[test]
    fn ranges_belong_to_indexed_actions_only() {
        assert!(
            check(&map(&[("session.jump", &["alt+shift+1..9"])]))
                .errors
                .is_empty()
        );
        assert!(
            !check(&map(&[("session.jump", &["alt+j"])]))
                .errors
                .is_empty()
        );
        assert!(
            !check(&map(&[("pane.zoom", &["alt+1..9"])]))
                .errors
                .is_empty()
        );
        // Unbinding is allowed.
        let unbound = check(&map(&[("session.jump", &[])]));
        assert!(unbound.errors.is_empty());
        assert!(unbound.keys["session.jump"].is_empty());
    }

    #[test]
    fn terminal_costs_are_warnings() {
        let check = check(&map(&[
            ("pane.zoom", &["alt+b"]),
            ("pane.close", &["mod+y"]),
        ]));
        assert!(check.errors.is_empty(), "{:?}", check.errors);
        assert!(
            check
                .warnings
                .iter()
                .any(|w| w.key == "alt+b" && w.message.contains("readline"))
        );
        assert!(
            check
                .warnings
                .iter()
                .any(|w| w.key == "mod+y" && w.message.contains("^Y"))
        );
    }

    #[test]
    fn desktop_keys_are_warnings() {
        for key in ["ctrl+alt+t", "ctrl+space", "mod+space", "ctrl+shift+v"] {
            let check = check(&map(&[("palette.open", &[key])]));
            assert!(check.errors.is_empty(), "{key}: {:?}", check.errors);
            assert!(
                check
                    .warnings
                    .iter()
                    .any(|w| w.action == "palette.open" && w.message.contains("may never arrive")),
                "{key}: {:?}",
                check.warnings
            );
        }
    }

    #[test]
    fn a_prefix_action_cannot_take_the_prefix_key() {
        let check = check(&map(&[("prefix.zoom", &["ctrl+b"])]));
        assert!(
            check
                .errors
                .iter()
                .any(|f| f.action == "prefix.zoom" && f.message.contains("pane prefix key")),
            "{:?}",
            check.errors
        );
        // Follows a rebound prefix.
        let moved = super::check(&map(&[
            ("prefix.arm", &["mod+a"]),
            ("prefix.zoom", &["ctrl+b"]),
        ]));
        assert!(moved.errors.is_empty(), "{:?}", moved.errors);
        let clash = super::check(&map(&[
            ("prefix.arm", &["mod+a"]),
            ("prefix.zoom", &["ctrl+a"]),
        ]));
        assert!(!clash.errors.is_empty());
    }

    #[test]
    fn unknown_actions_and_too_many_keys_are_refused() {
        assert!(!check(&map(&[("pane.nope", &["alt+q"])])).errors.is_empty());
        assert!(
            !check(&map(&[(
                "pane.zoom",
                &["alt+q", "alt+a", "alt+s", "alt+e", "alt+o"]
            )]))
            .errors
            .is_empty()
        );
    }
}
