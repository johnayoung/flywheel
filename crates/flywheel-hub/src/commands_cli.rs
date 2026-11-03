//! `fw hub commands <session>`: the code blocks an agent session suggested,
//! exactly as written, from the running hub.

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::script::resolve;
use std::io::{IsTerminal, Write};

/// Commonly the most a terminal accepts in one OSC 52 sequence (xterm's
/// default); larger copies are attempted, with a warning.
const OSC52_COMFORT_BYTES: usize = 74_994;

#[derive(Debug, clap::Args)]
pub struct CommandsArgs {
    /// Session id, unique id prefix, or name (a shell session: its
    /// workspace's agent session).
    session: String,
    /// Only the newest block, printed exactly (plus one newline).
    #[arg(long)]
    last: bool,
    /// Every fenced code block, not only shell commands.
    #[arg(long)]
    all: bool,
    /// JSON instead of text (an object with --last, else an array).
    #[arg(long)]
    json: bool,
    /// Copy the newest block to the clipboard of the terminal you are in
    /// (OSC 52; works over SSH; tmux needs `set -g set-clipboard on`; GNU
    /// screen gets it passed through). Prints only a confirmation, on
    /// stderr.
    #[arg(long, conflicts_with = "json")]
    copy: bool,
    /// How many blocks to list, newest first.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=500))]
    limit: u16,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

/// Run `fw hub commands`.
pub fn run(args: &CommandsArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // Block text is agent-written: on a terminal its control characters
    // are shown, not obeyed (piped output stays byte-exact).
    let escape = std::io::stdout().is_terminal();
    match fetch(args, err) {
        Ok((name, commands)) => match render(args, &name, &commands, escape, out, err) {
            Ok(code) => code,
            Err(message) => {
                let _ = writeln!(err, "fw hub: {message}");
                1
            }
        },
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            1
        }
    }
}

