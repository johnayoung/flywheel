//! A blocked agent's pending request -- from Claude Code's
//! `PermissionRequest` hook payload, else read off the permission dialog
//! on its screen (Claude Code 2.1.306, Codex's approval overlays) -- and
//! the narrowest rules that would let it through next time.

use super::rule::{self, Anchors, Risk, Rule, Target};
use super::shell::{self, Part, Word};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// What a blocked agent asks to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Request {
    /// `claude` or `codex`.
    pub agent: &'static str,
    /// The tool (`Bash`, `Edit`, `WebFetch`, `mcp__github__create_issue`;
    /// Codex: `exec`, `patch`).
    pub tool: String,
    pub target: Target,
    /// Where it was read: `hook` (the payload) or `screen` (the dialog).
    pub source: &'static str,
    /// Identifies this request (an answer is only sent to the request it
    /// was proposed for).
    pub key: String,
}

impl Request {
    fn new(agent: &'static str, tool: String, target: Target, source: &'static str) -> Self {
        let digest = Sha256::new()
            .chain_update(agent.as_bytes())
            .chain_update([0])
            .chain_update(tool.as_bytes())
            .chain_update([0])
            .chain_update(
                serde_json::to_string(&target)
                    .unwrap_or_default()
                    .as_bytes(),
            )
            .finalize();
        let key = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        Self {
            agent,
            tool,
            target,
            source,
            key,
        }
    }

    /// One line for logs and lists (`Bash: cargo test`).
    pub fn summary(&self) -> String {
        let what = match &self.target {
            Target::Command { command } => command.lines().next().unwrap_or("").to_owned(),
            Target::Edit { path } | Target::Read { path } => path.display().to_string(),
            Target::Fetch { url } => url.clone(),
            Target::Tool { name } => name.clone(),
        };
        format!("{}: {what}", self.tool)
    }
}

