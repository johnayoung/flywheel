//! Claude Code permission rules (`Tool` or `Tool(specifier)`, claude
//! 2.1.306): parsing and the checks the hub makes before writing one, how
//! a rule matches a request, how dangerous it is, and the sentence that
//! says what it lets the agent do.

use super::shell::{self, Word};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// Longest rule the hub writes.
pub const MAX_RULE_CHARS: usize = 1000;

/// A parsed rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub tool: String,
    /// What is inside the parentheses (literal: no escaping).
    pub spec: Option<String>,
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.spec {
            Some(spec) => write!(f, "{}({spec})", self.tool),
            None => f.write_str(&self.tool),
        }
    }
}

/// Tools whose path rules never match (Claude Code checks `Edit` and
/// `Read` only) -- a rule for one is accepted by Claude and does nothing.
const PATH_RULES_NEVER_MATCH: &[&str] = &["Write", "NotebookEdit", "MultiEdit", "Glob"];

impl Rule {
    /// Parse and check an allow rule the hub may write. Refused: what
    /// Claude Code would skip or never match.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("an empty rule".into());
        }
        if raw.chars().count() > MAX_RULE_CHARS {
            return Err(format!("a rule is at most {MAX_RULE_CHARS} characters"));
        }
        if raw.chars().any(|c| c.is_control()) {
            return Err("a rule cannot hold control characters (a newline, a tab)".into());
        }
        let (tool, spec) = match raw.find('(') {
            Some(open) => {
                let Some(inner) = raw[open + 1..].strip_suffix(')') else {
                    return Err(format!("{raw:?}: a specifier ends with `)`"));
                };
                (&raw[..open], Some(inner.to_owned()))
            }
            None => (raw, None),
        };
        let tool = tool.trim();
        if tool.is_empty()
            || !tool
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '*'))
        {
            return Err(format!("{raw:?}: not a tool name"));
        }
        if tool.contains('*') {
            // Allow globs only count after a literal `mcp__<server>__`.
            let anchored = tool
                .strip_prefix("mcp__")
                .and_then(|rest| rest.split_once("__"))
                .is_some_and(|(server, _)| !server.is_empty() && !server.contains('*'));
            if !anchored {
                return Err(format!(
                    "{raw:?}: an allow rule's tool glob must start with mcp__<server>__ (Claude Code skips it otherwise)"
                ));
            }
        }
        if tool.starts_with("mcp__") && spec.is_some() {
            return Err(format!(
                "{raw:?}: MCP rules take no parentheses (Claude Code skips them)"
            ));
        }
        if let Some(spec) = &spec {
            if spec.trim().is_empty() {
                return Err(format!("{raw:?}: an empty specifier"));
            }
            if PATH_RULES_NEVER_MATCH.contains(&tool) {
                return Err(format!(
                    "{raw:?}: Claude Code never matches a {tool}(path) rule; use Edit({spec}) (or {tool} alone for every use)"
                ));
            }
            if tool == "WebFetch" && !spec.starts_with("domain:") {
                return Err(format!(
                    "{raw:?}: WebFetch rules name a domain: WebFetch(domain:host)"
                ));
            }
            if tool == "Bash" && param_form(spec) {
                return Err(format!(
                    "{raw:?}: allow rules use the command itself, not command:..., e.g. Bash(npm test *)"
                ));
            }
            if tool == "Bash" && compound(spec) {
                return Err(format!(
                    "{raw:?}: Claude Code matches each command of a line on its own (split at &&, ||, ;, |, &), so a rule holding one never matches; allow each command separately"
                ));
            }
        }
        Ok(Self {
            tool: tool.to_owned(),
            spec,
        })
    }

    /// The same rule as Claude Code reads it: `Bash(x:*)` is `Bash(x *)`,
    /// `Bash(*)` is `Bash`.
    pub fn canonical(&self) -> String {
        match (&self.tool[..], self.spec.as_deref()) {
            ("Bash", Some(spec)) => {
                let spec = bash_pattern(spec);
                if spec.trim() == "*" {
                    "Bash".to_owned()
                } else {
                    format!("Bash({spec})")
                }
            }
            _ => self.to_string(),
        }
    }
}

/// `Bash(command:...)`: a parameter rule (deny / ask only).
fn param_form(spec: &str) -> bool {
    spec.split_once(':')
        .is_some_and(|(name, _)| name.trim() == "command" || name.trim() == "run_in_background")
}

/// A Bash specifier holding more than one simple command.
fn compound(spec: &str) -> bool {
    shell::split(&bash_pattern(spec)).is_ok_and(|parts| parts.len() > 1)
}

/// A Bash specifier with a trailing `:*` written the way the dialog does
/// (` *`).
pub fn bash_pattern(spec: &str) -> String {
    match spec.strip_suffix(":*") {
        Some(head) => format!("{head} *"),
        None => spec.to_owned(),
    }
}

/// Whether two rules are the same rule (canonically).
pub fn same(a: &str, b: &str) -> bool {
    match (Rule::parse_any(a), Rule::parse_any(b)) {
        (Some(a), Some(b)) => a.canonical() == b.canonical(),
        _ => a.trim() == b.trim(),
    }
}

impl Rule {
    /// Parse any rule found in a settings file (deny and ask rules too),
    /// without the allow-rule checks.
    pub fn parse_any(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        match raw.find('(') {
            Some(open) => Some(Self {
                tool: raw[..open].trim().to_owned(),
                spec: Some(raw[open + 1..].strip_suffix(')')?.to_owned()),
            }),
            None if !raw.is_empty() => Some(Self {
                tool: raw.to_owned(),
                spec: None,
            }),
            None => None,
        }
    }
}

