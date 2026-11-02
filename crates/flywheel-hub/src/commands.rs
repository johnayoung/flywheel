//! Suggested commands: the fenced code blocks of an agent's replies, with
//! their exact text, classified as shell commands or not.
//!
//! Agents render suggested commands with margins and wrapping, so copying
//! one out of the terminal mangles it. The hub reads the same text from
//! the agent's transcript instead (see [`crate::transcript`]); this module
//! is the pure part: Markdown fences in, blocks out.

use serde::{Deserialize, Serialize};

/// Largest block kept; a bigger one is not a command anyone pastes.
pub const MAX_BLOCK_BYTES: usize = 256 * 1024;

/// One fenced code block of a Markdown text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fenced {
    /// The info string's first word (` ```bash title=x ` -> `bash`),
    /// lowercased; `None` when untagged.
    pub lang: Option<String>,
    /// The block's content exactly as written, minus the fence lines and
    /// the fence's own indentation (a block nested in a list item); no
    /// trailing newline.
    pub text: String,
    /// Whether a closing fence was found (an unclosed fence runs to the
    /// end of the text, as in CommonMark).
    pub closed: bool,
}

/// What a fence line opens: its character, length, and indentation.
#[derive(Debug, Clone, Copy)]
struct Fence {
    ch: u8,
    len: usize,
    indent: usize,
}

/// A line that opens a fence: optional spaces, then 3+ backticks or
/// tildes; a backtick fence's info string has no backticks (so ```` ```x``` ````
/// on one line is inline code, not a fence).
fn opening_fence(line: &str) -> Option<(Fence, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let ch = *rest.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let len = rest.bytes().take_while(|&b| b == ch).count();
    if len < 3 {
        return None;
    }
    let info = &rest[len..];
    if ch == b'`' && info.contains('`') {
        return None;
    }
    Some((Fence { ch, len, indent }, info.trim()))
}

/// Whether `line` closes `fence`: the same character, at least as long,
/// nothing after it but whitespace.
fn closes(line: &str, fence: Fence) -> bool {
    let rest = line.trim_start_matches(' ');
    let len = rest.bytes().take_while(|&b| b == fence.ch).count();
    len >= fence.len && rest[len..].trim().is_empty()
}

/// Remove up to `n` leading spaces.
fn dedent(line: &str, n: usize) -> &str {
    let spaces = line.len() - line.trim_start_matches(' ').len();
    &line[spaces.min(n)..]
}

/// Every fenced code block of `markdown`, in order.
pub fn fenced_blocks(markdown: &str) -> Vec<Fenced> {
    let mut blocks = Vec::new();
    let mut open: Option<(Fence, Option<String>, Vec<&str>)> = None;
    for raw in markdown.split('\n') {
        // A CRLF text keeps its `\r` in content lines; fences ignore it.
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        match &mut open {
            None => {
                if let Some((fence, info)) = opening_fence(line) {
                    let lang = info
                        .split_whitespace()
                        .next()
                        .map(|w| w.trim_matches(|c| c == '{' || c == '}' || c == '.'))
                        .filter(|w| !w.is_empty())
                        .map(str::to_ascii_lowercase);
                    open = Some((fence, lang, Vec::new()));
                }
            }
            Some((fence, lang, lines)) => {
                if closes(line, *fence) {
                    blocks.push(Fenced {
                        lang: lang.take(),
                        text: lines.join("\n"),
                        closed: true,
                    });
                    open = None;
                } else {
                    lines.push(dedent(raw, fence.indent));
                }
            }
        }
    }
    if let Some((_, lang, mut lines)) = open {
        // The text's own final newline is not part of the block.
        if lines.last() == Some(&"") {
            lines.pop();
        }
        // An unclosed fence at the very end with nothing in it is the
        // start of a block still being written, not a block.
        if !lines.iter().all(|l| l.trim().is_empty()) {
            blocks.push(Fenced {
                lang,
                text: lines.join("\n"),
                closed: false,
            });
        }
    }
    blocks
}

/// Info-string languages that are shell.
const SHELL_LANGS: &[&str] = &[
    "bash",
    "sh",
    "zsh",
    "shell",
    "console",
    "shell-session",
    "shellsession",
    "sh-session",
    "fish",
    "ksh",
];

/// Shell-session languages: commands behind `$ ` prompts, maybe output.
const CONSOLE_LANGS: &[&str] = &["console", "shell-session", "shellsession", "sh-session"];

