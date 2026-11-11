//! What kind of call a tool call was, for counting where time goes: a
//! shell command by its program and verb (`cd x && cargo test -p y` is
//! `cargo test`), a wait loop as `poll`, a background command marked
//! `&`; a file edit by the file it writes; any other tool by its name
//! alone.

/// Tools that run a shell command (Claude Code's, Codex's).
const SHELLS: &[&str] = &[
    "Bash",
    "shell",
    "local_shell",
    "exec_command",
    "shell_command",
];
/// Tools that write a file.
const WRITES: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit", "apply_patch"];
/// Programs whose first word after the name is a verb worth keeping.
const VERBED: &[&str] = &[
    "cargo",
    "git",
    "npm",
    "pnpm",
    "yarn",
    "bun",
    "docker",
    "kubectl",
    "go",
    "make",
    "just",
    "uv",
    "pip",
    "pip3",
    "terraform",
    "systemctl",
    "brew",
    "apt",
    "rustup",
    "deno",
    "pytest",
];
/// Programs whose first two words after the name are verbs (`gh run watch`).
const TWO_VERBS: &[&str] = &["gh", "fw", "flywheel", "az", "gcloud", "aws"];
/// Interpreters: the script they run is what tells.
const INTERPRETERS: &[&str] = &[
    "python", "python3", "node", "bash", "sh", "zsh", "ruby", "perl",
];
/// Words that only wrap the command they precede.
const WRAPPERS: &[&str] = &[
    "timeout", "time", "env", "nice", "sudo", "nohup", "exec", "command",
];
/// Wrappers whose first argument is theirs (`flock LOCKFILE cmd`).
const WRAPPERS_WITH_ARG: &[&str] = &["flock"];

/// The shape of one finished call of `tool`: empty for a tool told apart
/// by its name alone.
pub fn of(tool: &str, summary: Option<&str>) -> String {
    let summary = summary.map(str::trim).unwrap_or_default();
    if SHELLS.contains(&tool) {
        return command(summary);
    }
    if WRITES.contains(&tool) {
        return summary.to_owned();
    }
    String::new()
}

/// Whether a shape is a wait loop.
pub fn is_poll(shape: &str) -> bool {
    shape == POLL || shape.starts_with("poll ")
}

const POLL: &str = "poll";

/// A shell command's shape.
pub fn command(line: &str) -> String {
    let (line, background) = match line.strip_suffix('&') {
        Some(rest) if !rest.ends_with('&') => (rest.trim_end(), true),
        _ => (line, false),
    };
    let mark = |shape: String| {
        if background {
            format!("{shape} &")
        } else {
            shape
        }
    };
    if polls(line) {
        return mark(POLL.to_owned());
    }
    for segment in segments(line) {
        let words: Vec<&str> = segment.split_whitespace().collect();
        let mut at = 0;
        while at < words.len() {
            let word = words[at];
            let assignment = word.contains('=') && !word.starts_with('-') && !word.starts_with('=');
            if WRAPPERS_WITH_ARG.contains(&word) {
                at += 1;
                while at < words.len()
                    && (words[at].starts_with('-') || words[at].bytes().all(|b| b.is_ascii_digit()))
                {
                    at += 1;
                }
                at += 1;
                continue;
            }
            if assignment {
                // `P="flock x cargo"`: a quoted value runs to its closing
                // quote.
                let quote = ['"', '\'']
                    .into_iter()
                    .find(|q| word.matches(*q).count() % 2 == 1);
                at += 1;
                if let Some(quote) = quote {
                    while at < words.len() && !words[at].contains(quote) {
                        at += 1;
                    }
                    at += 1;
                }
                continue;
            }
            if WRAPPERS.contains(&word) {
                at += 1;
                // `timeout 300 cmd`, `nice -n 5 cmd`: their arguments too.
                while at < words.len()
                    && (words[at].starts_with('-')
                        || words[at]
                            .bytes()
                            .all(|b| b.is_ascii_digit() || b == b'.' || b == b's' || b == b'm'))
                    && !VERBED.contains(&words[at])
                {
                    at += 1;
                }
                continue;
            }
            break;
        }
        let Some(first) = words.get(at) else {
            continue;
        };
        let program = first.rsplit('/').next().unwrap_or(first);
        if matches!(
            program,
            "cd" | "export" | "source" | "." | "set" | "pushd" | "popd" | "true"
        ) {
            continue;
        }
        // Flags are not verbs, nor the value `-C dir` (git, make) takes.
        let mut verbs: Vec<&str> = Vec::new();
        let mut rest = words[at + 1..].iter().copied();
        while let Some(word) = rest.next() {
            if word == "-C" || word == "-c" {
                rest.next();
            } else if !word.starts_with('-') && !word.contains('=') {
                verbs.push(word);
            }
        }
        let take = if TWO_VERBS.contains(&program) {
            2
        } else if VERBED.contains(&program) {
            1
        } else {
            0
        };
        if take > 0 {
            let mut shape = vec![program.to_owned()];
            shape.extend(
                verbs
                    .iter()
                    .take(take)
                    .take_while(|w| {
                        w.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    })
                    .map(|w| (*w).to_owned()),
            );
            return mark(shape.join(" "));
        }
        if INTERPRETERS.contains(&program) || program.starts_with("python3.") {
            let script = words[at + 1..].iter().find(|w| !w.starts_with('-'));
            // `-c CODE`, `-e CODE`, or a heredoc: code written inline.
            let inline = words[at + 1..]
                .iter()
                .any(|w| *w == "-c" || *w == "-e" || w.starts_with("<<") || *w == "-");
            return mark(match (inline, script) {
                (true, _) => format!("{program} -c"),
                (false, Some(script)) => {
                    format!("{program} {}", script.rsplit('/').next().unwrap_or(script))
                }
                (false, None) => program.to_owned(),
            });
        }
        return mark(program.to_owned());
    }
    mark(String::new())
}

