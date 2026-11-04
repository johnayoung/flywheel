//! `fw hub tls ca | rotate`: the local CA built-in TLS (`--tls auto`)
//! signs with (see [`crate::tls`]).

use crate::cli::{StateDirArg, resolve_state_dir};
use crate::endpoint::Endpoint;
use crate::machines::display_fingerprint;
use crate::tls::{ExtraNames, LocalCa, discover_names};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a call to the running hub may take.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, clap::Args)]
pub(crate) struct TlsArgs {
    #[command(subcommand)]
    command: TlsCommand,
}

#[derive(Debug, clap::Subcommand)]
enum TlsCommand {
    /// Print the local CA's SHA-256 fingerprint, where its certificate is,
    /// and the names it is constrained to (made now if there is none);
    /// --out writes a copy of the certificate for a device.
    Ca(CaArgs),
    /// Replace the local CA with a new one for this hub's names now (a new
    /// address, or revoking the old CA): the old key is deleted, a running
    /// hub serves certificates from the new CA at once, and every device
    /// must install the new CA (and should remove the old one).
    Rotate(RotateArgs),
}

#[derive(Debug, clap::Args)]
struct CaArgs {
    /// Write the CA certificate to this file (PEM; DER with --der).
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
    /// With --out: DER (`.crt`/`.cer` for Windows and Android) instead of
    /// PEM.
    #[arg(long, requires = "out")]
    der: bool,
    #[command(flatten)]
    state: StateDirArg,
}

