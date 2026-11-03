//! The hub's operator docs checked against the code they describe, so a
//! verb, flag, `hub.toml` key, default key binding, route, or plugin event
//! added, renamed, or removed fails the build until the docs follow:
//!
//! * `docs/hub/reference.md` `## Verbs`: every `fw hub` (sub)command clap
//!   knows has a `fw hub <path> ...` line in a code block naming each of its
//!   flags, and names no flag it lacks. Shared flag sets are markers:
//!   `{remote}` (`--url --token --fingerprint`), `{machine}` (`--machine`),
//!   `{serve}` (the `## Serve flags` table); `--state-dir` is every verb's
//!   but `report`'s.
//! * `## Serve flags` lists exactly the serve flags; `## hub.toml` holds
//!   [`config::EXAMPLE`] verbatim and one row per [`config::KEYS`] entry
//!   with when it applies.
//! * `## Keyboard shortcuts` holds the table generated from
//!   [`keys::ACTIONS`] (printed on failure, ready to paste).
//! * `docs/hub/api.md` lists every path the router mounts, lists no path
//!   it does not, and has one `## Plugin events` row per
//!   [`plugins::EVENTS`] entry.
//! * `docs/hub.md` (the front door) names every top-level verb and stays
//!   short.
//!
//! [`config::EXAMPLE`]: crate::config::EXAMPLE
//! [`config::KEYS`]: crate::config::KEYS
//! [`keys::ACTIONS`]: crate::settings::keys::ACTIONS
//! [`plugins::EVENTS`]: crate::plugins::EVENTS

use crate::config::{Apply, EXAMPLE, KEYS};
use crate::settings::keys::ACTIONS;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// The front door's size budget: details belong in `docs/hub/`.
const FRONT_DOOR_MAX_BYTES: usize = 12 * 1024;

/// The router's source: the routes it mounts are read from it.
const SERVER_RS: &str = include_str!("server.rs");

fn docs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

fn read_doc(relative: &str) -> String {
    let path = docs_dir().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The body of `## <heading>`: up to the next `## ` heading outside a code
/// block.
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    let marker = format!("\n## {heading}\n");
    let start = text
        .find(&marker)
        .unwrap_or_else(|| panic!("no `## {heading}` section"))
        + marker.len();
    let body = &text[start..];
    let mut fenced = false;
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        if line.starts_with("```") {
            fenced = !fenced;
        } else if !fenced && line.starts_with("## ") {
            return &body[..offset];
        }
        offset += line.len();
    }
    body
}

/// The visible long flags of `cmd`, without `--help`.
fn long_flags(cmd: &clap::Command) -> BTreeSet<String> {
    let mut flags: BTreeSet<String> = cmd
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .filter_map(|arg| arg.get_long().map(str::to_owned))
        .collect();
    flags.remove("help");
    flags
}

/// The visible aliases of `cmd`'s long flags (`--repo` for `--folder`):
/// a doc may name them, need not.
fn aliases(cmd: &clap::Command) -> BTreeSet<String> {
    cmd.get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .flat_map(|arg| arg.get_visible_aliases().unwrap_or_default())
        .map(str::to_owned)
        .collect()
}

/// Every (sub)command under `cmd` with its space-separated path.
fn walk<'a>(cmd: &'a clap::Command, path: &str, out: &mut Vec<(String, &'a clap::Command)>) {
    for sub in cmd.get_subcommands() {
        if sub.get_name() == "help" {
            continue;
        }
        let sub_path = if path.is_empty() {
            sub.get_name().to_owned()
        } else {
            format!("{path} {}", sub.get_name())
        };
        out.push((sub_path.clone(), sub));
        walk(sub, &sub_path, out);
    }
}

/// A verb is documented by its own line unless it only groups others
/// (`fw hub fanout`), i.e. has subcommands and nothing of its own.
fn needs_entry(cmd: &clap::Command) -> bool {
    cmd.get_subcommands().next().is_none()
        || cmd
            .get_arguments()
            .any(|arg| !arg.is_hide_set() && arg.get_id().as_str() != "help")
}

/// Every `--flag` named in `text`.
fn flags_in(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut found = BTreeSet::new();
    let mut i = 0;
    while let Some(at) = text[i..].find("--") {
        let start = i + at;
        let name_start = start + 2;
        let mut end = name_start;
        while end < bytes.len()
            && (bytes[end].is_ascii_lowercase()
                || bytes[end].is_ascii_digit()
                || bytes[end] == b'-')
        {
            end += 1;
        }
        let boundary = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        if boundary && end > name_start && bytes[name_start] != b'-' {
            found.insert(text[name_start..end].to_owned());
        }
        i = end.max(start + 2);
    }
    found
}