/// An absolute path for a tool's file argument (relative ones are from
/// the session's directory).
fn absolute(path: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(path.trim());
    rule::normalize(&if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

/// Tools that ask a question rather than for a permission.
const NOT_PERMISSIONS: &[&str] = &["AskUserQuestion", "ExitPlanMode", "EnterPlanMode"];

/// The request in a Claude Code `PermissionRequest` hook payload.
pub fn from_hook(payload: &serde_json::Value, cwd: &Path) -> Option<Request> {
    let tool = payload.get("tool_name")?.as_str()?.trim();
    if tool.is_empty() || NOT_PERMISSIONS.contains(&tool) {
        return None;
    }
    let input = payload
        .get("tool_input")
        .unwrap_or(&serde_json::Value::Null);
    let text = |key: &str| {
        input
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
    };
    let target = match tool {
        "Bash" => Target::Command {
            command: text("command")?.to_owned(),
        },
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => Target::Edit {
            path: absolute(text("file_path").or_else(|| text("notebook_path"))?, cwd),
        },
        "Read" | "Glob" | "Grep" | "LS" => Target::Read {
            path: absolute(text("file_path").or_else(|| text("path"))?, cwd),
        },
        "WebFetch" => Target::Fetch {
            url: text("url")?.to_owned(),
        },
        _ => Target::Tool {
            name: tool.to_owned(),
        },
    };
    Some(Request::new("claude", tool.to_owned(), target, "hook"))
}

fn is_rule_line(line: &str, c: char) -> bool {
    let t = line.trim();
    t.chars().count() >= 20 && t.chars().all(|x| x == c)
}

/// The request on a Claude Code permission dialog (the live screen's
/// rows): the dialog's header names what it asks (`Bash command`,
/// `Create file`, `Edit file`, `Fetch`), its body the command or file.
pub fn from_claude_screen(rows: &[String], cwd: &Path) -> Option<Request> {
    let rows: Vec<&str> = rows.iter().map(|r| r.trim_end()).collect();
    let last = rows.iter().rposition(|r| !r.trim().is_empty())?;
    // The dialog is what the screen ends with: its footer is in the last
    // few rows.
    let footer = (last.saturating_sub(4)..=last)
        .rev()
        .find(|&i| rows[i].contains("Esc to cancel"))?;
    let top = rows[..footer].iter().rposition(|r| is_rule_line(r, '─'))?;
    let body: Vec<&str> = rows[top + 1..footer].to_vec();
    let mut lines = body.iter().copied().filter(|l| !l.trim().is_empty());
    let header = lines.next()?.trim();
    match header {
        "Bash command" => {
            let dashes: Vec<usize> = body
                .iter()
                .enumerate()
                .filter(|(_, l)| is_rule_line(l, '╌'))
                .map(|(i, _)| i)
                .collect();
            let (&a, &b) = (dashes.first()?, dashes.get(1)?);
            let command: Vec<&str> = body[a + 1..b]
                .iter()
                .map(|l| l.strip_prefix(' ').unwrap_or(l))
                .collect();
            let command = command.join("\n").trim().to_owned();
            (!command.is_empty()).then(|| {
                Request::new(
                    "claude",
                    "Bash".into(),
                    Target::Command { command },
                    "screen",
                )
            })
        }
        "Create file" | "Edit file" | "Overwrite file" => {
            let path = lines.next()?.trim();
            Some(Request::new(
                "claude",
                if header == "Edit file" {
                    "Edit"
                } else {
                    "Write"
                }
                .into(),
                Target::Edit {
                    path: absolute(path, cwd),
                },
                "screen",
            ))
        }
        h if h.starts_with("Fetch") => {
            let url = body
                .iter()
                .flat_map(|l| l.split_whitespace())
                .find(|w| w.starts_with("https://") || w.starts_with("http://"))?;
            Some(Request::new(
                "claude",
                "WebFetch".into(),
                Target::Fetch {
                    url: url.to_owned(),
                },
                "screen",
            ))
        }
        _ => None,
    }
}

/// An option row of a Codex overlay (`› 1. Yes, proceed (y)`).
fn codex_option(line: &str) -> bool {
    let t = line.trim_start().trim_start_matches('›').trim_start();
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && t[digits..].starts_with(". ")
}

/// The request on a Codex approval overlay: a command (`$ ...`, with
/// heredoc lines after it) or a patch (`Destination: ...`).
pub fn from_codex_screen(rows: &[String], cwd: &Path) -> Option<Request> {
    let rows: Vec<&str> = rows.iter().map(|r| r.trim_end()).collect();
    if let Some(at) = rows
        .iter()
        .rposition(|r| r.trim() == "Would you like to run the following command?")
    {
        let start = (at + 1..rows.len()).find(|&i| rows[i].trim_start().starts_with("$ "))?;
        let indent = rows[start].len() - rows[start].trim_start().len();
        let mut command = vec![rows[start].trim_start()[2..].to_owned()];
        for row in &rows[start + 1..] {
            if codex_option(row) || row.trim_start().starts_with("Press enter") {
                break;
            }
            let cut = row.len() - row.trim_start().len();
            command.push(row[cut.min(indent)..].to_owned());
        }
        while command.last().is_some_and(|l| l.trim().is_empty()) {
            command.pop();
        }
        let command = command.join("\n").trim().to_owned();
        return (!command.is_empty()).then(|| {
            Request::new(
                "codex",
                "exec".into(),
                Target::Command { command },
                "screen",
            )
        });
    }
    if let Some(at) = rows.iter().rposition(|r| r.contains("Allow command?")) {
        // The legacy boxed overlay: `│ $ cargo fmt --all   │`.
        let line = rows[at + 1..]
            .iter()
            .map(|r| r.trim().trim_matches('│').trim())
            .find(|r| r.starts_with("$ "))?;
        return Some(Request::new(
            "codex",
            "exec".into(),
            Target::Command {
                command: line[2..].trim().to_owned(),
            },
            "screen",
        ));
    }
    if let Some(at) = rows
        .iter()
        .rposition(|r| r.trim() == "Would you like to make the following edits?")
    {
        let path = rows[at + 1..]
            .iter()
            .take_while(|r| !codex_option(r))
            .find_map(|r| r.trim().strip_prefix("Destination: "))
            .map(str::to_owned)
            .or_else(|| {
                // `src/lib.rs (+30 -0)`
                rows[at + 1..].iter().find_map(|r| {
                    let t = r.trim();
                    let (path, rest) = t.split_once(" (+")?;
                    rest.ends_with(')').then(|| path.to_owned())
                })
            })?;
        return Some(Request::new(
            "codex",
            "patch".into(),
            Target::Edit {
                path: absolute(&path, cwd),
            },
            "screen",
        ));
    }
    None
}

// --- proposals -------------------------------------------------------------------

/// One rule the hub can offer for a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub rule: String,
    /// `exact`, `scoped` (program, subcommand, and scoping options, then
    /// any arguments), `program` (any use of the program), `file`, `dir`,
    /// `domain`, `subdomains`, `tool`, `server`.
    pub kind: &'static str,
    pub risk: Risk,
    /// "lets the agent ... without asking".
    pub sentence: String,
}

/// What the hub offers for a request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Proposal {
    /// The rules "Always allow" writes (empty: none is safe to offer --
    /// see `notes` and `candidates`).
    pub rules: Vec<String>,
    /// For a single command (or file, domain, tool): every rule offered,
    /// narrowest first.
    pub candidates: Vec<Candidate>,
    pub notes: Vec<String>,
}