// --- matching ----------------------------------------------------------------

/// `*` matches any text (spaces too); anything else is literal.
fn glob(pattern: &[char], text: &[char]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some((sp, st)) = star {
            p = sp + 1;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// Whether Bash pattern `spec` matches one simple command (wrappers
/// already stripped), as Claude Code documents it: `*` is any text, and a
/// lone trailing ` *` also matches the bare command.
pub fn bash_matches(spec: &str, command: &str) -> bool {
    let pattern = bash_pattern(spec);
    let command = command.trim();
    let chars: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = command.chars().collect();
    if glob(&chars, &text) {
        return true;
    }
    if pattern.matches('*').count() == 1
        && let Some(head) = pattern.strip_suffix(" *")
    {
        return head == command;
    }
    false
}

/// `**` any path, `*` any text within one segment, `?` one character.
fn path_glob(pattern: &str, path: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some((b'*', rest)) if rest.first() == Some(&b'*') => {
                let rest = &rest[1..];
                // `**/` also matches no directory at all.
                if let Some(after) = rest.strip_prefix(&b"/"[..])
                    && go(after, t)
                {
                    return true;
                }
                (0..=t.len()).any(|i| go(rest, &t[i..]))
            }
            Some((b'*', rest)) => (0..=t.len())
                .take_while(|&i| i == 0 || t[i - 1] != b'/')
                .any(|i| go(rest, &t[i..])),
            Some((b'?', rest)) => t.first().is_some_and(|&c| c != b'/') && go(rest, &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    go(pattern.as_bytes(), path.as_bytes())
}

/// Where the rule's paths are anchored: the session's primary working
/// directory (`/path` in project and local settings), the settings file's
/// own anchor (`/path` in user settings means `~/.claude/path`), and home.
#[derive(Debug, Clone)]
pub struct Anchors {
    pub cwd: PathBuf,
    /// What a single leading `/` means in the file the rule is in.
    pub slash: PathBuf,
    pub home: PathBuf,
    /// Secret directories wherever they live, each with what it holds:
    /// the agents' config dirs and the hub's state dir (its token).
    pub secrets: Vec<(PathBuf, &'static str)>,
}

/// The absolute glob a path specifier stands for.
pub fn absolute_pattern(spec: &str, anchors: &Anchors) -> String {
    let join = |base: &Path, rest: &str| {
        let base = base.to_string_lossy();
        let base = base.trim_end_matches('/');
        if rest.is_empty() {
            base.to_owned()
        } else {
            format!("{base}/{rest}")
        }
    };
    if let Some(rest) = spec.strip_prefix("//") {
        format!("/{rest}")
    } else if let Some(rest) = spec.strip_prefix("~/") {
        join(&anchors.home, rest)
    } else if spec == "~" {
        anchors.home.to_string_lossy().into_owned()
    } else if let Some(rest) = spec.strip_prefix('/') {
        join(&anchors.slash, rest)
    } else if let Some(rest) = spec.strip_prefix("./") {
        join(&anchors.cwd, rest)
    } else if !spec.contains('/') {
        // A bare name matches at any depth (gitignore).
        join(&anchors.cwd, &format!("**/{spec}"))
    } else {
        join(&anchors.cwd, spec)
    }
}

/// The literal directory a path glob starts with (before its first glob
/// character).
fn literal_root(pattern: &str) -> PathBuf {
    let cut = pattern.find(['*', '?', '[']).unwrap_or(pattern.len());
    let head = &pattern[..cut];
    if cut == pattern.len() {
        return PathBuf::from(head);
    }
    match head.rfind('/') {
        Some(0) => PathBuf::from("/"),
        Some(i) => PathBuf::from(&head[..i]),
        None => PathBuf::from("/"),
    }
}

/// Lexically normalized (`.` and `..` resolved without touching disk).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// A pending request, as rules see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// A shell command line.
    Command { command: String },
    /// A file the agent edits or creates (absolute).
    Edit { path: PathBuf },
    /// A file or directory the agent reads (absolute).
    Read { path: PathBuf },
    /// A URL the agent fetches.
    Fetch { url: String },
    /// Any other tool (an MCP tool, WebSearch, ...), by name.
    Tool { name: String },
}

/// The host of a URL (lowercase, no trailing dot).
pub fn url_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    let host = parsed
        .host_str()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

fn domain_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
    if pattern == "*" {
        return true;
    }
    if let Some(parent) = pattern.strip_prefix("*.") {
        return host.len() > parent.len() + 1 && host.ends_with(&format!(".{parent}"));
    }
    let (p, h): (Vec<&str>, Vec<&str>) = (pattern.split('.').collect(), host.split('.').collect());
    p.len() == h.len() && p.iter().zip(&h).all(|(p, h)| *p == "*" || p == h)
}

/// Whether `rule` covers `target` (for Bash: every simple command of the
/// line it would need to approve; `None` when the line cannot be read).
pub fn covers(rule: &Rule, target: &Target, anchors: &Anchors) -> bool {
    match (rule.tool.as_str(), target) {
        ("Bash", Target::Command { command }) => match &rule.spec {
            None => true,
            Some(spec) => match shell::split(command) {
                Ok(parts) => {
                    !parts.is_empty()
                        && parts.iter().all(|part| {
                            let words = shell::effective_words(&part.words);
                            bash_matches(spec, &words_raw(words))
                        })
                }
                Err(_) => false,
            },
        },
        ("Bash", _) => false,
        ("Edit" | "Read", Target::Edit { path }) | ("Read", Target::Read { path }) => {
            if rule.tool == "Edit" && matches!(target, Target::Read { .. }) {
                return false;
            }
            match &rule.spec {
                None => true,
                Some(spec) => {
                    let pattern = absolute_pattern(spec, anchors);
                    let path = path.to_string_lossy();
                    path_glob(&pattern, &path)
                        || path_glob(&format!("{}/**", pattern.trim_end_matches('/')), &path)
                }
            }
        }
        ("WebFetch", Target::Fetch { url }) => match &rule.spec {
            None => true,
            Some(spec) => match (spec.strip_prefix("domain:"), url_host(url)) {
                (Some(pattern), Some(host)) => domain_matches(pattern, &host),
                _ => false,
            },
        },
        (tool, Target::Tool { name }) if rule.spec.is_none() => {
            tool == name
                || (tool.starts_with("mcp__")
                    && (glob(
                        &tool.chars().collect::<Vec<_>>(),
                        &name.chars().collect::<Vec<_>>(),
                    ) || name
                        .strip_prefix(tool)
                        .is_some_and(|rest| rest.starts_with("__"))))
        }
        _ => false,
    }
}

/// Whether everything `narrow` matches, `broad` matches too (read with
/// `narrow`'s `*` as text `broad` must cover): an allow rule that makes
/// another redundant, or a deny / ask rule that overrides an allow.
pub fn subsumes(broad: &Rule, narrow: &Rule, anchors: &Anchors) -> bool {
    if broad.tool != narrow.tool {
        // `mcp__github` covers `mcp__github__x` and `mcp__github__*`.
        return broad.spec.is_none()
            && narrow.spec.is_none()
            && broad.tool.starts_with("mcp__")
            && covers(
                broad,
                &Target::Tool {
                    name: narrow.tool.clone(),
                },
                anchors,
            );
    }
    if broad.canonical() == narrow.canonical() {
        return true;
    }
    let (Some(b), Some(n)) = (&broad.spec, &narrow.spec) else {
        return broad.spec.is_none() || broad.canonical() == "Bash";
    };
    match broad.tool.as_str() {
        "Bash" => bash_matches(b, &bash_pattern(n)),
        "Edit" | "Read" => {
            let (b, n) = (absolute_pattern(b, anchors), absolute_pattern(n, anchors));
            path_glob(&b, &n) || path_glob(&format!("{}/**", b.trim_end_matches('/')), &n)
        }
        "WebFetch" => match (b.strip_prefix("domain:"), n.strip_prefix("domain:")) {
            (Some(b), Some(n)) => {
                domain_matches(b, n.trim().trim_start_matches("*.")) && {
                    // `*.x.com` is covered by `*.x.com` (above) or a broader glob.
                    !n.trim().starts_with("*.") || b.trim().starts_with("*.") || b.trim() == "*"
                }
            }
            _ => false,
        },
        _ => false,
    }
}

/// Words as written, one space apart.
pub fn words_raw(words: &[Word]) -> String {
    words
        .iter()
        .map(|w| w.raw.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

// --- risk ----------------------------------------------------------------------

/// How a rule's breadth is judged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Risk {
    /// Never proposed by the hub; written only after the rule is typed.
    pub dangerous: bool,
    /// Why (empty when it is not dangerous).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

impl Risk {
    fn ok() -> Self {
        Self {
            dangerous: false,
            reason: String::new(),
        }
    }
    fn danger(reason: impl Into<String>) -> Self {
        Self {
            dangerous: true,
            reason: reason.into(),
        }
    }
}

/// How a dangerous command family counts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Any wildcard rule within the family (`git push origin x *` too).
    Family,
    /// Only a rule at least as broad as the entry (`python *`, not
    /// `python -m pytest *`).
    Covers,
}

/// Commands no wildcard rule may reach without a typed confirmation (and
/// `exact`: no exact rule either). The denylist: never auto-proposed.
const DANGEROUS: &[(&[&str], Reach, bool, &str)] = &[
    (&["sudo"], Reach::Family, true, "runs anything as root"),
    (
        &["su"],
        Reach::Family,
        true,
        "runs anything as another user",
    ),
    (&["doas"], Reach::Family, true, "runs anything as root"),
    (&["pkexec"], Reach::Family, true, "runs anything as root"),
    (&["rm"], Reach::Family, false, "deletes any file"),
    (&["shred"], Reach::Family, true, "destroys files"),
    (&["dd"], Reach::Family, true, "writes raw devices"),
    (&["mkfs"], Reach::Family, true, "formats disks"),
    (&["shutdown"], Reach::Family, true, "powers the machine off"),
    (&["reboot"], Reach::Family, true, "reboots the machine"),
    (
        &["curl"],
        Reach::Family,
        false,
        "downloads and uploads anything",
    ),
    (&["wget"], Reach::Family, false, "downloads anything"),
    (
        &["ssh"],
        Reach::Family,
        false,
        "runs commands on other machines",
    ),
    (
        &["scp"],
        Reach::Family,
        false,
        "copies files to other machines",
    ),
    (&["rsync"], Reach::Family, false, "copies files anywhere"),
    (&["nc"], Reach::Family, false, "opens network connections"),
    (&["ncat"], Reach::Family, false, "opens network connections"),
    (
        &["socat"],
        Reach::Family,
        false,
        "opens network connections",
    ),
    (
        &["chmod"],
        Reach::Family,
        false,
        "changes any file's permissions",
    ),
    (&["chown"], Reach::Family, false, "changes any file's owner"),
    (&["kill"], Reach::Family, false, "stops any process"),
    (&["pkill"], Reach::Family, false, "stops any process"),
    (&["killall"], Reach::Family, false, "stops any process"),
    (&["systemctl"], Reach::Family, false, "controls services"),
    (&["crontab"], Reach::Family, false, "schedules commands"),
    (&["eval"], Reach::Family, true, "runs any command"),
    (&["exec"], Reach::Family, true, "runs any command"),
    (&["env"], Reach::Family, false, "runs any command"),
    (&["xargs"], Reach::Family, false, "runs any command"),
    (&["sh"], Reach::Covers, false, "runs any shell code"),
    (&["bash"], Reach::Covers, false, "runs any shell code"),
    (&["zsh"], Reach::Covers, false, "runs any shell code"),
    (&["dash"], Reach::Covers, false, "runs any shell code"),
    (&["ksh"], Reach::Covers, false, "runs any shell code"),
    (&["fish"], Reach::Covers, false, "runs any shell code"),
    (&["sh", "-c"], Reach::Family, false, "runs any shell code"),
    (&["bash", "-c"], Reach::Family, false, "runs any shell code"),
    (&["zsh", "-c"], Reach::Family, false, "runs any shell code"),
    (&["python"], Reach::Covers, false, "runs any Python code"),
    (&["python3"], Reach::Covers, false, "runs any Python code"),
    (
        &["python", "-c"],
        Reach::Family,
        false,
        "runs any Python code",
    ),
    (
        &["python3", "-c"],
        Reach::Family,
        false,
        "runs any Python code",
    ),
    (&["node"], Reach::Covers, false, "runs any JavaScript"),
    (&["node", "-e"], Reach::Family, false, "runs any JavaScript"),
    (&["perl"], Reach::Covers, false, "runs any Perl code"),
    (&["perl", "-e"], Reach::Family, false, "runs any Perl code"),
    (&["ruby"], Reach::Covers, false, "runs any Ruby code"),
    (&["ruby", "-e"], Reach::Family, false, "runs any Ruby code"),
    (&["php"], Reach::Covers, false, "runs any PHP code"),
    (
        &["npx"],
        Reach::Covers,
        false,
        "downloads and runs any package",
    ),
    (
        &["bunx"],
        Reach::Covers,
        false,
        "downloads and runs any package",
    ),
    (
        &["pnpx"],
        Reach::Covers,
        false,
        "downloads and runs any package",
    ),
    (
        &["uvx"],
        Reach::Covers,
        false,
        "downloads and runs any package",
    ),
    (
        &["cp"],
        Reach::Family,
        false,
        "can overwrite any file, the agent's own settings too",
    ),
    (
        &["mv"],
        Reach::Family,
        false,
        "can overwrite any file, the agent's own settings too",
    ),
    (
        &["tee"],
        Reach::Family,
        false,
        "can overwrite any file, the agent's own settings too",
    ),
    (
        &["ln"],
        Reach::Family,
        false,
        "can replace any file with a link",
    ),
    (
        &["install"],
        Reach::Family,
        false,
        "can overwrite any file, the agent's own settings too",
    ),
    (&["truncate"], Reach::Family, false, "can empty any file"),
    (
        &["sed"],
        Reach::Family,
        false,
        "edits any file in place (-i) and can run commands",
    ),
    (
        &["awk"],
        Reach::Family,
        false,
        "can run any command (system())",
    ),
    (
        &["gawk"],
        Reach::Family,
        false,
        "can run any command (system())",
    ),
    (
        &["mawk"],
        Reach::Family,
        false,
        "can run any command (system())",
    ),
    (
        &["git", "push"],
        Reach::Family,
        false,
        "pushes, force-pushes, and deletes remote branches",
    ),
    (
        &["git", "reset", "--hard"],
        Reach::Family,
        true,
        "throws away uncommitted work",
    ),
    (
        &["git", "clean"],
        Reach::Family,
        true,
        "deletes untracked files",
    ),
    (
        &["git", "checkout", "--"],
        Reach::Family,
        true,
        "throws away uncommitted work",
    ),
    (
        &["git", "restore"],
        Reach::Family,
        false,
        "throws away uncommitted work",
    ),
    (
        &["git", "filter-branch"],
        Reach::Family,
        true,
        "rewrites history",
    ),
    (
        &["git", "config"],
        Reach::Family,
        false,
        "can make git run any program",
    ),
    (
        &["git", "-c"],
        Reach::Family,
        true,
        "can make git run any program",
    ),
    (
        &["git", "update-ref"],
        Reach::Family,
        false,
        "moves any branch",
    ),
    (
        &["git", "branch", "-D"],
        Reach::Family,
        true,
        "deletes branches",
    ),
    (
        &["gh", "api"],
        Reach::Family,
        false,
        "calls any GitHub API with your credentials",
    ),
    (
        &["gh", "pr", "merge"],
        Reach::Family,
        false,
        "merges pull requests",
    ),
    (
        &["gh", "release", "create"],
        Reach::Family,
        false,
        "publishes releases",
    ),
    (
        &["gh", "workflow", "run"],
        Reach::Family,
        false,
        "starts CI workflows with your credentials",
    ),
    (
        &["gh", "repo", "delete"],
        Reach::Family,
        true,
        "deletes repositories",
    ),
    (
        &["gh", "release", "delete"],
        Reach::Family,
        true,
        "deletes releases",
    ),
    (
        &["docker", "run"],
        Reach::Family,
        false,
        "runs any container (with any host mount)",
    ),
    (
        &["docker", "exec"],
        Reach::Family,
        false,
        "runs anything in a container",
    ),
    (
        &["npm", "publish"],
        Reach::Family,
        true,
        "publishes a package",
    ),
    (
        &["cargo", "publish"],
        Reach::Family,
        true,
        "publishes a crate",
    ),
    (
        &["pnpm", "publish"],
        Reach::Family,
        true,
        "publishes a package",
    ),
    (
        &["yarn", "publish"],
        Reach::Family,
        true,
        "publishes a package",
    ),
    (
        &["twine", "upload"],
        Reach::Family,
        true,
        "publishes a package",
    ),
    // The hub's own CLI: starts any process, types into other sessions,
    // prints the hub's token, and edits these very rules.
    (&["fw"], Reach::Family, true, HUB_CLI),
    (&["flywheel"], Reach::Family, true, HUB_CLI),
    // Programs and subcommands that run any command they are given.
    (&["find"], Reach::Family, false, FIND_WHY),
    (
        &["git", "submodule", "foreach"],
        Reach::Family,
        true,
        "runs any command in each submodule",
    ),
    (
        &["git", "rebase"],
        Reach::Family,
        false,
        "runs any command (-x/--exec) and rewrites history",
    ),
    (
        &["git", "bisect", "run"],
        Reach::Family,
        true,
        "runs any command",
    ),
    (
        &["npm", "exec"],
        Reach::Family,
        false,
        "downloads and runs any package",
    ),
    (
        &["npm", "x"],
        Reach::Family,
        false,
        "downloads and runs any package",
    ),
    (
        &["pnpm", "dlx"],
        Reach::Family,
        false,
        "downloads and runs any package",
    ),
    (&["pnpm", "exec"], Reach::Family, false, "runs any command"),
    (
        &["yarn", "dlx"],
        Reach::Family,
        false,
        "downloads and runs any package",
    ),
    (&["yarn", "exec"], Reach::Family, false, "runs any command"),
    (&["bun"], Reach::Covers, false, "runs any JavaScript"),
    (
        &["bun", "x"],
        Reach::Family,
        false,
        "downloads and runs any package",
    ),
    (&["bun", "-e"], Reach::Family, false, "runs any JavaScript"),
    (&["deno"], Reach::Covers, false, "runs any JavaScript"),
    (
        &["deno", "eval"],
        Reach::Family,
        false,
        "runs any JavaScript",
    ),
    (&["pypy"], Reach::Covers, false, "runs any Python code"),
    (
        &["docker", "container", "run"],
        Reach::Family,
        false,
        "runs any container (with any host mount)",
    ),
    (
        &["docker", "container", "exec"],
        Reach::Family,
        false,
        "runs anything in a container",
    ),
    (
        &["podman", "run"],
        Reach::Family,
        false,
        "runs any container (with any host mount)",
    ),
    (
        &["podman", "exec"],
        Reach::Family,
        false,
        "runs anything in a container",
    ),
    (&["busybox"], Reach::Family, false, "runs any command"),
    (&["script"], Reach::Family, false, "runs any command"),
    (&["parallel"], Reach::Family, false, "runs any command"),
    (
        &["tar"],
        Reach::Family,
        false,
        "can run commands (--to-command) and overwrite any file",
    ),
    (
        &["vi"],
        Reach::Family,
        false,
        "edits any file and runs commands",
    ),
    (
        &["vim"],
        Reach::Family,
        false,
        "edits any file and runs commands",
    ),
    (
        &["nvim"],
        Reach::Family,
        false,
        "edits any file and runs commands",
    ),
    (
        &["emacs"],
        Reach::Family,
        false,
        "edits any file and runs commands",
    ),
];

/// Why the hub's own CLI is never auto-allowed.
const HUB_CLI: &str =
    "controls the hub: starts any process, types into other sessions, prints the hub's token";
/// Why a `find` wildcard is never auto-allowed.
const FIND_WHY: &str = "deletes, writes, or runs commands on what it finds";

/// Interpreters whose versioned names (`python3.12`, `perl5.36`) run the
/// same code: judged by their base name.
const VERSIONED: &[&str] = &["python", "pypy", "perl", "ruby", "node", "php"];

/// A program word judged by name: `/usr/bin/rm` and `./rm` are rm,
/// `python3.12` is python.
fn program_name(word: &str) -> String {
    let name = word.rsplit('/').next().unwrap_or(word);
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    if base != name && VERSIONED.contains(&base) {
        return base.to_owned();
    }
    name.to_owned()
}

/// This hub's own executable name (`fw`, `flywheel`, or however it was
/// installed): judged like the hub CLI.
fn hub_exe_name() -> Option<&'static str> {
    static NAME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    })
    .as_deref()
}