/// The `fw hub ...` lines of a section's code blocks, keyed by the verb
/// path they resolve to in `tree` (`""`: no verb), the syntax part only
/// (before a `  #` comment). An indented line continues the one above.
fn entries(text: &str, tree: &clap::Command) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut fenced = false;
    let mut open = false;
    for line in text.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
            open = false;
            continue;
        }
        if !fenced {
            continue;
        }
        let syntax = line.split("  #").next().unwrap_or("");
        if let Some(rest) = line.strip_prefix("fw hub ") {
            let mut cmd = tree;
            let mut path: Vec<&str> = Vec::new();
            for token in rest.split_whitespace() {
                match cmd.find_subcommand(token) {
                    Some(sub) if token != "help" => {
                        path.push(token);
                        cmd = sub;
                    }
                    _ => break,
                }
            }
            out.push((path.join(" "), syntax.to_owned()));
            open = true;
        } else if open && line.starts_with("    ") {
            let last = out.last_mut().expect("an open entry");
            last.1.push(' ');
            last.1.push_str(syntax);
        } else {
            open = false;
        }
    }
    out
}

/// Cells of a markdown table row (`| a | b |` -> `["a", "b"]`).
fn cells(row: &str) -> Vec<&str> {
    let inner = row.trim().trim_start_matches('|').trim_end_matches('|');
    inner.split(" | ").map(str::trim).collect()
}

/// A code span for `text` (doubled backticks when it holds one).
fn code(text: &str) -> String {
    if text.contains('`') {
        format!("`` {text} ``")
    } else {
        format!("`{text}`")
    }
}

/// The serve flags: `serve`'s own, less the state dir and handoff.
fn serve_flags(tree: &clap::Command) -> BTreeSet<String> {
    let serve = tree.find_subcommand("serve").expect("fw hub serve");
    let mut flags = long_flags(serve);
    flags.remove("state-dir");
    flags.remove("handoff");
    flags
}

fn tree() -> clap::Command {
    let mut tree = crate::cli::command();
    tree.build();
    tree
}

#[test]
fn reference_documents_every_verb_and_flag() {
    let tree = tree();
    let reference = read_doc("hub/reference.md");
    let verbs = section(&reference, "Verbs");
    let mut by_path: BTreeMap<String, String> = BTreeMap::new();
    let mut errors = Vec::new();
    for (path, syntax) in entries(verbs, &tree) {
        if path.is_empty() {
            errors.push(format!("`{syntax}`: no such verb"));
            continue;
        }
        let joined = by_path.entry(path).or_default();
        joined.push(' ');
        joined.push_str(&syntax);
    }
    let serve = serve_flags(&tree);
    let mut all = Vec::new();
    walk(&tree, "", &mut all);
    let known: BTreeSet<&str> = all.iter().map(|(path, _)| path.as_str()).collect();
    for path in by_path.keys() {
        let cmd = all.iter().find(|(p, _)| p == path).map(|(_, c)| *c);
        if cmd.is_none_or(|cmd| !needs_entry(cmd)) {
            errors.push(format!("`fw hub {path}` documented but only groups verbs"));
        }
    }
    for (path, cmd) in &all {
        if !needs_entry(cmd) {
            continue;
        }
        let Some(syntax) = by_path.get(path) else {
            errors.push(format!(
                "`fw hub {path}` has no line in reference.md ## Verbs"
            ));
            continue;
        };
        let flags = long_flags(cmd);
        let mut covered = BTreeSet::new();
        let state_dir = flags.contains("state-dir");
        if state_dir == (path == "report") {
            errors.push(format!(
                "`fw hub {path}`: --state-dir {} (the Conventions say every verb but report takes it)",
                if state_dir { "present" } else { "absent" }
            ));
        }
        covered.insert("state-dir".to_owned());
        let markers: [(&str, &[&str]); 3] = [
            ("{remote}", &["url", "token", "fingerprint"]),
            ("{machine}", &["machine"]),
            ("{serve}", &[]),
        ];
        for (marker, set) in markers {
            let set: BTreeSet<String> = if marker == "{serve}" {
                serve.clone()
            } else {
                set.iter().map(|s| (*s).to_owned()).collect()
            };
            let has = set.iter().all(|f| flags.contains(f));
            if syntax.contains(marker) != has {
                errors.push(format!(
                    "`fw hub {path}`: {marker} {} but the verb {} those flags",
                    if has { "missing" } else { "shown" },
                    if has { "takes" } else { "lacks" }
                ));
            }
            if has {
                covered.extend(set);
            }
        }
        let named = flags_in(syntax);
        let aliases = aliases(cmd);
        for flag in flags.difference(&covered) {
            if !named.contains(flag) {
                errors.push(format!("`fw hub {path}`: --{flag} not documented"));
            }
        }
        for flag in &named {
            if !flags.contains(flag) && !aliases.contains(flag) {
                errors.push(format!(
                    "`fw hub {path}`: documents --{flag}, which it does not take"
                ));
            }
        }
    }
    let table: BTreeSet<String> = section(&reference, "Serve flags")
        .lines()
        .filter_map(|row| row.strip_prefix("| `--"))
        .filter_map(|rest| rest.split('`').next())
        .map(str::to_owned)
        .collect();
    if table != serve {
        errors.push(format!(
            "## Serve flags lists {table:?}, `fw hub serve` takes {serve:?}"
        ));
    }
    assert!(known.contains("serve") && known.contains("fanout new"));
    assert!(
        errors.is_empty(),
        "docs/hub/reference.md drifted:\n{}",
        errors.join("\n")
    );
}