/// Program table: how many subcommand words it takes, and its options
/// that narrow what it acts on (`-p pkg`), each with whether it takes a
/// value.
struct Program {
    names: &'static [&'static str],
    depth: usize,
    scoping: &'static [(&'static str, bool)],
}

const PROGRAMS: &[Program] = &[
    Program {
        names: &["cargo"],
        depth: 1,
        scoping: &[
            ("-p", true),
            ("--package", true),
            ("--bin", true),
            ("--test", true),
            ("--example", true),
            ("--features", true),
            ("-F", true),
            ("--workspace", false),
            ("--all", false),
            ("--lib", false),
            ("--bins", false),
            ("--tests", false),
            ("--all-targets", false),
            ("--all-features", false),
            ("--release", false),
            ("--locked", false),
            ("--offline", false),
        ],
    },
    Program {
        names: &["git"],
        depth: 1,
        scoping: &[],
    },
    Program {
        names: &["go"],
        depth: 1,
        scoping: &[],
    },
    Program {
        names: &["npm", "pnpm", "yarn", "bun", "uv", "poetry", "pdm", "deno"],
        depth: 1,
        scoping: &[
            ("-w", true),
            ("--workspace", true),
            ("--filter", true),
            ("-F", true),
            ("-r", false),
        ],
    },
    Program {
        names: &["gh"],
        depth: 2,
        scoping: &[],
    },
    Program {
        names: &[
            "docker",
            "podman",
            "kubectl",
            "helm",
            "terraform",
            "make",
            "just",
            "brew",
            "pip",
            "pip3",
            "dotnet",
            "mvn",
            "gradle",
            "./gradlew",
            "rustup",
            "mix",
            "bundle",
            "rake",
            "swift",
            "flutter",
            "dart",
            "zig",
        ],
        depth: 1,
        scoping: &[],
    },
];

/// Subcommands that take one more word that names what runs
/// (`npm run build`, `docker compose up`).
const RUNNERS: &[&str] = &["run", "compose", "exec", "x"];

