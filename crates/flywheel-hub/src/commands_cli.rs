//! `fw hub commands <session>`: the code blocks an agent session suggested,
//! exactly as written, from the running hub.

use crate::auth::Token;
use crate::client;
use crate::paths::default_state_dir;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Commonly the most a terminal accepts in one OSC 52 sequence (xterm's
/// default); larger copies are attempted, with a warning.
const OSC52_COMFORT_BYTES: usize = 74_994;

#[derive(Debug, clap::Args)]
pub struct CommandsArgs {
    /// The session: its id, a unique id prefix, or its exact name.
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
    /// (OSC 52; works over SSH; tmux needs `set -g set-clipboard on`).
    /// Prints nothing.
    #[arg(long, conflicts_with = "json")]
    copy: bool,
    /// How many blocks to list, newest first.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=500))]
    limit: u16,
    /// State directory [default: $XDG_STATE_HOME/flywheel/hub or
    /// ~/.local/state/flywheel/hub].
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
}

/// Run `fw hub commands`.
pub fn run(args: &CommandsArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match fetch(args) {
        Ok((name, commands)) => match render(args, &name, &commands, out, err) {
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

/// An authenticated GET against the running hub.
fn get(addr: std::net::SocketAddr, token: &Token, path: &str) -> Result<serde_json::Value, String> {
    let response = client::request(addr, "GET", path, token.expose(), None, CLIENT_TIMEOUT)
        .map_err(|e| format!("cannot reach the running hub at {addr}: {e}"))?;
    if response.status != 200 {
        return Err(format!(
            "the hub refused {path}: {} {}",
            response.status,
            response.error_message()
        ));
    }
    serde_json::from_slice(&response.body).map_err(|e| format!("bad answer from the hub: {e}"))
}

/// The session matching `wanted` by id, unique id prefix, or exact name.
pub fn resolve_session(
    sessions: &[serde_json::Value],
    wanted: &str,
) -> Result<(String, String), String> {
    let field = |s: &serde_json::Value, k: &str| {
        s.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let pair = |s: &serde_json::Value| (field(s, "id"), field(s, "name"));
    if let Some(exact) = sessions.iter().find(|s| field(s, "id") == wanted) {
        return Ok(pair(exact));
    }
    let named: Vec<_> = sessions
        .iter()
        .filter(|s| field(s, "name") == wanted)
        .collect();
    let prefixed: Vec<_> = sessions
        .iter()
        .filter(|s| wanted.len() >= 4 && field(s, "id").starts_with(wanted))
        .collect();
    match (named.as_slice(), prefixed.as_slice()) {
        ([one], _) | ([], [one]) => Ok(pair(one)),
        ([], []) => Err(format!("no session {wanted:?} (an id, id prefix, or name)")),
        (many, _) if many.len() > 1 => Err(format!(
            "{} sessions are named {wanted:?}; use an id: {}",
            many.len(),
            many.iter()
                .map(|s| field(s, "id"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        (_, many) => Err(format!(
            "id prefix {wanted:?} matches {} sessions: {}",
            many.len(),
            many.iter()
                .map(|s| field(s, "id"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn state_dir(arg: Option<&Path>) -> Result<PathBuf, String> {
    match arg {
        Some(dir) => Ok(dir.to_path_buf()),
        None => default_state_dir()
            .ok_or_else(|| "cannot resolve a state dir: set HOME or pass --state-dir".to_owned()),
    }
}

/// The session's name and its commands, newest first.
fn fetch(args: &CommandsArgs) -> Result<(String, Vec<serde_json::Value>), String> {
    let dir = state_dir(args.state_dir.as_deref())?;
    let info = crate::remote::running_hub(&dir)
        .map_err(|e| format!("runtime file in {}: {e}", dir.display()))?
        .ok_or_else(|| {
            format!(
                "no hub is running for state dir {} (start one with `fw hub serve`)",
                dir.display()
            )
        })?;
    let token = Token::load(&dir)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "the running hub's token file is missing".to_owned())?;
    let addr = info.connect_addr();
    let sessions = get(addr, &token, "/api/sessions")?;
    let sessions = sessions
        .get("sessions")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let (id, name) = resolve_session(&sessions, &args.session)?;
    let limit = if args.last || args.copy {
        1
    } else {
        args.limit
    };
    let path = format!("/api/sessions/{id}/commands?limit={limit}&all={}", args.all);
    let list = get(addr, &token, &path)?;
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
    let text_of = |c: &serde_json::Value| {
        c.get("command")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_owned()
    };
    if commands.is_empty() {
        writeln!(err, "fw hub: {name:?} has suggested no {what} yet").map_err(io)?;
        return Ok(if args.last || args.copy { 1 } else { 0 });
    }
    if args.copy {
        let text = text_of(&commands[0]);
        copy_osc52(&text)?;
        let mut note = format!(
            "fw hub: copied {} bytes to the clipboard (OSC 52)",
            text.len()
        );
        if osc52(&text).len() > OSC52_COMFORT_BYTES {
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

/// The OSC 52 sequence that sets the clipboard to `text`.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Write the OSC 52 copy to the controlling terminal (stdout when that is
/// one, else `/dev/tty`), so `--copy` works even with stdout redirected.
fn copy_osc52(text: &str) -> Result<(), String> {
    let sequence = osc52(text);
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
        assert_eq!(osc52("ls\n"), "\x1b]52;c;bHMK\x07");
    }

    #[test]
    fn sessions_resolve_by_id_prefix_or_name() {
        let sessions = vec![
            json!({"id": "aaaa1111", "name": "claude 1"}),
            json!({"id": "aaaa2222", "name": "shell"}),
            json!({"id": "bbbb3333", "name": "shell"}),
        ];
        let id = |w| resolve_session(&sessions, w).map(|(id, _)| id);
        assert_eq!(id("aaaa2222").unwrap(), "aaaa2222");
        assert_eq!(id("claude 1").unwrap(), "aaaa1111");
        assert_eq!(id("bbbb").unwrap(), "bbbb3333");
        assert!(id("aaaa").unwrap_err().contains("matches 2 sessions"));
        assert!(id("shell").unwrap_err().contains("2 sessions are named"));
        assert!(
            id("bbb").unwrap_err().contains("no session"),
            "short prefixes do not match"
        );
        assert!(id("zzzz").is_err());
    }
}