#[test]
fn reference_documents_every_hub_toml_key() {
    let tree = tree();
    let serve = serve_flags(&tree);
    let reference = read_doc("hub/reference.md");
    let body = section(&reference, "hub.toml");
    assert!(
        body.contains(&format!("```toml\n{EXAMPLE}```")),
        "## hub.toml must hold config::EXAMPLE verbatim:\n```toml\n{EXAMPLE}```"
    );
    let mut errors = Vec::new();
    let mut rows = BTreeMap::new();
    for row in body.lines().filter(|l| l.starts_with("| `")) {
        let cells = cells(row);
        let key = cells[0].trim_matches('`').to_owned();
        rows.insert(
            key,
            (cells[1].to_owned(), cells[cells.len() - 1].to_owned()),
        );
    }
    let mut flags_used = BTreeSet::new();
    for (key, apply) in KEYS {
        let Some((flag, applies)) = rows.remove(*key) else {
            errors.push(format!("`{key}` has no row"));
            continue;
        };
        let want = match apply {
            Apply::Live => "live",
            Apply::Restart => "restart",
        };
        if applies != want {
            errors.push(format!("`{key}` applies `{want}`, documented `{applies}`"));
        }
        if flag != "-" {
            let name = flag.trim_matches('`').trim_start_matches("--").to_owned();
            if !serve.contains(&name) {
                errors.push(format!("`{key}`: {flag} is not a serve flag"));
            }
            flags_used.insert(name);
        }
    }
    for key in rows.keys() {
        errors.push(format!("`{key}` documented but not a hub.toml key"));
    }
    // "Every one but --notify-url is also a hub.toml key".
    for flag in serve.difference(&flags_used) {
        if flag != "notify-url" {
            errors.push(format!("serve flag --{flag} maps to no hub.toml key row"));
        }
    }
    assert!(
        errors.is_empty(),
        "docs/hub/reference.md ## hub.toml drifted:\n{}",
        errors.join("\n")
    );
}

/// The key table, generated from the keymap catalog.
fn key_table() -> String {
    let mut table = String::from("| Action | Default | |\n|---|---|---|\n");
    for action in ACTIONS {
        let defaults = if action.defaults.is_empty() {
            "-".to_owned()
        } else {
            action
                .defaults
                .iter()
                .map(|k| code(k))
                .collect::<Vec<_>>()
                .join(", ")
        };
        table.push_str(&format!(
            "| `{}` | {defaults} | {} |\n",
            action.id, action.label
        ));
    }
    table
}

#[test]
fn reference_key_table_matches_the_keymap() {
    let reference = read_doc("hub/reference.md");
    let body = section(&reference, "Keyboard shortcuts");
    let table = key_table();
    assert!(
        body.contains(&table),
        "## Keyboard shortcuts must hold the table generated from settings::keys::ACTIONS:\n{table}"
    );
}

/// Every path literal `router()` mounts, catch-alls of the API excluded.
fn routes() -> BTreeSet<String> {
    let start = SERVER_RS.find("pub fn router(").expect("router()");
    let body = &SERVER_RS[start..];
    let body = &body[..body.find("\n}\n").expect("router() ends")];
    let mut routes = BTreeSet::new();
    let mut rest = body;
    while let Some(at) = rest.find(".route(") {
        rest = &rest[at + ".route(".len()..];
        let open = rest.find('"').expect("a path literal");
        let literal = &rest[open + 1..];
        let close = literal.find('"').expect("closing quote");
        let path = &literal[..close];
        if path != "/api" && path != "/api/{*rest}" {
            routes.insert(path.to_owned());
        }
        rest = &literal[close..];
    }
    routes
}

#[test]
fn api_lists_every_route_and_no_other() {
    let api = read_doc("hub/api.md");
    let routes = routes();
    assert!(routes.len() > 80, "router parse found only {routes:?}");
    let mut documented = BTreeSet::new();
    for row in api.lines().filter(|l| l.starts_with("| ")) {
        let cells = cells(row);
        if cells.len() < 3 || !cells[1].starts_with("`/") {
            continue;
        }
        let path = cells[1]
            .trim_start_matches('`')
            .split(['`', '[', '?'])
            .next()
            .unwrap_or_default()
            .to_owned();
        documented.insert(path);
    }
    let missing: Vec<_> = routes.difference(&documented).collect();
    let stale: Vec<_> = documented.difference(&routes).collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "docs/hub/api.md drifted from server::router(): missing {missing:?}, not mounted {stale:?}"
    );
}

