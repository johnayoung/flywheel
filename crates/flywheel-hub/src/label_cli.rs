//! `fw hub label SESSION [KEY=VALUE]... [--unset KEY]...`: a session's
//! labels (see [`crate::labels`]), printed or changed, optionally only when
//! a condition holds (`--if VALUE`, `--if-unset`: a claim).

use crate::endpoint::EndpointArgs;
use crate::script::find_session;
use crate::util::{fail, print_json};
use serde_json::{Value, json};
use std::io::Write;

#[derive(Debug, clap::Args)]
pub(crate) struct LabelArgs {
    /// The session (id, unique id prefix, or name).
    session: String,
    /// Labels to set (`KEY=VALUE`; `KEY=` removes one). None: print them.
    #[arg(value_name = "KEY=VALUE")]
    pairs: Vec<String>,
    /// Remove this label; repeatable.
    #[arg(long, value_name = "KEY")]
    unset: Vec<String>,
    /// Change only when the one key named now holds VALUE (exit 1, nothing
    /// changed, when it does not).
    #[arg(long = "if", value_name = "VALUE", conflicts_with = "if_unset")]
    if_value: Option<String>,
    /// Change only when the one key named holds no value now: the first
    /// of several claims wins, the rest exit 1.
    #[arg(long)]
    if_unset: bool,
    /// Print JSON: {"labels":{...},"state"?}.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

pub(crate) fn label(args: &LabelArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run(args) {
        Ok(view) => {
            if args.json {
                print_json(out, &view);
            } else {
                print_labels(out, &view);
            }
            0
        }
        Err(message) => fail(err, &message),
    }
}

fn run(args: &LabelArgs) -> Result<Value, String> {
    let change =
        crate::labels::parse_change(&args.pairs, &args.unset).map_err(|e| e.to_string())?;
    let conn = crate::endpoint::Endpoint::resolve(&args.endpoint)?;
    let (id, _) = find_session(&conn, &args.session)?;
    let path = format!("/api/sessions/{id}/labels");
    if change.is_empty() {
        if args.if_value.is_some() || args.if_unset {
            return Err("--if and --if-unset need a label to set".into());
        }
        return conn.blocking_call("GET", &path, None);
    }
    let mut body = json!({ "labels": change });
    if let Some(value) = &args.if_value {
        body["if"] = json!(value);
    }
    if args.if_unset {
        body["if_unset"] = json!(true);
    }
    conn.blocking_call("PATCH", &path, Some(&body))
}

/// `key=value` per line, sorted (the hub's `fw.*` last), then `fw.state`.
fn print_labels(out: &mut dyn Write, view: &Value) {
    let labels = view["labels"].as_object().cloned().unwrap_or_default();
    let (hub, own): (Vec<_>, Vec<_>) = labels
        .iter()
        .partition(|(k, _)| k.starts_with(crate::labels::HUB_PREFIX));
    for (key, value) in own.into_iter().chain(hub) {
        let _ = writeln!(out, "{key}={}", value.as_str().unwrap_or_default());
    }
    if let Some(state) = view.get("state").and_then(Value::as_str) {
        let _ = writeln!(out, "{}={state}", crate::labels::STATE);
    }
}