/// First words that mark an untagged block as a command.
const COMMAND_WORDS: &[&str] = &[
    "./gradlew",
    "[",
    "alias",
    "apt",
    "apt-get",
    "asdf",
    "awk",
    "aws",
    "az",
    "base64",
    "basename",
    "bash",
    "bat",
    "brew",
    "bun",
    "bundle",
    "bunx",
    "cargo",
    "cat",
    "cd",
    "chgrp",
    "chmod",
    "chown",
    "clang",
    "claude",
    "cmake",
    "codex",
    "composer",
    "conda",
    "cp",
    "createdb",
    "crontab",
    "curl",
    "cut",
    "deno",
    "df",
    "diff",
    "dig",
    "dirname",
    "dnf",
    "docker",
    "docker-compose",
    "doctl",
    "dotnet",
    "dropdb",
    "du",
    "echo",
    "env",
    "eslint",
    "eval",
    "exec",
    "export",
    "fd",
    "ffmpeg",
    "find",
    "fish",
    "flyctl",
    "flywheel",
    "for",
    "fw",
    "gawk",
    "gcc",
    "gcloud",
    "gem",
    "gh",
    "git",
    "go",
    "gradle",
    "grep",
    "gunzip",
    "gzip",
    "head",
    "helm",
    "heroku",
    "java",
    "javac",
    "journalctl",
    "jq",
    "kill",
    "killall",
    "kubectl",
    "launchctl",
    "less",
    "ln",
    "ls",
    "lsof",
    "make",
    "md5sum",
    "meson",
    "minikube",
    "mise",
    "mkdir",
    "mktemp",
    "mongosh",
    "mount",
    "mv",
    "mvn",
    "mypy",
    "mysql",
    "nano",
    "nc",
    "netstat",
    "ninja",
    "nix",
    "node",
    "nohup",
    "npm",
    "npx",
    "nvm",
    "ollama",
    "openssl",
    "pacman",
    "pbcopy",
    "pbpaste",
    "perl",
    "pg_dump",
    "pgrep",
    "php",
    "ping",
    "pip",
    "pip3",
    "pipx",
    "pkill",
    "pnpm",
    "podman",
    "poetry",
    "prettier",
    "printf",
    "ps",
    "psql",
    "pwd",
    "pyenv",
    "pytest",
    "python",
    "python3",
    "rails",
    "rake",
    "readlink",
    "realpath",
    "redis-cli",
    "rg",
    "rm",
    "rsync",
    "ruby",
    "ruff",
    "rustc",
    "rustup",
    "scp",
    "sed",
    "set",
    "sh",
    "sha256sum",
    "sleep",
    "sort",
    "source",
    "sqlite3",
    "ssh",
    "stat",
    "sudo",
    "supabase",
    "swift",
    "systemctl",
    "tac",
    "tail",
    "tailscale",
    "tar",
    "tee",
    "terraform",
    "test",
    "tmux",
    "touch",
    "tox",
    "tr",
    "tree",
    "tsc",
    "uname",
    "uniq",
    "unset",
    "unzip",
    "uv",
    "uvx",
    "vercel",
    "vim",
    "wc",
    "wget",
    "which",
    "while",
    "whoami",
    "wl-copy",
    "wrangler",
    "xargs",
    "xclip",
    "xcodebuild",
    "xz",
    "yarn",
    "yq",
    "zip",
    "zsh",
];

/// Whether a block of language `lang` is a shell command block: a shell
/// language, or untagged and starting like a command (its first line that
/// is not a comment begins with a known command, a path to run, an
/// environment assignment before one, or a `$ ` prompt).
pub fn is_shell(lang: Option<&str>, text: &str) -> bool {
    match lang {
        Some(lang) => SHELL_LANGS.contains(&lang),
        None => looks_like_command(text),
    }
}