#[test]
fn api_lists_every_plugin_event() {
    let api = read_doc("hub/api.md");
    let body = section(&api, "Plugins");
    let documented: BTreeSet<String> = body
        .lines()
        .filter_map(|row| row.strip_prefix("| `"))
        .filter_map(|rest| rest.split('`').next())
        .map(str::to_owned)
        .collect();
    let events: BTreeSet<String> = crate::plugins::EVENTS
        .iter()
        .map(|e| (*e).to_owned())
        .collect();
    assert_eq!(
        documented, events,
        "docs/hub/api.md plugin events drifted from plugins::EVENTS"
    );
}

#[test]
fn front_door_is_short_and_names_every_verb() {
    let front = read_doc("hub.md");
    assert!(
        front.len() <= FRONT_DOOR_MAX_BYTES,
        "docs/hub.md is {} bytes (budget {FRONT_DOOR_MAX_BYTES}): move detail into docs/hub/",
        front.len()
    );
    let verbs = section(&front, "Verbs");
    let tree = tree();
    let missing: Vec<&str> = tree
        .get_subcommands()
        .map(clap::Command::get_name)
        .filter(|name| *name != "help" && !verbs.contains(&format!("`{name}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/hub.md ## Verbs misses {missing:?}"
    );
    for page in ["reference", "api", "security", "workflows", "internals"] {
        assert!(
            front.contains(&format!("(hub/{page}.md")),
            "docs/hub.md links no hub/{page}.md"
        );
    }
}

#[test]
fn flag_scanner_reads_flags_not_dashes() {
    let found = flags_in("fw hub new WS [--kind K] [-- ARGV...] [--yes|-y] a--b --x-y");
    let want: BTreeSet<String> = ["kind", "yes", "x-y"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    assert_eq!(found, want);
    assert_eq!(code("alt+`"), "`` alt+` ``");
    assert_eq!(cells("| `a` | b | c |"), vec!["`a`", "b", "c"]);
}

/// GitHub's anchor for a heading: lowercase, punctuation dropped, spaces
/// to hyphens.
fn anchor(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// The anchors of `text`'s headings (outside code blocks).
fn anchors(text: &str) -> BTreeSet<String> {
    let mut fenced = false;
    let mut out = BTreeSet::new();
    for line in text.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
        } else if !fenced && line.starts_with('#') {
            out.insert(anchor(line.trim_start_matches('#').trim()));
        }
    }
    out
}

#[test]
fn hub_doc_links_resolve() {
    let pages = [
        "hub.md",
        "hub/reference.md",
        "hub/api.md",
        "hub/security.md",
        "hub/workflows.md",
        "hub/internals.md",
    ];
    let mut errors = Vec::new();
    for page in pages {
        let text = read_doc(page);
        let dir = std::path::Path::new(page)
            .parent()
            .unwrap_or(std::path::Path::new(""));
        let mut rest = text.as_str();
        while let Some(at) = rest.find("](") {
            rest = &rest[at + 2..];
            let Some(end) = rest.find(')') else { break };
            let target = &rest[..end];
            rest = &rest[end..];
            if target.contains("://") || target.contains(' ') {
                continue;
            }
            let (file, fragment) = target.split_once('#').unwrap_or((target, ""));
            let resolved = if file.is_empty() {
                page.to_owned()
            } else {
                let joined = dir.join(file);
                // Normalize `hub/../x.md` -> `x.md`.
                let mut parts: Vec<String> = Vec::new();
                for part in joined.components() {
                    match part {
                        std::path::Component::ParentDir => {
                            parts.pop();
                        }
                        other => parts.push(other.as_os_str().to_string_lossy().into_owned()),
                    }
                }
                parts.join("/")
            };
            let path = docs_dir().join(&resolved);
            let Ok(target_text) = std::fs::read_to_string(&path) else {
                errors.push(format!("{page}: link to missing {resolved}"));
                continue;
            };
            if !fragment.is_empty() && !anchors(&target_text).contains(fragment) {
                errors.push(format!("{page}: no #{fragment} in {resolved}"));
            }
        }
    }
    assert!(
        errors.is_empty(),
        "broken hub doc links:\n{}",
        errors.join("\n")
    );
    assert_eq!(
        anchor("Terminal client: `fw hub attach`"),
        "terminal-client-fw-hub-attach"
    );
    assert_eq!(anchor("hub.toml"), "hubtoml");
}