/// `git push` flags that rewrite or delete what others have.
const FORCE_FLAGS: &[&str] = &[
    "--force",
    "-f",
    "--force-with-lease",
    "--delete",
    "-d",
    "--mirror",
    "--prune",
];

/// A rule's literal words (before its first `*`): `(texts, whether the
/// last one is cut short by the wildcard, whether there is a wildcard)`.
fn literal_words(spec: &str) -> Option<(Vec<String>, bool, bool)> {
    let pattern = bash_pattern(spec);
    let wildcard = pattern.contains('*');
    let literal = match pattern.find('*') {
        Some(i) => &pattern[..i],
        None => &pattern[..],
    };
    let partial = wildcard && !literal.is_empty() && !literal.ends_with(char::is_whitespace);
    let parts = shell::split(literal).ok()?;
    let part = parts.into_iter().next()?;
    let mut words: Vec<Word> = part.words;
    // Leading assignments and wrappers: Claude Code strips them from the
    // command before matching; judge what runs.
    let skip = words.iter().take_while(|w| shell::is_assignment(w)).count();
    words.drain(..skip);
    // Only wrappers before the wildcard (`timeout 5 *`, `nohup *`): the
    // wildcard stands where the program goes.
    if wildcard && shell::only_wrappers(&words, partial) {
        return Some((Vec::new(), false, wildcard));
    }
    let wrappers = shell::wrapper_len(&words);
    words.drain(..wrappers);
    Some((
        words.into_iter().map(|w| w.text).collect(),
        partial,
        wildcard,
    ))
}