/// A command that waits by looping or sleeping.
fn polls(line: &str) -> bool {
    let words: Vec<&str> = line
        .split(|c: char| c.is_whitespace() || c == ';' || c == '&' || c == '|' || c == '(')
        .filter(|w| !w.is_empty())
        .collect();
    words
        .first()
        .is_some_and(|w| matches!(*w, "until" | "while" | "watch"))
        || words.contains(&"sleep")
}

/// The commands of a line, split at `&&`, `||`, `;`, and `|`.
fn segments(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let two = bytes.get(i..i + 2);
        if two == Some(b"&&") || two == Some(b"||") {
            out.push(&line[start..i]);
            i += 2;
            start = i;
            continue;
        }
        if bytes[i] == b';' || bytes[i] == b'|' {
            out.push(&line[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    out.push(&line[start..]);
    out.into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_known_by_program_and_verb() {
        let cases = [
            (
                "cd ~/code/flywheel && cargo test -p flywheel-hub --lib lanes",
                "cargo test",
            ),
            ("cargo build -q -p flywheel 2>&1 | tail -3", "cargo build"),
            (
                "RUST_LOG=debug timeout 300 cargo clippy --all-targets",
                "cargo clippy",
            ),
            ("git -C repo status --short", "git status"),
            ("gh run watch 4311 --exit-status", "gh run watch"),
            (
                "~/code/flywheel/target/debug/fw hub fleet --lanes",
                "fw hub fleet",
            ),
            ("scripts/check.sh 2>&1 | tail -5", "check.sh"),
            ("python3 -c 'print(1)'", "python3 -c"),
            ("python3 - <<'EOF'", "python3 -c"),
            ("python3 <<'EOF'", "python3 -c"),
            (
                "flock -w 600 /tmp/intel-cargo.lock cargo build",
                "cargo build",
            ),
            ("P=\"flock /tmp/l nice cargo\" && $P test", "$P"),
            ("A='x y' cargo check", "cargo check"),
            ("python3 /tmp/mine.py --all", "python3 mine.py"),
            ("export A=1; npm run build", "npm run"),
            ("cargo test &", "cargo test &"),
            ("ls -la", "ls"),
            ("", ""),
        ];
        for (line, shape) in cases {
            assert_eq!(command(line), shape, "{line}");
        }
    }

    #[test]
    fn waits_are_polls() {
        for line in [
            "sleep 30 && gh run view 1",
            "until grep -q ok log; do sleep 2; done",
            "while kill -0 123 2>/dev/null; do sleep 10; done",
            "timeout 175 bash -c 'while kill -0 1; do sleep 10; done'",
        ] {
            assert!(is_poll(&command(line)), "{line}: {}", command(line));
        }
        assert!(!is_poll(&command("cargo test")));
    }

    #[test]
    fn edits_are_known_by_their_file_and_other_tools_by_name() {
        assert_eq!(of("Write", Some("/tmp/x.py")), "/tmp/x.py");
        assert_eq!(of("Read", Some("/tmp/x.py")), "");
        assert_eq!(of("Bash", Some("cargo fmt --all")), "cargo fmt");
    }
}