/// The agent session to read for `wanted`: the session itself, or, for a
/// shell session, the one agent session of its workspace (as the web UI's
/// drawer does). Returns (id, name, note to print).
pub fn resolve_agent(
    sessions: &[serde_json::Value],
    wanted: &str,
) -> Result<(String, String, Option<String>), String> {
    let session = resolve(sessions, wanted, "session")?;
    let field = |s: &serde_json::Value, k: &str| {
        s.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let (id, name) = (field(session, "id"), field(session, "name"));
    if matches!(field(session, "kind").as_str(), "claude" | "codex") {
        return Ok((id, name, None));
    }
    let workspace = field(session, "workspace_id");
    let agents: Vec<_> = sessions
        .iter()
        .filter(|s| {
            field(s, "workspace_id") == workspace
                && matches!(field(s, "kind").as_str(), "claude" | "codex")
        })
        .collect();
    match agents.as_slice() {
        [] => Err(format!(
            "{name:?} is a {} session, not an agent (Claude Code or Codex) session, and its workspace has none",
            field(session, "kind")
        )),
        [agent] => {
            let agent_name = field(agent, "name");
            let note = format!(
                "{name:?} is a shell session; showing its workspace's agent session {agent_name:?}"
            );
            Ok((field(agent, "id"), agent_name, Some(note)))
        }
        many => Err(format!(
            "{name:?} is a shell session; name one of its workspace's agent sessions: {}",
            many.iter()
                .map(|s| format!("{:?}", field(s, "name")))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The session's name and its commands, newest first.
fn fetch(
    args: &CommandsArgs,
    err: &mut dyn Write,
) -> Result<(String, Vec<serde_json::Value>), String> {
    let conn = Endpoint::resolve(&args.endpoint)?;
    let sessions = conn.blocking_sessions()?;
    let (id, name, note) = resolve_agent(&sessions, &args.session)?;
    if let Some(note) = note {
        let _ = writeln!(err, "fw hub: {note}");
    }
    let limit = if args.last || args.copy {
        1
    } else {
        args.limit
    };
    let path = format!("/api/sessions/{id}/commands?limit={limit}&all={}", args.all);
    let list = conn.blocking_call("GET", &path, None)?;
    let commands = list
        .get("commands")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    Ok((name, commands))
}

fn render(
    args: &CommandsArgs,
    name: &str,
    commands: &[serde_json::Value],
    escape: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let what = if args.all {
        "code blocks"
    } else {
        "shell commands"
    };
    let io = |e: std::io::Error| e.to_string();
    if args.json {
        let value = if args.last {
            commands.first().cloned().unwrap_or(serde_json::Value::Null)
        } else {
            serde_json::Value::Array(commands.to_vec())
        };
        writeln!(out, "{value}").map_err(io)?;
        return Ok(if args.last && commands.is_empty() {
            1
        } else {
            0
        });
    }
    let raw_of = |c: &serde_json::Value| {
        c.get("command")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let text_of = |c: &serde_json::Value| {
        let raw = raw_of(c);
        if escape { visible_controls(&raw) } else { raw }
    };
    if commands.is_empty() {
        writeln!(err, "fw hub: {name:?} has suggested no {what} yet").map_err(io)?;
        return Ok(if args.last || args.copy { 1 } else { 0 });
    }
    if args.copy {
        let text = raw_of(&commands[0]);
        copy_osc52(&text)?;
        let mut note = format!(
            "fw hub: copied {} bytes to the clipboard (OSC 52)",
            text.len()
        );
        if osc52(&text, false).len() > OSC52_COMFORT_BYTES {
            note.push_str("; it is large, and some terminals drop OSC 52 copies this big");
        }
        writeln!(err, "{note}").map_err(io)?;
        return Ok(0);
    }
    if args.last {
        writeln!(out, "{}", text_of(&commands[0])).map_err(io)?;
        return Ok(0);
    }
    for (i, command) in commands.iter().enumerate() {
        if i > 0 {
            writeln!(out).map_err(io)?;
        }
        let field = |k: &str| command.get(k).and_then(|v| v.as_str()).unwrap_or_default();
        let lang = match field("lang") {
            "" => "text",
            lang => lang,
        };
        writeln!(
            out,
            "# {} {lang} {}",
            command
                .get("seq")
                .and_then(|v| v.as_u64())
                .unwrap_or_default(),
            field("timestamp")
        )
        .map_err(io)?;
        writeln!(out, "{}", text_of(command)).map_err(io)?;
    }
    Ok(0)
}

/// `text` with C0 and C1 control characters (other than newline and tab)
/// shown as `^X` / `\u{..}`, so an escape sequence in a block is printed,
/// not run by the terminal.
pub fn visible_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            '\x7f' => out.push_str("^?"),
            c if (c as u32) < 0x20 => {
                out.push('^');
                out.push(char::from(b'@' + c as u8));
            }
            c if ('\u{80}'..='\u{9f}').contains(&c) => {
                out.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// The OSC 52 sequence that sets the clipboard to `text`. Inside GNU
/// screen (`screen`), it is passed through in DCS chunks, since screen
/// does not forward OSC 52 itself. (tmux forwards plain OSC 52 with
/// `set-clipboard on`.)
pub fn osc52(text: &str, screen: bool) -> String {
    let encoded = base64(text.as_bytes());
    if !screen {
        return format!("\x1b]52;c;{encoded}\x07");
    }
    // screen limits one DCS string; split the payload.
    let mut out = String::from("\x1bP\x1b]52;c;");
    for chunk in encoded.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push_str("\x1b\\\x1bP");
    }
    out.push_str("\x07\x1b\\");
    out
}

/// Whether we run inside GNU screen (and not a tmux inside it).
fn in_screen() -> bool {
    std::env::var_os("TMUX").is_none_or(|v| v.is_empty())
        && std::env::var_os("STY").is_some_and(|v| !v.is_empty())
}

/// Write the OSC 52 copy to the controlling terminal (stdout when that is
/// one, else `/dev/tty`), so `--copy` works even with stdout redirected.
fn copy_osc52(text: &str) -> Result<(), String> {
    let sequence = osc52(text, in_screen());
    let stdout = std::io::stdout();
    if stdout.is_terminal() {
        let mut lock = stdout.lock();
        return lock
            .write_all(sequence.as_bytes())
            .and_then(|()| lock.flush())
            .map_err(|e| e.to_string());
    }
    let mut tty = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .map_err(|e| format!("--copy needs a terminal to send OSC 52 to: {e}"))?;
    tty.write_all(sequence.as_bytes())
        .and_then(|()| tty.flush())
        .map_err(|e| e.to_string())
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (raw, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(raw.as_bytes()), encoded);
        }
        assert_eq!(osc52("ls\n", false), "\x1b]52;c;bHMK\x07");
        assert_eq!(
            osc52("ls\n", true),
            "\x1bP\x1b]52;c;bHMK\x1b\\\x1bP\x07\x1b\\"
        );
        // Long payloads: every DCS piece stays small.
        let long = osc52(&"x".repeat(300), true);
        assert!(long.split("\x1bP").all(|piece| piece.len() < 100));
    }

    #[test]
    fn terminal_output_shows_control_characters_instead_of_obeying_them() {
        let hostile = "\x1b]52;c;Y3VybCBldmlsfHNo\x07ls\tx\ny\u{9b}2J\x7f";
        assert_eq!(
            visible_controls(hostile),
            "^[]52;c;Y3VybCBldmlsfHNo^Gls\tx\ny\\u{9b}2J^?"
        );
        let args = <CommandsArgs as clap::Args>::augment_args(clap::Command::new("t"))
            .try_get_matches_from(["t", "s1", "--last"])
            .and_then(|m| <CommandsArgs as clap::FromArgMatches>::from_arg_matches(&m))
            .unwrap();
        let commands = [json!({"command": hostile})];
        let (mut out, mut err) = (Vec::new(), Vec::new());
        render(&args, "s1", &commands, true, &mut out, &mut err).unwrap();
        assert!(!out.contains(&0x1b), "no raw ESC reaches the terminal");
        // Piped: exact.
        let mut out = Vec::new();
        render(&args, "s1", &commands, false, &mut out, &mut err).unwrap();
        assert_eq!(out, format!("{hostile}\n").into_bytes());
    }

    #[test]
    fn a_shell_session_resolves_to_its_workspaces_agent() {
        let sessions = vec![
            json!({"id": "a1a1a1a1", "name": "sh", "kind": "shell", "workspace_id": "w1"}),
            json!({"id": "b2b2b2b2", "name": "cl", "kind": "claude", "workspace_id": "w1"}),
            json!({"id": "c3c3c3c3", "name": "lone", "kind": "shell", "workspace_id": "w2"}),
            json!({"id": "d4d4d4d4", "name": "two", "kind": "shell", "workspace_id": "w3"}),
            json!({"id": "e5e5e5e5", "name": "x1", "kind": "codex", "workspace_id": "w3"}),
            json!({"id": "f6f6f6f6", "name": "x2", "kind": "claude", "workspace_id": "w3"}),
        ];
        let (id, name, note) = resolve_agent(&sessions, "sh").unwrap();
        assert_eq!((id.as_str(), name.as_str()), ("b2b2b2b2", "cl"));
        assert!(note.unwrap().contains("shell session"));
        assert_eq!(resolve_agent(&sessions, "cl").unwrap().2, None);
        assert!(
            resolve_agent(&sessions, "lone")
                .unwrap_err()
                .contains("has none")
        );
        let err = resolve_agent(&sessions, "two").unwrap_err();
        assert!(err.contains("\"x1\"") && err.contains("\"x2\""), "{err}");
    }
}