/// Pipes into an interpreter: `curl ... | sh`.
pub fn pipes_to_shell(spec: &str) -> bool {
    let Ok(parts) = shell::split(spec) else {
        return false;
    };
    parts.iter().skip(1).any(|part| {
        let words = shell::effective_words(&part.words);
        let words = match words.first().map(|w| w.text.as_str()) {
            Some("sudo") => &words[1..],
            _ => words,
        };
        words.first().is_some_and(|w| {
            let name = w.text.rsplit('/').next().unwrap_or(&w.text);
            matches!(
                name,
                "sh" | "bash"
                    | "zsh"
                    | "dash"
                    | "ksh"
                    | "fish"
                    | "python"
                    | "python3"
                    | "perl"
                    | "ruby"
                    | "node"
            )
        })
    })
}

/// git's global options that take the next word as their value.
const GIT_VALUE_OPTIONS: &[&str] = &[
    "-C",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--super-prefix",
];
/// git's global options that take no value.
const GIT_FLAG_OPTIONS: &[&str] = &[
    "-p",
    "-P",
    "--paginate",
    "--no-pager",
    "--bare",
    "--no-replace-objects",
    "--no-lazy-fetch",
    "--no-optional-locks",
    "--no-advice",
    "--literal-pathspecs",
    "--glob-pathspecs",
    "--noglob-pathspecs",
    "--icase-pathspecs",
];

