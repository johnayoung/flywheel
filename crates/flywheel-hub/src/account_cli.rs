//! `fw hub account ls | add | rm`: Claude Code accounts (see
//! [`crate::accounts`]). Local to the state dir: no running hub needed,
//! and a running one sees a change at the next launch.

use crate::accounts;
use crate::cli::{StateDirArg, resolve_state_dir};
use crate::util::print_json;
use std::io::Write;

#[derive(Debug, clap::Args)]
pub(crate) struct AccountArgs {
    #[command(subcommand)]
    command: AccountCommand,
}

#[derive(Debug, clap::Subcommand)]
enum AccountCommand {
    /// List the accounts (names and when their token was written; never
    /// the tokens).
    Ls(LsArgs),
    /// Store a token from `claude setup-token` as account NAME, read from
    /// stdin (no echo on a terminal; never on the command line).
    Add(AddArgs),
    /// Delete account NAME (sessions launched with it fail to relaunch
    /// until it is added again).
    Rm(RmArgs),
}

#[derive(Debug, clap::Args)]
struct LsArgs {
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

#[derive(Debug, clap::Args)]
struct AddArgs {
    /// Account name: lowercase letters, digits, - and _.
    name: String,
    /// Replace the token of an existing account.
    #[arg(long)]
    replace: bool,
    #[command(flatten)]
    state: StateDirArg,
}

#[derive(Debug, clap::Args)]
struct RmArgs {
    /// The account to delete.
    name: String,
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn main(args: &AccountArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = match &args.command {
        AccountCommand::Ls(args) => ls(args, out),
        AccountCommand::Add(args) => add(args, out, err),
        AccountCommand::Rm(args) => rm(args, out),
    };
    match result {
        Ok(()) => 0,
        Err(message) => crate::util::fail(err, &message),
    }
}

fn ls(args: &LsArgs, out: &mut dyn Write) -> Result<(), String> {
    let state_dir = resolve_state_dir(&args.state)?;
    let list = accounts::list(&state_dir);
    if args.json {
        print_json(out, &serde_json::json!({ "accounts": list }));
        return Ok(());
    }
    if list.is_empty() {
        let _ = writeln!(
            out,
            "no Claude accounts: `claude setup-token` (signed in as the account), then `fw hub account add NAME`"
        );
    }
    for account in list {
        let _ = writeln!(
            out,
            "{}  {}",
            account.name,
            crate::search_cli::local_time(&account.updated.to_rfc3339())
        );
    }
    Ok(())
}

fn add(args: &AddArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<(), String> {
    let state_dir = resolve_state_dir(&args.state)?;
    accounts::validate_name(&args.name).map_err(|e| e.to_string())?;
    if accounts::exists(&state_dir, &args.name) && !args.replace {
        return Err(format!(
            "account {:?} exists: --replace to change its token",
            args.name
        ));
    }
    crate::paths::ensure_private_dir(&state_dir)
        .map_err(|e| format!("state dir {}: {e}", state_dir.display()))?;
    let token =
        crate::machine_cli::read_secret(None, "the token `claude setup-token` printed", err)?;
    accounts::add(&state_dir, &args.name, &token, args.replace).map_err(|e| e.to_string())?;
    let _ = writeln!(
        out,
        "added Claude account {:?}: new claude sessions can launch with it (--account {})",
        args.name, args.name
    );
    Ok(())
}

fn rm(args: &RmArgs, out: &mut dyn Write) -> Result<(), String> {
    let state_dir = resolve_state_dir(&args.state)?;
    accounts::remove(&state_dir, &args.name).map_err(|e| e.to_string())?;
    let _ = writeln!(out, "removed Claude account {:?}", args.name);
    Ok(())
}