#[derive(Debug, clap::Args)]
struct RotateArgs {
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn main(args: &TlsArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = match &args.command {
        TlsCommand::Ca(args) => ca(args, out, err),
        TlsCommand::Rotate(args) => rotate(args, out, err),
    };
    match result {
        Ok(code) => code,
        Err(message) => {
            let _ = writeln!(err, "fw hub: {message}");
            1
        }
    }
}

/// Where the hub listens (the running one's address, else hub.toml's) and
/// its extra TLS names.
fn hub_listen(state_dir: &Path) -> Result<(SocketAddr, ExtraNames), String> {
    let effective = crate::config::load(
        state_dir,
        &crate::config::Flags::default(),
        crate::config::home_dir().as_deref(),
    )?;
    let listen = match crate::cli::running_hub(state_dir)? {
        Some(info) => info.listen,
        None => SocketAddr::new(effective.bind, effective.port),
    };
    Ok((listen, effective.tls_names))
}

fn describe(
    ca: &LocalCa,
    state_dir: &Path,
    names: &crate::tls::Names,
    extra: &ExtraNames,
    out: &mut dyn Write,
) {
    let date = |t| chrono::DateTime::<chrono::Utc>::from(t).format("%Y-%m-%d");
    let _ = writeln!(
        out,
        "local CA:            {}",
        crate::tls::ca_cert_path(state_dir).display()
    );
    let _ = writeln!(
        out,
        "SHA-256 fingerprint: {}",
        display_fingerprint(ca.fingerprint())
    );
    let _ = writeln!(
        out,
        "valid:               {} to {}",
        date(ca.created()),
        date(ca.expires())
    );
    let _ = writeln!(
        out,
        "name-constrained to: {}",
        ca.constraints().list().join(", ")
    );
    let _ = writeln!(
        out,
        "  (a device that installs it trusts it for these names, and names under them, only: it \
         cannot vouch for any other site)"
    );
    for line in crate::tls::public_warnings(&ca.constraints().public(names, extra), extra) {
        let _ = writeln!(out, "{line}");
    }
}

fn ca(args: &CaArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let state_dir = resolve_state_dir(&args.state)?;
    crate::paths::ensure_private_dir(&state_dir)
        .map_err(|e| format!("state dir {}: {e}", state_dir.display()))?;
    let (listen, extra) = hub_listen(&state_dir)?;
    let names = discover_names(listen, &extra);
    let (ca, created) = LocalCa::load_or_create(&state_dir, &names, &extra)?;
    if created {
        let _ = writeln!(err, "fw hub: made a local CA for this hub's names");
    }
    // A running hub knows its opt-ins (serve flags included); hub.toml
    // alone may not.
    let status = running_tls(&state_dir);
    let allowed = status
        .as_ref()
        .and_then(|s| s["public_names"].as_array())
        .and_then(|list| {
            let specs: Vec<_> = list
                .iter()
                .filter_map(|v| v.as_str())
                .filter_map(|raw| crate::tls::parse_name(raw).ok())
                .collect();
            ExtraNames::new(specs.clone(), specs).ok()
        });
    describe(
        &ca,
        &state_dir,
        &names,
        allowed.as_ref().unwrap_or(&extra),
        out,
    );
    if let Some(path) = &args.out {
        let bytes = if args.der {
            ca.cert_der().as_ref().to_vec()
        } else {
            ca.cert_pem().as_bytes().to_vec()
        };
        std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let _ = writeln!(out, "wrote the CA certificate to {}", path.display());
    }
    match status {
        Some(status) if status["mode"] == "auto" => {
            let outside = status["outside"].as_array().map_or(0, Vec::len);
            let urls = crate::tls::ca_page_urls(listen);
            let _ = writeln!(
                out,
                "the running hub serves it; devices install it from {}",
                if urls.is_empty() {
                    "/ca on a LAN address (the hub listens on loopback only)".to_owned()
                } else {
                    urls.join(" or ")
                }
            );
            if outside > 0 {
                let list: Vec<&str> = status["outside"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                    .collect();
                let _ = writeln!(
                    err,
                    "fw hub: tls: {} outside the constraints, so not in the hub's certificate \
                     (`fw hub tls rotate` covers it)",
                    list.join(", ")
                );
            }
        }
        _ => {
            let _ = writeln!(
                out,
                "serve with it: `fw hub serve --tls auto` (or hub.toml [server] tls = \"auto\")"
            );
        }
    }
    Ok(0)
}

/// The running hub's TLS status, if one runs and answers.
fn running_tls(state_dir: &Path) -> Option<serde_json::Value> {
    let hub = Endpoint::running(state_dir).ok()??;
    hub.with_timeout(CLIENT_TIMEOUT)
        .blocking_call("GET", "/api/tls", None)
        .ok()
}

fn rotate(args: &RotateArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let state_dir = resolve_state_dir(&args.state)?;
    crate::paths::ensure_private_dir(&state_dir)
        .map_err(|e| format!("state dir {}: {e}", state_dir.display()))?;
    let (listen, extra) = hub_listen(&state_dir)?;
    let old = LocalCa::load(&state_dir).ok().flatten();
    let names = discover_names(listen, &extra);
    let ca = LocalCa::rotate(&state_dir, &names, &extra)?;
    if let Some(old) = &old {
        let _ = writeln!(
            out,
            "replaced the local CA {} (its key is deleted: nothing new is signed by it)",
            display_fingerprint(old.fingerprint())
        );
    }
    describe(&ca, &state_dir, &names, &extra, out);
    match Endpoint::running(&state_dir)? {
        Some(hub) => {
            let status =
                hub.with_timeout(CLIENT_TIMEOUT)
                    .blocking_call("POST", "/api/tls/reload", None);
            match status {
                Ok(_) => {
                    let _ = writeln!(
                        out,
                        "the running hub now serves a certificate from the new CA"
                    );
                }
                Err(message) if message.contains("TLS is off") => {
                    let _ = writeln!(
                        out,
                        "the running hub does not serve TLS; `--tls auto` uses the new CA"
                    );
                }
                Err(message) => {
                    let _ = writeln!(
                        err,
                        "fw hub: the running hub did not reload ({message}); it picks the new CA up within a minute"
                    );
                }
            }
        }
        None => {
            let _ = writeln!(
                out,
                "{}; it serves from the new CA when started with `--tls auto`",
                crate::endpoint::no_hub_message(&state_dir)
            );
        }
    }
    let urls = crate::tls::ca_page_urls(listen);
    let _ = writeln!(
        out,
        "every device must install the new CA ({}) and should remove the old one",
        if urls.is_empty() {
            "/ca".to_owned()
        } else {
            urls.join(" or ")
        }
    );
    Ok(0)
}