/// Strip git's global options (`git -C dir push` runs `git push`), so the
/// subcommand is judged. A wildcard inside them reaches every subcommand:
/// the words become just `git`. `Err` for an option that can make git run
/// any program, or one the hub does not know.
fn strip_git_globals(words: &mut Vec<String>, partial: &mut bool) -> Result<(), Risk> {
    if words.first().map(String::as_str) != Some("git") {
        return Ok(());
    }
    let run_anything = || {
        Risk::danger(
            "sets git config or helpers on the command line, which can make git run any program",
        )
    };
    // Each option found is removed, so the next word is again at 1.
    const I: usize = 1;
    while words.len() > I {
        let i = I;
        let w = words[i].as_str();
        let last_partial = *partial && i == words.len() - 1;
        if last_partial && (w.starts_with('-') || w.is_empty()) {
            // `git -C *`, `git --no-p*`: any option, then any subcommand.
            words.truncate(1);
            *partial = false;
            return Ok(());
        }
        if !w.starts_with('-') {
            break;
        }
        if w == "-c" || w.starts_with("--config-env") || w.starts_with("--exec-path") {
            return Err(run_anything());
        }
        let name = w.split_once('=').map_or(w, |(name, _)| name);
        if GIT_VALUE_OPTIONS.contains(&name) && !w.contains('=') {
            if i + 1 >= words.len() || (*partial && i + 1 == words.len() - 1) {
                // The value is cut by the wildcard: what follows is anything.
                words.truncate(1);
                *partial = false;
                return Ok(());
            }
            words.drain(i..i + 2);
        } else if GIT_VALUE_OPTIONS.contains(&name) || GIT_FLAG_OPTIONS.contains(&w) {
            words.remove(i);
        } else {
            return Err(Risk::danger(format!(
                "`{w}` before git's subcommand is an option the hub cannot judge"
            )));
        }
    }
    Ok(())
}