fn subcommand_word(word: &Word) -> bool {
    let t = &word.text;
    !word.quoted
        && t.len() <= 40
        && t.starts_with(|c: char| c.is_ascii_lowercase())
        && t.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn script_word(word: &Word) -> bool {
    let t = &word.text;
    !word.quoted
        && !t.is_empty()
        && t.len() <= 60
        && !t.starts_with('-')
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.' | '@' | '/'))
        && !t.contains("..")
}

/// How many leading words make the scoped prefix, and whether it names a
/// subcommand (a known program's).
fn scoped_len(words: &[Word]) -> (usize, bool) {
    let Some(first) = words.first() else {
        return (0, false);
    };
    let name = first.text.rsplit('/').next().unwrap_or(&first.text);
    let mut i = 1;
    // `python -m module`
    if matches!(name, "python" | "python3")
        && words.get(1).is_some_and(|w| w.text == "-m")
        && words.get(2).is_some_and(script_word)
    {
        return (3, true);
    }
    let Some(program) = PROGRAMS
        .iter()
        .find(|p| p.names.contains(&name) || p.names.contains(&first.text.as_str()))
    else {
        return (1, false);
    };
    // Program-level options before the subcommand: `git -C dir`, `cargo +nightly`.
    loop {
        match words.get(i) {
            Some(w) if name == "git" && w.text == "-C" && words.get(i + 1).is_some() => i += 2,
            Some(w) if name == "cargo" && w.text.starts_with('+') && !w.quoted => i += 1,
            _ => break,
        }
    }
    let mut depth = program.depth;
    let mut subcommands = 0;
    while depth > 0 {
        let Some(w) = words.get(i) else { break };
        if !subcommand_word(w) {
            break;
        }
        i += 1;
        subcommands += 1;
        depth -= 1;
        if RUNNERS.contains(&w.text.as_str())
            && let Some(next) = words.get(i)
            && script_word(next)
        {
            i += 1;
            break;
        }
    }
    if subcommands == 0 {
        return (1, false);
    }
    // Then the options that narrow it.
    while let Some(w) = words.get(i) {
        let (flag, inline) = match w.text.split_once('=') {
            Some((flag, _)) if flag.starts_with("--") => (flag, true),
            _ => (w.text.as_str(), false),
        };
        let Some((_, takes_value)) = program.scoping.iter().find(|(f, _)| *f == flag) else {
            break;
        };
        if *takes_value && !inline {
            match words.get(i + 1) {
                Some(v) if script_word(v) => i += 2,
                _ => break,
            }
        } else {
            i += 1;
        }
    }
    (i, true)
}

/// Simple commands that never need a rule (Claude Code runs them without
/// asking): left out of what is proposed.
fn needs_no_rule(part: &Part, words: &[Word]) -> bool {
    if part.redirect {
        return false;
    }
    let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
    matches!(
        texts.as_slice(),
        ["cd", ..]
            | ["pwd"]
            | ["true"]
            | ["false"]
            | ["echo", ..]
            | ["ls", ..]
            | ["wc", ..]
            | ["which", ..]
            | ["git", "status", ..]
            | ["git", "log", ..]
            | ["git", "diff", ..]
    )
}

/// Most rules saved for one command line (Claude Code's own limit).
pub const MAX_PART_RULES: usize = 5;

/// The candidate Bash rules for one simple command, narrowest first:
/// `(candidates, the default's index)`.
fn bash_candidates(
    words: &[Word],
    anchors: &Anchors,
    repo: &Path,
) -> (Vec<Candidate>, Option<usize>) {
    let mut out: Vec<Candidate> = Vec::new();
    let mut push = |spec: String, kind: &'static str| {
        let raw = format!("Bash({spec})");
        let Ok(rule) = Rule::parse(&raw) else {
            return;
        };
        if out.iter().any(|c| rule::same(&c.rule, &raw)) {
            return;
        }
        out.push(Candidate {
            risk: rule::risk(&rule, anchors, repo),
            sentence: rule::describe(&rule, anchors),
            rule: raw,
            kind,
        });
    };
    let exact = rule::words_raw(words);
    push(exact, "exact");
    let (len, has_sub) = scoped_len(words);
    if has_sub {
        push(format!("{} *", rule::words_raw(&words[..len])), "scoped");
    }
    push(format!("{} *", words[0].raw), "program");
    let exact_ok = out.first().is_some_and(|c| !c.risk.dangerous);
    let scoped = out
        .iter()
        .position(|c| c.kind == "scoped" && !c.risk.dangerous);
    let default = scoped.or(exact_ok.then_some(0));
    (out, default)
}

