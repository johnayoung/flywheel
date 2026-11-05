//! `fw hub broadcast TARGET... TEXT`: one prompt to several agent sessions
//! (see [`crate::broadcast`]) through the running hub (or, with
//! `--machine`, its proxy to the hub that owns them).

use crate::endpoint::{Endpoint, EndpointArgs};
use crate::util::{fail, print_json, str_field};
use serde_json::{Value, json};
use std::io::{Read, Write};

#[derive(Debug, clap::Args)]
pub(crate) struct BroadcastArgs {
    /// The sessions and workspaces sent to (id, unique id prefix, or
    /// name; a workspace: each of its agent sessions), then the prompt
    /// (`-`: read stdin).
    #[arg(required = true, num_args = 1.., value_name = "TARGET... TEXT")]
    words: Vec<String>,
    /// Open the prompt with a line naming every session it went to.
    #[arg(long)]
    header: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn broadcast(args: &BroadcastArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run(args, out) {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

fn run(args: &BroadcastArgs, out: &mut dyn Write) -> Result<i32, String> {
    let (text, targets) = split(&args.words)?;
    let text = if text == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        text
    } else {
        text.clone()
    };
    let conn = Endpoint::resolve(&args.endpoint)?;
    let done = conn.blocking_call(
        "POST",
        "/api/broadcast",
        Some(&json!({ "targets": targets, "text": text, "header": args.header })),
    )?;
    if args.json {
        print_json(out, &done);
    } else {
        print_results(out, &done);
    }
    let refused = done["counts"]["refused"].as_u64().unwrap_or(0);
    Ok(i32::from(refused > 0))
}

/// The targets and the text (the last word).
fn split(words: &[String]) -> Result<(&String, &[String]), String> {
    match words.split_last() {
        Some((text, targets)) if !targets.is_empty() => Ok((text, targets)),
        _ => Err("name the sessions or workspaces, then the prompt".into()),
    }
}

/// One line per target: its result, the session, and why.
fn print_results(out: &mut dyn Write, done: &Value) {
    let id = str_field(done, "id");
    let counts = &done["counts"];
    let _ = writeln!(
        out,
        "broadcast {}: {} sent, {} queued, {} refused",
        &id[..id.len().min(8)],
        counts["sent"],
        counts["queued"],
        counts["refused"]
    );
    for result in done["results"].as_array().into_iter().flatten() {
        let who = match result.get("session").and_then(Value::as_str) {
            Some(name) => format!("{name} ({})", str_field(result, "kind")),
            None => str_field(result, "target").to_owned(),
        };
        let why = result
            .get("why")
            .and_then(Value::as_str)
            .map(|w| format!(": {}", crate::commands_cli::visible_controls(w)))
            .unwrap_or_default();
        let _ = writeln!(out, "  {:<8} {who}{why}", str_field(result, "result"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Cli {
        #[command(flatten)]
        args: BroadcastArgs,
    }

    fn parse(argv: &[&str]) -> Result<BroadcastArgs, clap::Error> {
        Cli::try_parse_from(std::iter::once("broadcast").chain(argv.iter().copied()))
            .map(|c| c.args)
    }

    #[test]
    fn targets_then_text_and_flags_anywhere_stay_flags() {
        let args = parse(&[
            "api",
            "--header",
            "web-1",
            "rebase on main",
            "--state-dir",
            "/s",
        ])
        .unwrap();
        assert_eq!(args.words, ["api", "web-1", "rebase on main"]);
        assert!(args.header);
        // `-` (stdin) is the text, never a flag.
        let args = parse(&["api", "-", "--json"]).unwrap();
        assert_eq!(args.words, ["api", "-"]);
        assert!(args.json);
        // A target and the text, at least.
        assert!(split(&parse(&["only-text"]).unwrap().words).is_err());
        let words = parse(&["a", "b", "go"]).unwrap().words;
        let (text, targets) = split(&words).unwrap();
        assert_eq!((text.as_str(), targets), ("go", &words[..2]));
    }
}