/// The path components a word names (`--out=.claude/x` names `.claude/x`).
fn word_paths(word: &str) -> impl Iterator<Item = &str> {
    let value = word.split_once('=').map(|(_, v)| v);
    std::iter::once(word).chain(value)
}

/// What a command argument names that the agent must not be let change
/// without a typed confirmation: `.claude/`, `.git/`, ... (any program),
/// or (for a program that writes files) a path outside the repo.
fn argument_risk(words: &[String], anchors: &Anchors, repo: &Path) -> Option<Risk> {
    let program = words.first()?.as_str();
    let writes = matches!(
        program,
        "cp" | "mv"
            | "tee"
            | "ln"
            | "install"
            | "truncate"
            | "rsync"
            | "rm"
            | "rmdir"
            | "mkdir"
            | "unzip"
            | "tar"
            | "patch"
            | "dd"
            | "shred"
    );
    for word in &words[1..] {
        for text in word_paths(word) {
            if let Some(p) = PROTECTED.iter().find(|p| text.split('/').any(|c| c == **p)) {
                return Some(Risk::danger(format!(
                    "names {p} (the agent could change {})",
                    protected_what(p)
                )));
            }
            if !writes || text.starts_with('-') || text.is_empty() {
                continue;
            }
            let path = if let Some(rest) = text.strip_prefix("~/") {
                anchors.home.join(rest)
            } else if text == "~" {
                anchors.home.clone()
            } else if text.starts_with('/') {
                PathBuf::from(text)
            } else {
                anchors.cwd.join(text)
            };
            let path = normalize(&path);
            if !path.starts_with(repo) && path != Path::new("/dev/null") {
                return Some(Risk::danger(format!(
                    "{program} touches {}, outside the repo ({})",
                    path.display(),
                    repo.display()
                )));
            }
        }
    }
    None
}

