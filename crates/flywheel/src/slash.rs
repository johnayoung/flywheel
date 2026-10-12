//! Slash-command vocabulary shared by the dashboard and session screens.
//!
//! Both screens carry a persistent input bar; a leading `/` switches the
//! input into command mode. The verbs in [`SLASH_COMMANDS`] reuse the exact
//! store-mediated semantics of their pre-existing CLI / key-binding twins --
//! the spec calls this an "alternate input surface, not a new channel".
//!
//! This module is deliberately UI-agnostic: it parses the typed line, names
//! the verbs, builds the help text, and surfaces the unknown-command error
//! message. The screens own the actual dispatch (enqueue against the
//! selected / viewed run, pop the screen, exit the app, ...) because the
//! shape of those side effects differs between the dashboard and the
//! session view.

/// `/help` -- list the slash commands.
pub const SLASH_HELP: &str = "help";
/// `/status` -- the selected / viewed run's lifecycle status.
pub const SLASH_STATUS: &str = "status";
/// `/approve` -- approve the parked manual gate.
pub const SLASH_APPROVE: &str = "approve";
/// `/reject [feedback]` -- reject the parked manual gate.
pub const SLASH_REJECT: &str = "reject";
/// `/interrupt` -- interrupt the run.
pub const SLASH_INTERRUPT: &str = "interrupt";
/// `/archive` -- archive completed phases.
pub const SLASH_ARCHIVE: &str = "archive";
/// `/history` -- open the finished-run listing.
pub const SLASH_HISTORY: &str = "history";
/// `/worker start|stop` -- the supervised worker handle.
pub const SLASH_WORKER: &str = "worker";
/// `/autopilot start|stop` -- the supervised autopilot handle.
pub const SLASH_AUTOPILOT: &str = "autopilot";
/// `/quit` -- exit through the detach / stop prompt.
pub const SLASH_QUIT: &str = "quit";
/// `/exit` -- stop every supervised daemon and exit immediately.
pub const SLASH_EXIT: &str = "exit";

/// The verbs the input bar honours, in help-listing order.
pub const SLASH_COMMANDS: [&str; 11] = [
    SLASH_HELP,
    SLASH_STATUS,
    SLASH_APPROVE,
    SLASH_REJECT,
    SLASH_INTERRUPT,
    SLASH_ARCHIVE,
    SLASH_HISTORY,
    SLASH_WORKER,
    SLASH_AUTOPILOT,
    SLASH_QUIT,
    SLASH_EXIT,
];

/// The `/help` listing, one line per entry.
pub const HELP_LINES: [&str; 12] = [
    "available slash commands",
    "  /help               show this list",
    "  /status             show the selected/viewed run's lifecycle status",
    "  /approve            approve the parked manual gate",
    "  /reject [feedback]  reject the parked manual gate (optional feedback)",
    "  /interrupt          interrupt the run",
    "  /archive            archive completed phases",
    "  /history            list finished runs (enter opens a run's session)",
    "  /worker start|stop  spawn or gracefully stop the supervised worker",
    "  /autopilot start|stop  spawn or stop the supervised autopilot daemon",
    "  /quit               exit (prompts to detach or stop running daemons)",
    "  /exit               stop ALL supervised daemons and exit immediately",
];

/// [`HELP_LINES`] joined with newlines: the text `/help` surfaces inline.
pub fn help_text() -> String {
    HELP_LINES.join("\n")
}

/// One parsed slash-command invocation.
///
/// `verb` is the bare name (no leading `/`); `argument` is the rest of the
/// line after the verb, stripped of surrounding whitespace. `raw` is the
/// original typed line minus the leading `/` so a re-render of the input on
/// error can preserve the operator's keystrokes exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub verb: String,
    pub argument: String,
    pub raw: String,
}

/// Whether `text` begins with the slash-command sigil.
///
/// Whitespace-only / empty strings (and a bare `/`) return `false` so a
/// stray `/` is rejected at the parser rather than treated as a verb.
pub fn is_slash(text: &str) -> bool {
    text.starts_with('/') && text.trim().len() > 1
}

/// Split a leading-slash line into `(verb, argument)`.
///
/// The verb is the first whitespace-delimited token after `/`; the argument
/// is everything after the first space, stripped. An isolated `/verb`
/// carries an empty argument. Returns `None` when the line (after leading
/// whitespace) does not start with `/`.
pub fn parse_slash(text: &str) -> Option<SlashCommand> {
    let stripped = text.trim_start();
    let body = stripped.strip_prefix('/')?;
    if body.is_empty() || body.starts_with(char::is_whitespace) {
        return Some(SlashCommand {
            verb: String::new(),
            argument: String::new(),
            raw: body.to_owned(),
        });
    }
    let (head, tail) = match body.split_once(' ') {
        Some((head, tail)) => (head, tail),
        None => (body, ""),
    };
    Some(SlashCommand {
        verb: head.trim().to_owned(),
        argument: tail.trim().to_owned(),
        raw: body.to_owned(),
    })
}

/// The inline error rendered when a slash verb is not recognised: name the
/// bad verb, point at `/help`.
pub fn unknown_command_notice(verb: &str) -> String {
    format!("unknown slash command /{verb}; try /help")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_slash_requires_a_verb_after_the_sigil() {
        assert!(is_slash("/help"));
        assert!(is_slash("/reject needs work"));
        assert!(!is_slash("/"));
        assert!(!is_slash("/   "));
        assert!(!is_slash(""));
        assert!(!is_slash("help"));
        assert!(!is_slash(" /help"));
    }

    #[test]
    fn parse_slash_splits_verb_and_argument() {
        let cmd = parse_slash("/reject needs more tests ").unwrap();
        assert_eq!(cmd.verb, "reject");
        assert_eq!(cmd.argument, "needs more tests");
        assert_eq!(cmd.raw, "reject needs more tests ");
    }

    #[test]
    fn parse_slash_isolated_verb_has_empty_argument() {
        let cmd = parse_slash("/approve").unwrap();
        assert_eq!(cmd.verb, "approve");
        assert_eq!(cmd.argument, "");
        assert_eq!(cmd.raw, "approve");
    }

    #[test]
    fn parse_slash_bare_sigil_or_leading_space_yields_empty_verb() {
        let cmd = parse_slash("/").unwrap();
        assert_eq!(cmd.verb, "");
        assert_eq!(cmd.raw, "");
        let cmd = parse_slash("/ status").unwrap();
        assert_eq!(cmd.verb, "");
        assert_eq!(cmd.argument, "");
        assert_eq!(cmd.raw, " status");
    }

    #[test]
    fn parse_slash_tolerates_leading_whitespace_and_rejects_non_slash() {
        let cmd = parse_slash("   /worker start").unwrap();
        assert_eq!(cmd.verb, "worker");
        assert_eq!(cmd.argument, "start");
        assert!(parse_slash("worker start").is_none());
    }

    #[test]
    fn help_text_names_every_verb() {
        let text = help_text();
        for verb in SLASH_COMMANDS {
            assert!(text.contains(&format!("/{verb}")), "missing /{verb}");
        }
        assert!(text.starts_with("available slash commands\n"));
        assert_eq!(text.lines().count(), HELP_LINES.len());
    }

    #[test]
    fn unknown_command_notice_names_verb_and_help() {
        assert_eq!(
            unknown_command_notice("nope"),
            "unknown slash command /nope; try /help"
        );
        assert_eq!(
            unknown_command_notice(""),
            "unknown slash command /; try /help"
        );
    }
}