/// Which settings file a rule is written to: decides what a leading `/`
/// means in a path rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    /// Local and project settings: `/` is the session's directory.
    Project,
    /// User settings: `/` is `~/.claude`, so paths are written `//abs`.
    User,
}

fn path_spec(path: &Path, anchors: &Anchors, anchor: Anchor) -> String {
    match (anchor, path.strip_prefix(&anchors.cwd)) {
        (Anchor::Project, Ok(rel)) if !rel.as_os_str().is_empty() => format!("/{}", rel.display()),
        _ => format!("/{}", path.display()),
    }
}

/// The rules offered to Claude Code for `request`, to be written to a
/// file anchored as `anchor`.
pub fn propose_claude(
    request: &Request,
    anchors: &Anchors,
    repo: &Path,
    anchor: Anchor,
) -> Proposal {
    let mut proposal = Proposal::default();
    let candidate = |raw: String, kind: &'static str| -> Option<Candidate> {
        let rule = Rule::parse(&raw).ok()?;
        Some(Candidate {
            risk: rule::risk(&rule, anchors, repo),
            sentence: rule::describe(&rule, anchors),
            rule: raw,
            kind,
        })
    };
    match &request.target {
        Target::Command { command } if rule::pipes_to_shell(command) => {
            proposal.notes.push(
                "this pipes into a shell or interpreter: approve it each time (no rule is safe)"
                    .into(),
            );
        }
        Target::Command { command } => {
            let parts = match shell::split(command) {
                Ok(parts) => parts,
                Err(e) => {
                    proposal
                        .notes
                        .push(format!("cannot read the command ({e}): approve it once"));
                    return proposal;
                }
            };
            let mut needing = Vec::new();
            for part in &parts {
                let skip = part
                    .words
                    .iter()
                    .take_while(|w| shell::is_assignment(w))
                    .count();
                let words = shell::effective_words(&part.words[skip..]);
                if words.is_empty() {
                    continue;
                }
                if part.substitution || part.heredoc {
                    proposal.notes.push(format!(
                        "`{}` runs code a rule cannot see (a substitution, subshell, or heredoc): approve it each time",
                        part.raw
                    ));
                    proposal.rules.clear();
                    proposal.candidates.clear();
                    return proposal;
                }
                if skip > 0 {
                    proposal.notes.push(format!(
                        "`{}` starts with a variable assignment: Claude Code matches allow rules past known-safe variables only",
                        part.raw
                    ));
                }
                if part.redirect {
                    proposal.notes.push(format!(
                        "`{}` redirects to or from a file: Claude Code may still ask about that file",
                        part.raw
                    ));
                }
                if needs_no_rule(part, words) {
                    continue;
                }
                needing.push(words.to_vec());
            }
            if needing.is_empty() {
                proposal
                    .notes
                    .push("nothing in this command needs a rule".into());
                return proposal;
            }
            if needing.len() > MAX_PART_RULES {
                proposal.notes.push(format!(
                    "{} commands in one line: more than the {MAX_PART_RULES} rules one approval may add",
                    needing.len()
                ));
                return proposal;
            }
            let single = needing.len() == 1;
            let mut all_safe = true;
            for words in &needing {
                let (candidates, default) = bash_candidates(words, anchors, repo);
                match default {
                    Some(i) => proposal.rules.push(candidates[i].rule.clone()),
                    None => all_safe = false,
                }
                if single {
                    proposal.candidates = candidates;
                } else if let Some(i) = default {
                    proposal.candidates.push(candidates[i].clone());
                }
            }
            if !all_safe {
                proposal.rules.clear();
                proposal.notes.push(
                    "no rule narrow enough to offer: only a typed confirmation writes one".into(),
                );
            }
        }
        Target::Edit { path } | Target::Read { path } => {
            let tool = if matches!(request.target, Target::Edit { .. }) {
                "Edit"
            } else {
                "Read"
            };
            let file = path_spec(path, anchors, anchor);
            proposal
                .candidates
                .extend(candidate(format!("{tool}({file})"), "file"));
            if let Some(dir) = path.parent()
                && dir != Path::new("/")
            {
                let dir_spec = path_spec(dir, anchors, anchor);
                let dir_spec = if dir == anchors.cwd {
                    "/".to_owned()
                } else {
                    format!("{dir_spec}/")
                };
                proposal
                    .candidates
                    .extend(candidate(format!("{tool}({dir_spec}**)"), "dir"));
            }
            if let Some(first) = proposal.candidates.iter().find(|c| !c.risk.dangerous) {
                proposal.rules.push(first.rule.clone());
            } else {
                proposal
                    .notes
                    .push("this path is outside the repo or protected: only a typed confirmation writes a rule".into());
            }
            if tool == "Edit" {
                proposal.notes.push(
                    "Claude Code saves no file-edit approval on its own (it lasts the session); a rule here makes it standing".into(),
                );
            }
        }
        Target::Fetch { url } => {
            let Some(host) = rule::url_host(url) else {
                proposal.notes.push("the URL has no host".into());
                return proposal;
            };
            proposal
                .candidates
                .extend(candidate(format!("WebFetch(domain:{host})"), "domain"));
            let labels: Vec<&str> = host.split('.').collect();
            if labels.len() >= 3 {
                proposal.candidates.extend(candidate(
                    format!("WebFetch(domain:*.{})", labels[1..].join(".")),
                    "subdomains",
                ));
            }
            if let Some(first) = proposal.candidates.first().filter(|c| !c.risk.dangerous) {
                proposal.rules.push(first.rule.clone());
            }
        }
        Target::Tool { name } => {
            proposal.candidates.extend(candidate(name.clone(), "tool"));
            if let Some(rest) = name.strip_prefix("mcp__")
                && let Some((server, _)) = rest.split_once("__")
            {
                proposal
                    .candidates
                    .extend(candidate(format!("mcp__{server}__*"), "server"));
            }
            if let Some(first) = proposal.candidates.first().filter(|c| !c.risk.dangerous) {
                proposal.rules.push(first.rule.clone());
            }
        }
    }
    proposal
}