/// `find` actions that delete, write, or run commands.
const FIND_ACTIONS: &[&str] = &[
    "-delete",
    "-exec",
    "-execdir",
    "-ok",
    "-okdir",
    "-fprint",
    "-fprint0",
    "-fprintf",
    "-fls",
    "-files0-from",
];

fn bash_risk(spec: Option<&str>, anchors: &Anchors, repo: &Path) -> Risk {
    let Some(spec) = spec else {
        return Risk::danger("lets the agent run every shell command");
    };
    let pattern = bash_pattern(spec);
    if pattern.trim() == "*" {
        return Risk::danger("lets the agent run every shell command");
    }
    if pattern.contains("$(") || pattern.contains('`') {
        return Risk::danger("holds a command substitution");
    }
    if pipes_to_shell(&pattern) {
        return Risk::danger("pipes into a shell or interpreter");
    }
    if compound(&pattern) {
        return Risk::danger("holds more than one command");
    }
    let Some((words, mut partial, wildcard)) = literal_words(&pattern) else {
        return Risk::danger("cannot be read as a command");
    };
    if words.is_empty() {
        return Risk::danger("the wildcard stands where the program goes: it matches any program");
    }
    // The program by name (`/usr/bin/rm` is rm, `python3.12` is python).
    let mut words = words;
    if let Some(first) = words.first_mut() {
        *first = program_name(first);
    }
    let hub_exe = hub_exe_name().is_some_and(|exe| {
        if partial && words.len() == 1 {
            exe.starts_with(words[0].as_str())
        } else {
            words[0] == exe
        }
    });
    if hub_exe {
        return Risk::danger(format!("runs this hub's own CLI, which {HUB_CLI}"));
    }
    if let Err(risk) = strip_git_globals(&mut words, &mut partial) {
        return risk;
    }
    let mkfs = |w: &str| w.starts_with("mkfs");
    for (entry, reach, exact_too, why) in DANGEROUS {
        let n = words.len().min(entry.len());
        let compatible = (0..n).all(|i| {
            let (rule, bad) = (words[i].as_str(), entry[i]);
            let last_partial = partial && i == words.len() - 1;
            if last_partial {
                bad.starts_with(rule)
            } else {
                rule == bad || (bad == "mkfs" && mkfs(rule))
            }
        });
        if !compatible {
            continue;
        }
        // An interpreter given only options before the wildcard
        // (`python -u *`, `perl -i *`) still runs any code.
        let as_broad =
            words.len() <= entry.len() || words[entry.len()..].iter().all(|w| w.starts_with('-'));
        let hit = if wildcard {
            match reach {
                Reach::Family => true,
                Reach::Covers => as_broad,
            }
        } else {
            *exact_too && words.len() >= entry.len()
        };
        if hit {
            let verb = if wildcard { "covers" } else { "runs" };
            return Risk::danger(format!("{verb} `{}`, which {why}", entry.join(" ")));
        }
    }
    if let Some(risk) = argument_risk(&words, anchors, repo) {
        return risk;
    }
    // Exact forms that are dangerous by their arguments.
    if !wildcard {
        match words.first().map(String::as_str) {
            Some("rm")
                if words[1..].iter().any(|w| {
                    w == "--recursive"
                        || (w.starts_with('-')
                            && !w.starts_with("--")
                            && (w.contains('r') || w.contains('R')))
                }) =>
            {
                return Risk::danger("deletes directories recursively");
            }
            Some("find")
                if words[1..]
                    .iter()
                    .any(|w| FIND_ACTIONS.contains(&w.as_str())) =>
            {
                return Risk::danger(FIND_WHY);
            }
            Some("git")
                if words.get(1).map(String::as_str) == Some("rebase")
                    && words[2..].iter().any(|w| {
                        w == "-x"
                            || w.starts_with("--exec")
                            || (w.starts_with("-x") && !w.starts_with("--"))
                    }) =>
            {
                return Risk::danger("runs any command (-x/--exec)");
            }
            Some("sed")
                if words[1..]
                    .iter()
                    .any(|w| w.starts_with("-i") || w.starts_with("--in-place")) =>
            {
                return Risk::danger("edits files in place");
            }
            Some("git")
                if words.get(1).map(String::as_str) == Some("push")
                    && words[2..].iter().any(|w| {
                        FORCE_FLAGS.contains(&w.as_str())
                            || w.starts_with('+')
                            || w.starts_with("--force")
                            // `git push origin :main` deletes main.
                            || w.starts_with(':')
                            || (w.starts_with('-')
                                && !w.starts_with("--")
                                && (w.contains('f') || w.contains('d')))
                    }) =>
            {
                return Risk::danger("force-pushes or deletes remote branches");
            }
            _ => {}
        }
    }
    Risk::ok()
}

/// What the agent must never be let edit without a typed confirmation:
/// its own permission and hook settings, git internals, Codex's config.
const PROTECTED: &[&str] = &[".claude", ".git", ".codex", ".mcp.json", ".gitmodules"];

/// Sensitive directories under home a read rule must not cover.
const SECRET_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".config",
    ".claude",
    ".codex",
    ".docker",
    ".kube",
    ".netrc",
    ".git-credentials",
    ".local/state",
    ".npmrc",
    ".pypirc",
    ".pgpass",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".gem/credentials",
    ".terraform.d",
    ".bash_history",
    ".zsh_history",
];

fn protected_what(p: &str) -> &'static str {
    match p {
        ".claude" => "its own permissions and hooks",
        ".codex" => "Codex's config and rules",
        ".mcp.json" => "which MCP servers it loads",
        _ => "git's internals and hooks",
    }
}

