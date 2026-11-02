//! Restart restore: how a session the hub stopped on a clean shutdown comes
//! back when the hub starts again (same id, name, workspace, and place in
//! its workspace's order; a fresh PTY in its saved working directory).
//!
//! The decision is per kind ([`plan`]). Plain interactive shells relaunch
//! as they were; anything that cannot come back as itself -- a shell
//! running a one-off command, an agent CLI without native resume -- comes
//! back as a fresh hub shell in the same directory, never by re-running
//! the original command (which may not be safe to repeat).

use crate::agent::SessionKind;
use crate::state::SessionRecord;
use std::path::Path;

/// Shells relaunched as they were recorded (an interactive shell keeps no
/// work that rerunning it would repeat).
const INTERACTIVE_SHELLS: [&str; 10] = [
    "sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "nu",
];
/// Flags an interactive shell may carry and still be relaunched as is.
const INTERACTIVE_FLAGS: [&str; 3] = ["-l", "-i", "--login"];

/// What a restored session runs. Its record keeps its own kind and
/// command either way: a [`RestoreHow::Shell`] plan's command is recorded
/// as the session's `fallback_command`, so a later restore (say, by a hub
/// whose agent kind gained native resume) can still bring it back as
/// itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    pub command: Vec<String>,
    pub how: RestoreHow,
}

/// How [`plan`] brings a session back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreHow {
    /// Its recorded command, relaunched as is.
    Relaunch,
    /// A fresh hub shell in its working directory (it could not come back
    /// as itself).
    Shell,
}

impl RestoreHow {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Relaunch => "relaunched",
            Self::Shell => "as a fresh shell",
        }
    }
}

/// How `record` comes back, given the hub's shell (`default_shell`).
///
/// The per-kind hook: an agent kind with native resume (Claude Code's
/// `--resume <session id>`, Codex's `resume`) answers here with its own
/// [`RestoreHow::Relaunch`] plan (its resume argv); one without falls back
/// to a shell. The plan is computed from the record's own kind and
/// command on every restore, never from a previous fallback.
pub fn plan(record: &SessionRecord, default_shell: &[String]) -> RestorePlan {
    match record.kind {
        SessionKind::Shell if is_interactive_shell(&record.command) => RestorePlan {
            command: record.command.clone(),
            how: RestoreHow::Relaunch,
        },
        SessionKind::Shell | SessionKind::Claude | SessionKind::Codex => {
            shell_fallback(default_shell)
        }
    }
}

/// A fresh hub shell.
fn shell_fallback(default_shell: &[String]) -> RestorePlan {
    RestorePlan {
        command: default_shell.to_vec(),
        how: RestoreHow::Shell,
    }
}

/// An interactive shell: a known shell program with at most login /
/// interactive flags (no script, no `-c`).
fn is_interactive_shell(argv: &[String]) -> bool {
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    let name = Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A login shell's argv[0] may carry a leading `-`.
    let name = name.trim_start_matches('-');
    INTERACTIVE_SHELLS.contains(&name)
        && args.iter().all(|a| INTERACTIVE_FLAGS.contains(&a.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionState;
    use chrono::Utc;

    fn record(kind: SessionKind, command: &[&str]) -> SessionRecord {
        SessionRecord {
            id: "s".into(),
            workspace_id: "w".into(),
            name: "n".into(),
            kind,
            cwd: "/tmp".into(),
            command: command.iter().map(|s| (*s).to_owned()).collect(),
            agent_session_id: None,
            created_at: Utc::now(),
            state: SessionState::stopped(Utc::now(), true),
            fallback_command: None,
        }
    }

    #[test]
    fn interactive_shells_relaunch_as_recorded() {
        let shell = vec!["/bin/zsh".to_owned()];
        for argv in [
            &["/bin/bash"][..],
            &["zsh", "-l"],
            &["/usr/bin/fish", "--login", "-i"],
            &["-bash"],
        ] {
            let plan = plan(&record(SessionKind::Shell, argv), &shell);
            assert_eq!(plan.how, RestoreHow::Relaunch, "{argv:?}");
            assert_eq!(plan.command, argv.to_vec(), "{argv:?}");
        }
    }

    #[test]
    fn a_previous_fallback_does_not_change_the_plan() {
        let shell = vec!["/bin/zsh".to_owned()];
        let mut agent = record(SessionKind::Claude, &["/usr/bin/claude"]);
        agent.fallback_command = Some(vec!["/bin/bash".to_owned()]);
        assert_eq!(plan(&agent, &shell).command, shell);
        let mut interactive = record(SessionKind::Shell, &["/bin/bash"]);
        interactive.fallback_command = Some(vec!["/bin/zsh".to_owned()]);
        assert_eq!(plan(&interactive, &shell).how, RestoreHow::Relaunch);
    }

    #[test]
    fn commands_and_agents_come_back_as_a_fresh_shell() {
        let shell = vec!["/bin/zsh".to_owned()];
        for (kind, argv) in [
            (SessionKind::Shell, &["/bin/sh", "-c", "make deploy"][..]),
            (SessionKind::Shell, &["bash", "script.sh"]),
            (SessionKind::Shell, &["vim", "notes"]),
            (SessionKind::Shell, &[]),
            (SessionKind::Claude, &["/usr/bin/claude"]),
            (SessionKind::Codex, &["codex"]),
        ] {
            let plan = plan(&record(kind, argv), &shell);
            assert_eq!(
                plan,
                RestorePlan {
                    command: shell.clone(),
                    how: RestoreHow::Shell,
                },
                "{kind:?} {argv:?}"
            );
        }
    }
}