fn looks_like_command(text: &str) -> bool {
    let Some(first) = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
    else {
        return false;
    };
    let first = first.strip_prefix("$ ").unwrap_or(first);
    let mut words = first.split_whitespace();
    let mut word = words.next().unwrap_or_default();
    // `FOO=bar BAZ=1 cmd ...`
    while is_assignment(word) {
        match words.next() {
            Some(next) => word = next,
            // A bare assignment line is shell too.
            None => return true,
        }
    }
    COMMAND_WORDS.contains(&word)
        || word.starts_with("./")
        || word.starts_with("~/")
        || (word.starts_with('/') && word.len() > 1 && !word.starts_with("//"))
        || word.ends_with(".sh")
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The text to copy or paste for a block: `text` itself, except a
/// console-style block (a console language, or an untagged block whose
/// first line carries a `$ ` prompt) gets just its commands: the prompted
/// lines without the `$ `, their continuations (after a trailing `\\`, or
/// behind a `> ` secondary prompt, which is stripped too), and blank lines
/// between commands. Every other line is the commands' output and is left
/// out, so Send + Run never feeds output to the shell. A console block with
/// no prompt at all is left exactly as written.
pub fn copyable(lang: Option<&str>, text: &str) -> String {
    let prompted = |l: &str| l == "$" || l.starts_with("$ ");
    let lines: Vec<&str> = text.split('\n').collect();
    let console = match lang {
        Some(lang) => CONSOLE_LANGS.contains(&lang),
        None => lines
            .iter()
            .find(|l| !l.trim().is_empty())
            .is_some_and(|l| prompted(l)),
    };
    if !console || !lines.iter().any(|l| prompted(l)) {
        return text.to_owned();
    }
    // Each line: Some(command text) or None (output); blank lines decided
    // after, by their neighbours.
    let mut kept: Vec<Option<&str>> = Vec::with_capacity(lines.len());
    // The previous line was a command (`Some`), and whether it ended in `\`.
    let mut previous: Option<bool> = None;
    for line in &lines {
        let command = if *line == "$" {
            Some("")
        } else if let Some(rest) = line.strip_prefix("$ ") {
            Some(rest)
        } else if previous == Some(true) {
            // After a trailing `\`: the same command, maybe behind `> `.
            Some(line.strip_prefix("> ").unwrap_or(line))
        } else if previous.is_some() && (line.starts_with("> ") || *line == ">") {
            // A secondary prompt right after a command: its continuation
            // (a heredoc body, an open quote).
            Some(line.strip_prefix("> ").unwrap_or(""))
        } else {
            None
        };
        previous = command.map(|c| c.ends_with('\\'));
        kept.push(command);
    }
    let blank = |i: usize| lines[i].trim().is_empty() && kept[i].is_none();
    let mut out: Vec<&str> = Vec::new();
    for (i, line) in kept.iter().enumerate() {
        match line {
            Some(command) => out.push(command),
            None if blank(i) => {
                let before = (0..i).rev().find(|&j| !blank(j));
                let after = (i + 1..lines.len()).find(|&j| !blank(j));
                let command = |j: Option<usize>| j.is_some_and(|j| kept[j].is_some());
                if command(before) && command(after) {
                    out.push("");
                }
            }
            None => {}
        }
    }
    out.join("\n")
}

/// Where a command was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandSource {
    /// A Claude Code transcript (`projects/*/<id>.jsonl`).
    Transcript,
    /// A Codex rollout (`sessions/YYYY/MM/DD/rollout-*-<id>.jsonl`).
    Rollout,
    /// A Codex `notify` payload's `last-assistant-message` (no rollout
    /// found).
    Notify,
}