fn path_risk(rule: &Rule, anchors: &Anchors, repo: &Path) -> Risk {
    let tool = rule.tool.as_str();
    let Some(spec) = &rule.spec else {
        return Risk::danger(match tool {
            "Read" => "lets the agent read every file on this machine",
            _ => "lets the agent change every file on this machine",
        });
    };
    let pattern = absolute_pattern(spec, anchors);
    let root = normalize(&literal_root(&pattern));
    let globbed = pattern.contains(['*', '?', '[']);
    let home = &anchors.home;
    if root == Path::new("/") || (globbed && home.starts_with(&root)) || root == *home {
        return Risk::danger(format!(
            "covers {}, every file under it",
            if root == Path::new("/") {
                "/".to_owned()
            } else {
                root.display().to_string()
            }
        ));
    }
    if tool == "Read" {
        let hits = |dir: &PathBuf| root.starts_with(dir) || (globbed && dir.starts_with(&root));
        if let Some(dir) = SECRET_DIRS.iter().map(|d| home.join(d)).find(hits) {
            return Risk::danger(format!(
                "covers credentials or settings under your home ({})",
                dir.display()
            ));
        }
        // The agents' config dirs and the hub's state dir, wherever they
        // are (not necessarily under home).
        let covered: Vec<String> = anchors
            .secrets
            .iter()
            .filter(|(d, _)| hits(d))
            .map(|(d, what)| format!("{} ({what})", d.display()))
            .collect();
        if !covered.is_empty() {
            return Risk::danger(format!("covers {}", covered.join(" and ")));
        }
        return Risk::ok();
    }
    if !root.starts_with(repo) {
        return Risk::danger(format!("edits outside the repo ({})", repo.display()));
    }
    let touches = PROTECTED.iter().find(|p| {
        let dir = repo.join(p);
        root.starts_with(&dir) || (globbed && dir.starts_with(&root))
    });
    if let Some(p) = touches {
        return Risk::danger(format!(
            "covers {p} (the agent could change {})",
            protected_what(p)
        ));
    }
    Risk::ok()
}

/// How dangerous `rule` is for a session in `anchors.cwd` of `repo`.
pub fn risk(rule: &Rule, anchors: &Anchors, repo: &Path) -> Risk {
    match rule.tool.as_str() {
        "Bash" => bash_risk(rule.spec.as_deref(), anchors, repo),
        "Edit" | "Read" => path_risk(rule, anchors, repo),
        "Write" | "NotebookEdit" | "MultiEdit" => {
            Risk::danger("lets the agent write every file on this machine")
        }
        "WebFetch" => match rule.spec.as_deref().and_then(|s| s.strip_prefix("domain:")) {
            None => Risk::danger("lets the agent fetch any URL"),
            Some(domain) => {
                let domain = domain.trim();
                let parent = domain.strip_prefix("*.").unwrap_or(domain);
                if domain == "*" || !parent.contains('.') || parent.split('.').any(|l| l == "*") {
                    Risk::danger("matches nearly every domain")
                } else {
                    Risk::ok()
                }
            }
        },
        tool if tool.starts_with("mcp__") => {
            let rest = &tool[5..];
            match rest.split_once("__") {
                Some((_, name)) if !name.is_empty() && name != "*" => Risk::ok(),
                _ => Risk::danger(format!(
                    "lets the agent use every tool of the {} MCP server, including any it adds later",
                    rest.split("__").next().unwrap_or(rest)
                )),
            }
        }
        _ => Risk::ok(),
    }
}

// --- the sentence ----------------------------------------------------------------

/// "This lets the agent ... without asking": what `rule` permits.
pub fn describe(rule: &Rule, anchors: &Anchors) -> String {
    let spec = rule.spec.as_deref();
    match rule.tool.as_str() {
        "Bash" => match spec.map(bash_pattern) {
            None => "run any shell command".to_owned(),
            Some(p) if p.trim() == "*" => "run any shell command".to_owned(),
            Some(p) => {
                let stars = p.matches('*').count();
                match p.strip_suffix(" *") {
                    Some(head) if stars == 1 => {
                        format!("run `{head}` (with any further arguments)")
                    }
                    _ if stars == 0 => format!("run exactly `{p}`"),
                    _ => format!("run any command matching `{p}` (each * is any text)"),
                }
            }
        },
        "Edit" => match spec {
            None => "edit or create any file".to_owned(),
            Some(s) => {
                let pattern = absolute_pattern(s, anchors);
                if pattern.contains(['*', '?']) {
                    format!("edit or create files matching `{pattern}`")
                } else {
                    format!("edit or create `{pattern}`")
                }
            }
        },
        "Read" => match spec {
            None => "read any file".to_owned(),
            Some(s) => format!("read `{}`", absolute_pattern(s, anchors)),
        },
        "WebFetch" => match spec.and_then(|s| s.strip_prefix("domain:")) {
            Some(d) => format!("fetch pages from {d}"),
            None => "fetch any URL".to_owned(),
        },
        "WebSearch" => "search the web".to_owned(),
        tool if tool.starts_with("mcp__") => {
            let rest = &tool[5..];
            match rest.split_once("__") {
                Some((server, "*")) | Some((server, "")) => {
                    format!("use every tool of the {server} MCP server")
                }
                Some((server, name)) => format!("use the {server} MCP tool {name}"),
                None => format!("use every tool of the {rest} MCP server"),
            }
        }
        tool => match spec {
            None => format!("use {tool} for anything"),
            Some(s) => format!("use {tool} for `{s}`"),
        },
    }
}