// --- Codex ------------------------------------------------------------------------

/// A Codex `prefix_rule` the hub can offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexCandidate {
    /// The argv prefix it allows.
    pub pattern: Vec<String>,
    /// The `prefix_rule(...)` line as written.
    pub text: String,
    pub kind: &'static str,
    pub risk: Risk,
    pub sentence: String,
}

/// What the hub offers Codex for a request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CodexProposal {
    /// The rules "Always allow" writes (one per command of the line).
    pub rules: Vec<CodexCandidate>,
    pub candidates: Vec<CodexCandidate>,
    pub notes: Vec<String>,
}

/// A Starlark string literal (JSON escaping is a subset Starlark reads).
fn starlark(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// The rules-file line for `pattern`.
pub fn codex_rule_text(pattern: &[String]) -> String {
    let items: Vec<String> = pattern.iter().map(|p| starlark(p)).collect();
    format!(
        "prefix_rule(pattern = [{}], decision = \"allow\", justification = \"allowed from fw hub\")",
        items.join(", ")
    )
}

/// Check a pattern the hub may write (from the CLI or the API).
pub fn codex_candidate(
    pattern: Vec<String>,
    kind: &'static str,
    anchors: &Anchors,
    repo: &Path,
) -> Result<CodexCandidate, String> {
    if pattern.is_empty() || pattern.len() > 32 {
        return Err("a prefix rule names 1-32 words".into());
    }
    if pattern
        .iter()
        .any(|w| w.is_empty() || w.chars().any(char::is_control) || w.len() > 200)
    {
        return Err("a prefix rule's words are 1-200 characters, no control characters".into());
    }
    // A prefix rule allows anything after its words: judge it as a
    // trailing-wildcard Bash rule.
    let as_bash = Rule {
        tool: "Bash".into(),
        spec: Some(format!(
            "{} *",
            pattern
                .iter()
                .map(|w| crate::util::shell_quote(w))
                .collect::<Vec<_>>()
                .join(" ")
        )),
    };
    let risk = rule::risk(&as_bash, anchors, repo);
    let shown = pattern
        .iter()
        .map(|w| crate::util::shell_quote(w))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(CodexCandidate {
        text: codex_rule_text(&pattern),
        sentence: format!("run `{shown}` (with any further arguments)"),
        pattern,
        kind,
        risk,
    })
}

/// The `prefix_rule`s offered to Codex for `request`.
pub fn propose_codex(request: &Request, anchors: &Anchors, repo: &Path) -> CodexProposal {
    let mut proposal = CodexProposal::default();
    if let Target::Command { command } = &request.target
        && rule::pipes_to_shell(command)
    {
        proposal.notes.push(
            "this pipes into a shell or interpreter: approve it each time (no rule is safe)".into(),
        );
        return proposal;
    }
    let Target::Command { command } = &request.target else {
        proposal.notes.push(
            "Codex has no per-file allow rule: its sandbox_mode (workspace-write) decides which edits need no approval".into(),
        );
        return proposal;
    };
    let parts = match shell::split(command) {
        Ok(parts) => parts,
        Err(e) => {
            proposal
                .notes
                .push(format!("cannot read the command ({e}): approve it once"));
            return proposal;
        }
    };
    let mut needing = Vec::new();
    for part in &parts {
        let words = shell::effective_words(&part.words);
        if words.is_empty() {
            continue;
        }
        if part.substitution || part.heredoc {
            proposal.notes.push(format!(
                "`{}` runs code a rule cannot see (a substitution, subshell, or heredoc): approve it each time",
                part.raw
            ));
            return proposal;
        }
        if needs_no_rule(part, words) {
            continue;
        }
        needing.push(words.to_vec());
    }
    if needing.is_empty() || needing.len() > MAX_PART_RULES {
        proposal
            .notes
            .push("no command here to write a rule for".into());
        return proposal;
    }
    let single = needing.len() == 1;
    let mut all_safe = true;
    for words in &needing {
        let texts: Vec<String> = words.iter().map(|w| w.text.clone()).collect();
        let (len, has_sub) = scoped_len(words);
        let mut list: Vec<CodexCandidate> = Vec::new();
        let mut add = |pattern: Vec<String>, kind| {
            if list.iter().any(|c| c.pattern == pattern) {
                return;
            }
            if let Ok(c) = codex_candidate(pattern, kind, anchors, repo) {
                list.push(c);
            }
        };
        add(texts.clone(), "exact");
        if has_sub {
            add(texts[..len].to_vec(), "scoped");
        }
        add(texts[..1].to_vec(), "program");
        let default = list
            .iter()
            .position(|c| c.kind == "scoped" && !c.risk.dangerous)
            .or_else(|| list.first().filter(|c| !c.risk.dangerous).map(|_| 0));
        match default {
            Some(i) => proposal.rules.push(list[i].clone()),
            None => all_safe = false,
        }
        if single {
            proposal.candidates = list;
        } else if let Some(i) = default {
            proposal.candidates.push(list[i].clone());
        }
    }
    if !all_safe {
        proposal.rules.clear();
        proposal
            .notes
            .push("no rule narrow enough to offer: only a typed confirmation writes one".into());
    }
    proposal
}