/// A code block an agent suggested in one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// Per-session sequence number, increasing in arrival order.
    pub seq: u64,
    /// The agent conversation it came from.
    pub conversation: Option<String>,
    /// The reply it is part of (Claude's line `uuid`, Codex's message id
    /// or turn id).
    pub message_id: String,
    /// When the agent wrote it (RFC 3339, as the agent recorded it).
    pub timestamp: Option<String>,
    /// Its position among the reply's code blocks (0-based).
    pub block_index: usize,
    pub lang: Option<String>,
    /// A shell command block (see [`is_shell`]).
    pub shell: bool,
    /// The block exactly as written.
    pub text: String,
    /// What Copy / Send paste: `text`, or with `$ ` prompts stripped (see
    /// [`copyable`]).
    pub command: String,
    pub source: CommandSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(md: &str) -> Vec<(Option<String>, String, bool)> {
        fenced_blocks(md)
            .into_iter()
            .map(|b| (b.lang, b.text, b.closed))
            .collect()
    }

    #[test]
    fn extracts_exact_block_text_with_languages() {
        let md = "Run this:\n\n```bash\ncat <<'EOF' > /tmp/x\n  indented $HOME\n\tTab\nEOF\n```\n\nthen\n~~~ Python title=\"a\"\nprint(1)\n~~~\n";
        assert_eq!(
            blocks(md),
            vec![
                (
                    Some("bash".into()),
                    "cat <<'EOF' > /tmp/x\n  indented $HOME\n\tTab\nEOF".into(),
                    true
                ),
                (Some("python".into()), "print(1)".into(), true),
            ]
        );
    }

    #[test]
    fn fences_follow_commonmark_rules() {
        // Inline triple backticks are not a fence; a shorter or different
        // fence inside does not close; a longer one does.
        let md = "use ```x``` inline\n````md\n```bash\nls\n```\n`````\n";
        assert_eq!(
            blocks(md),
            vec![(Some("md".into()), "```bash\nls\n```".into(), true)]
        );
        // A closing fence may not carry text.
        let md = "```\na\n``` not\nb\n```";
        assert_eq!(blocks(md), vec![(None, "a\n``` not\nb".into(), true)]);
        // Unclosed: runs to the end; empty unclosed is dropped.
        assert_eq!(
            blocks("```sh\necho hi\n"),
            vec![(Some("sh".into()), "echo hi".into(), false)]
        );
        assert!(blocks("text\n```sh\n").is_empty());
        // Empty closed block survives (it is a block, if a useless one).
        assert_eq!(blocks("```\n```"), vec![(None, String::new(), true)]);
    }

    #[test]
    fn list_nested_blocks_lose_only_the_fence_indent() {
        let md = "1. Install:\n   ```bash\n   cargo install fw \\\n     --locked\n   ```\n";
        assert_eq!(
            blocks(md),
            vec![(
                Some("bash".into()),
                "cargo install fw \\\n  --locked".into(),
                true
            )]
        );
    }

    #[test]
    fn crlf_text_keeps_its_carriage_returns_in_content() {
        assert_eq!(
            blocks("```sh\r\necho a\r\n```\r\n"),
            vec![(Some("sh".into()), "echo a\r".into(), true)]
        );
    }

    #[test]
    fn classifies_shell_blocks() {
        for lang in ["bash", "sh", "zsh", "shell", "console", "fish"] {
            assert!(is_shell(Some(lang), "anything"), "{lang}");
        }
        assert!(!is_shell(Some("rust"), "cargo build"));
        assert!(!is_shell(Some("json"), "{}"));
        for text in [
            "git status",
            "$ cargo test",
            "# build it\nmake",
            "RUST_LOG=debug cargo run",
            "FOO=1",
            "./scripts/check.sh",
            "~/bin/tool --x",
            "/usr/bin/env python3",
            "deploy.sh prod",
            "sudo systemctl restart x",
        ] {
            assert!(is_shell(None, text), "{text:?}");
        }
        for text in [
            "",
            "# only a comment",
            "fn main() {}",
            "{\"a\": 1}",
            "error: could not compile",
            "The quick brown fox",
            "// comment",
            "= not an assignment",
        ] {
            assert!(!is_shell(None, text), "{text:?}");
        }
    }

    #[test]
    fn console_blocks_copy_only_their_commands() {
        assert_eq!(
            copyable(Some("console"), "$ cd x\n$ make\n$"),
            "cd x\nmake\n"
        );
        assert_eq!(copyable(None, "$ ls\n\n$ pwd"), "ls\n\npwd");
        // Output lines are left out (the reviewers' live block).
        assert_eq!(
            copyable(Some("console"), "$ echo \"hello\"\nhello\n$ pwd"),
            "echo \"hello\"\npwd"
        );
        assert_eq!(
            copyable(None, "$ ls\nfile.txt\n\nother.txt\n\n$ pwd\n/tmp"),
            "ls\npwd"
        );
        // Continuations: after a trailing backslash, and `> ` prompts.
        assert_eq!(
            copyable(
                Some("console"),
                "$ cargo build \\\n    --release\nCompiling\n$ cat <<EOF > f\n> a b\n> EOF\n$ wc -l f\n1 f"
            ),
            "cargo build \\\n    --release\ncat <<EOF > f\na b\nEOF\nwc -l f"
        );
        // No prompt at all, or not a console language: exactly as written.
        assert_eq!(copyable(Some("console"), "ls\nfile"), "ls\nfile");
        assert_eq!(copyable(Some("bash"), "$ ls\nx"), "$ ls\nx");
        assert_eq!(copyable(None, "ls\n$ pwd"), "ls\n$ pwd");
        // `$HOME` is not a prompt.
        assert_eq!(copyable(None, "$HOME/bin/x"), "$HOME/bin/x");
        assert_eq!(copyable(Some("console"), ""), "");
    }

    #[test]
    fn common_first_words_classify_untagged_blocks() {
        for text in [
            "bash -c 'echo hi'",
            "sh install.sh",
            "zsh -l",
            "sort -u a | wc -l",
            "diff a b",
            "[ -f x ] && echo y",
            "test -d x",
            "openssl rand -hex 32",
        ] {
            assert!(is_shell(None, text), "{text:?}");
        }
    }
}
