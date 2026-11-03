//! What a screen verdict does to an agent session's status. Reports from
//! the agent's integration (Claude Code hooks, Codex `notify`) win; the
//! screen fills only the gaps they leave:
//!
//! * Nothing reported yet: a dialog (folder trust) is `blocked`, a prompt
//!   `idle` (Codex: a running turn `working`).
//! * Codex has no turn-start or approval hook: its screen drives
//!   `working` and `blocked` throughout; `notify` still ends turns.
//! * A reported `working`/`blocked` spell whose screen showed a running
//!   turn or a dialog, then shows neither for [`SETTLE_AFTER`]: the turn
//!   was interrupted (Esc fires no hook), `idle`. A spell that never
//!   showed one (a narrow terminal cuts the footer) is left alone.
//! * A reported `blocked` whose dialog closes onto a running turn was
//!   approved: `working` (the tool's own hook comes only once it ends).
//! * A reported `working` with a known dialog on screen for
//!   [`SETTLE_AFTER`] and no hook: `blocked`.
//!
//! No verdict (an unknown screen) never makes anything `blocked`.

use super::{ScreenState, Verdict};
use crate::agent::SessionKind;
use crate::status::{AgentStatus, StatusSource};
use std::time::{Duration, Instant};

/// How long a screen must hold before it ends a spell (interrupt, an
/// unreported dialog, a screen-driven turn going idle).
pub const SETTLE_AFTER: Duration = Duration::from_secs(3);
/// Output silence after which a screen-driven `working` whose busy line
/// is still drawn counts as stalled (a live turn redraws its timer).
pub const QUIET_AFTER: Duration = Duration::from_secs(5);

/// One look at the screen, with the status it is weighed against.
#[derive(Debug, Clone, Copy)]
pub struct Look<'a> {
    /// The session's status before seen tracking.
    pub status: AgentStatus,
    /// Whether the agent's integration (or the hub) set it, not the screen.
    pub reported: bool,
    /// The status's change count (one spell of it from the next).
    pub spell: u64,
    pub verdict: &'a Verdict,
    /// The manifest file the verdict came from.
    pub file: &'a str,
    pub now: Instant,
    /// When the session last wrote output.
    pub last_output: Option<Instant>,
}

/// A status the screen calls for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub status: AgentStatus,
    pub detail: Option<String>,
    pub source: StatusSource,
}

/// One agent session's screen bookkeeping.
#[derive(Debug)]
pub struct ScreenTracker {
    /// The integration reports turn starts and dialogs (Claude Code's
    /// hooks); Codex's reports only finished turns.
    hooks_cover_turns: bool,
    spell: Option<u64>,
    /// This spell has shown a running turn or a dialog.
    marker_seen: bool,
    /// This (blocked) spell has shown its dialog.
    dialog_seen: bool,
    /// The running turn was on screen as this tracker caused the next
    /// spell (an approval): that spell starts having seen it.
    carry_marker: bool,
    /// A change waiting for the screen to hold: target and since when.
    pending: Option<(AgentStatus, Instant)>,
    /// A screen-driven `working` is being watched for a stall.
    watch_quiet: bool,
    /// A screen-driven turn stalled (its busy line stayed drawn with no
    /// output): the session's last output then. The screen starts a turn
    /// again only after newer output, so a frozen busy line cannot flip
    /// `working` / `unknown` forever.
    stalled: Option<Option<Instant>>,
}

impl ScreenTracker {
    pub fn new(kind: SessionKind) -> Self {
        Self {
            hooks_cover_turns: kind == SessionKind::Claude,
            spell: None,
            marker_seen: false,
            dialog_seen: false,
            carry_marker: false,
            pending: None,
            watch_quiet: false,
            stalled: None,
        }
    }

    /// Whether the screen may start a turn (`working`): not from the busy
    /// line of a turn that stalled, until the session outputs again.
    fn may_work(&mut self, look: &Look<'_>) -> bool {
        match self.stalled {
            Some(at) if look.last_output <= at => false,
            _ => {
                self.stalled = None;
                true
            }
        }
    }

    /// Whether the next look may change something with no new output
    /// (keep looking).
    pub fn pending(&self) -> bool {
        self.pending.is_some() || self.watch_quiet
    }

    /// Weigh one look; `Some` when the status should change.
    pub fn observe(&mut self, look: Look<'_>) -> Option<Decision> {
        if self.spell != Some(look.spell) {
            self.spell = Some(look.spell);
            self.marker_seen = std::mem::take(&mut self.carry_marker);
            self.dialog_seen = false;
            self.pending = None;
        }
        self.watch_quiet = false;
        let state = look.verdict.state;
        if state == Some(ScreenState::Hold) {
            self.pending = None;
            return None;
        }
        let now = |status| Some(manifest(look, status));
        use AgentStatus::{Blocked, Done, Idle, Unknown, Working};
        use ScreenState as S;
        match (look.status, look.reported) {
            (Unknown, _) => match state {
                Some(S::Blocked) => now(Blocked),
                Some(S::Idle) => now(Idle),
                Some(S::Working) if !self.hooks_cover_turns && self.may_work(&look) => now(Working),
                _ => None,
            },
            // The screen set this status: it may change it.
            (Working | Blocked, false) => match state {
                Some(S::Blocked) if look.status == Working => now(Blocked),
                Some(S::Working) if look.status == Blocked => {
                    if self.may_work(&look) {
                        now(Working)
                    } else {
                        None
                    }
                }
                Some(S::Blocked) => {
                    self.pending = None;
                    None
                }
                Some(S::Working) => {
                    self.pending = None;
                    let quiet = look
                        .last_output
                        .is_none_or(|at| look.now.saturating_duration_since(at) >= QUIET_AFTER);
                    if quiet {
                        self.stalled = Some(look.last_output);
                        return Some(Decision {
                            status: Unknown,
                            detail: None,
                            source: StatusSource::Heuristic {
                                reason: format!(
                                    "the busy line is drawn but nothing was output for {} s",
                                    QUIET_AFTER.as_secs()
                                ),
                            },
                        });
                    }
                    self.watch_quiet = true;
                    None
                }
                Some(S::Idle) => self.settle(look, Idle, || manifest(look, Idle)),
                Some(S::Hold) => None,
                None => self.settle(look, Unknown, || Decision {
                    status: Unknown,
                    detail: None,
                    source: StatusSource::Heuristic {
                        reason: format!(
                            "no rule matched the screen for {} s",
                            SETTLE_AFTER.as_secs()
                        ),
                    },
                }),
            },
            (Idle, false) => match state {
                Some(S::Blocked) => now(Blocked),
                Some(S::Working) if !self.hooks_cover_turns && self.may_work(&look) => now(Working),
                _ => None,
            },
            // A report set this status: fill only what reports miss.
            (Working | Blocked, true) => match state {
                Some(S::Working) => {
                    if look.status == Blocked && self.dialog_seen {
                        self.dialog_seen = false;
                        self.carry_marker = true;
                        return now(Working);
                    }
                    self.marker_seen = true;
                    self.pending = None;
                    None
                }
                Some(S::Blocked) => {
                    self.marker_seen = true;
                    if look.status == Blocked {
                        self.dialog_seen = true;
                        self.pending = None;
                        return None;
                    }
                    self.settle(look, Blocked, || manifest(look, Blocked))
                }
                Some(S::Idle) | None if self.marker_seen => {
                    let decision = match state {
                        Some(_) => manifest(look, Idle),
                        None => Decision {
                            status: Idle,
                            detail: None,
                            source: StatusSource::Heuristic {
                                reason: format!(
                                    "the running turn and its dialogs left the screen {} s ago with no report (interrupted)",
                                    SETTLE_AFTER.as_secs()
                                ),
                            },
                        },
                    };
                    self.settle(look, Idle, || decision)
                }
                _ => None,
            },
            (Done | Idle, true) | (Done, false) if !self.hooks_cover_turns => match state {
                Some(S::Working) if self.may_work(&look) => now(Working),
                Some(S::Blocked) => now(Blocked),
                _ => None,
            },
            _ => None,
        }
    }

    /// Call for `target` once the screen has asked for it for
    /// [`SETTLE_AFTER`] in a row.
    fn settle(
        &mut self,
        look: Look<'_>,
        target: AgentStatus,
        decision: impl FnOnce() -> Decision,
    ) -> Option<Decision> {
        match self.pending {
            Some((pending, since)) if pending == target => {
                if look.now.saturating_duration_since(since) < SETTLE_AFTER {
                    return None;
                }
                self.pending = None;
                self.marker_seen = false;
                Some(decision())
            }
            _ => {
                self.pending = Some((target, look.now));
                None
            }
        }
    }
}

/// The verdict's rule as the reason for `status`.
fn manifest(look: Look<'_>, status: AgentStatus) -> Decision {
    Decision {
        status,
        detail: (status == AgentStatus::Blocked)
            .then(|| look.verdict.detail.clone())
            .flatten(),
        source: StatusSource::Manifest {
            file: look.file.to_owned(),
            rule: look.verdict.rule.clone().unwrap_or_default(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(state: Option<ScreenState>) -> Verdict {
        Verdict {
            state,
            rule: state.map(|s| format!("r_{}", s.as_str())),
            detail: (state == Some(ScreenState::Blocked)).then(|| "may I?".to_owned()),
            evidence: Vec::new(),
        }
    }

    struct Run {
        tracker: ScreenTracker,
        t0: Instant,
        status: AgentStatus,
        reported: bool,
        spell: u64,
    }

    impl Run {
        fn new(kind: SessionKind) -> Self {
            Self {
                tracker: ScreenTracker::new(kind),
                t0: Instant::now(),
                status: AgentStatus::Unknown,
                reported: false,
                spell: 0,
            }
        }

        /// A report changes the status.
        fn report(&mut self, status: AgentStatus) {
            self.status = status;
            self.reported = true;
            self.spell += 1;
        }

        /// Look at `state` at `ms` (output at that moment too); apply what
        /// it decides.
        fn look(&mut self, ms: u64, state: Option<ScreenState>) -> Option<AgentStatus> {
            self.look_quiet(ms, state, ms)
        }

        fn look_quiet(
            &mut self,
            ms: u64,
            state: Option<ScreenState>,
            output_ms: u64,
        ) -> Option<AgentStatus> {
            let verdict = verdict(state);
            let decision = self.tracker.observe(Look {
                status: self.status,
                reported: self.reported,
                spell: self.spell,
                verdict: &verdict,
                file: "x.toml",
                now: self.t0 + Duration::from_millis(ms),
                last_output: Some(self.t0 + Duration::from_millis(output_ms)),
            })?;
            self.status = decision.status;
            self.reported = false;
            self.spell += 1;
            Some(decision.status)
        }
    }

    const W: Option<ScreenState> = Some(ScreenState::Working);
    const B: Option<ScreenState> = Some(ScreenState::Blocked);
    const I: Option<ScreenState> = Some(ScreenState::Idle);
    const H: Option<ScreenState> = Some(ScreenState::Hold);
    const SETTLE: u64 = SETTLE_AFTER.as_millis() as u64;

    #[test]
    fn before_any_report_a_dialog_blocks_and_a_prompt_is_idle() {
        let mut claude = Run::new(SessionKind::Claude);
        assert_eq!(claude.look(0, None), None);
        // Claude's hooks own its turns: a busy screen alone is nothing.
        assert_eq!(claude.look(10, W), None);
        assert_eq!(claude.look(20, B), Some(AgentStatus::Blocked));
        // Trust accepted: the prompt box.
        assert_eq!(claude.look(30, I), None);
        assert_eq!(claude.look(30 + SETTLE, I), Some(AgentStatus::Idle));
        let mut codex = Run::new(SessionKind::Codex);
        assert_eq!(codex.look(0, I), Some(AgentStatus::Idle));
    }

    #[test]
    fn codex_screen_drives_working_and_blocked_and_notify_ends_turns() {
        let mut run = Run::new(SessionKind::Codex);
        assert_eq!(run.look(0, I), Some(AgentStatus::Idle));
        assert_eq!(run.look(100, W), Some(AgentStatus::Working));
        assert_eq!(run.look(200, B), Some(AgentStatus::Blocked));
        // Approved: the turn runs on at once.
        assert_eq!(run.look(300, W), Some(AgentStatus::Working));
        // notify: done; the composer afterwards changes nothing.
        run.report(AgentStatus::Done);
        assert_eq!(run.look(400, I), None);
        assert_eq!(run.look(400 + 2 * SETTLE, I), None);
        // The next turn, straight into an approval.
        assert_eq!(run.look(10_000, B), Some(AgentStatus::Blocked));
        // Declined (esc): back at the composer once it holds.
        assert_eq!(run.look(10_100, I), None);
        assert_eq!(run.look(10_100 + SETTLE - 1, I), None);
        assert_eq!(run.look(10_100 + SETTLE, I), Some(AgentStatus::Idle));
    }

    #[test]
    fn a_screen_driven_turn_that_stalls_or_turns_unrecognizable_is_unknown() {
        let mut run = Run::new(SessionKind::Codex);
        run.look(0, W);
        assert_eq!(run.status, AgentStatus::Working);
        // The busy line stays drawn with no output.
        assert_eq!(run.look_quiet(4_000, W, 0), None);
        assert!(run.tracker.pending());
        assert_eq!(
            run.look_quiet(QUIET_AFTER.as_millis() as u64, W, 0),
            Some(AgentStatus::Unknown)
        );
        // It stays unknown while the busy line holds with no output (no
        // working/unknown flapping), and runs again on fresh output.
        for ms in (5_250..20_000).step_by(250) {
            assert_eq!(run.look_quiet(ms, W, 0), None, "at {ms} ms");
        }
        assert_eq!(
            run.look_quiet(20_000, W, 19_900),
            Some(AgentStatus::Working)
        );
        // A dialog over a stalled line still blocks; approving it with no
        // output yet does not restart the turn from the stale line.
        let mut run = Run::new(SessionKind::Codex);
        run.look(0, W);
        run.look_quiet(QUIET_AFTER.as_millis() as u64, W, 0);
        assert_eq!(run.status, AgentStatus::Unknown);
        assert_eq!(run.look_quiet(6_000, B, 0), Some(AgentStatus::Blocked));
        assert_eq!(run.look_quiet(6_500, W, 0), None);
        assert_eq!(run.look_quiet(7_000, W, 6_900), Some(AgentStatus::Working));
        let mut run = Run::new(SessionKind::Codex);
        run.look(0, W);
        assert_eq!(run.look(100, None), None);
        // Something else flashes up: the wait starts over.
        assert_eq!(run.look(200, I), None);
        assert_eq!(run.look(300, None), None);
        assert_eq!(run.look(300 + SETTLE, None), Some(AgentStatus::Unknown));
        // Never blocked from an unknown screen.
        assert_ne!(run.status, AgentStatus::Blocked);
    }

    #[test]
    fn a_reported_turn_settles_only_after_its_marker_showed() {
        let mut run = Run::new(SessionKind::Claude);
        run.report(AgentStatus::Working);
        // The footer never showed: nothing settles however long.
        assert_eq!(run.look(0, I), None);
        assert_eq!(run.look(10 * SETTLE, I), None);
        run.report(AgentStatus::Working);
        assert_eq!(run.look(0, W), None);
        assert_eq!(run.look(100, I), None);
        // A transcript viewer holds the turn: the wait restarts after.
        assert_eq!(run.look(100 + SETTLE, H), None);
        assert_eq!(run.look(200 + SETTLE, I), None);
        assert_eq!(run.look(200 + 2 * SETTLE, I), Some(AgentStatus::Idle));
        // An unrecognized screen settles an interrupted turn too.
        run.report(AgentStatus::Working);
        run.look(0, W);
        run.look(100, None);
        assert_eq!(run.look(100 + SETTLE, None), Some(AgentStatus::Idle));
    }

    #[test]
    fn a_reported_dialog_approved_is_working_and_an_unreported_one_blocks() {
        let mut run = Run::new(SessionKind::Claude);
        run.report(AgentStatus::Blocked);
        assert_eq!(run.look(0, B), None);
        assert_eq!(run.look(100, W), Some(AgentStatus::Working));
        // The approval's turn starts having seen its footer: an Esc now
        // settles it.
        assert_eq!(run.look(200, I), None);
        assert_eq!(run.look(200 + SETTLE, I), Some(AgentStatus::Idle));
        // A blocked spell whose dialog never showed is not approved by a
        // busy screen (the footer may be from before the dialog).
        run.report(AgentStatus::Blocked);
        assert_eq!(run.look(0, W), None);
        // Reported working with a dialog the hooks never reported.
        run.report(AgentStatus::Working);
        assert_eq!(run.look(0, B), None);
        assert_eq!(run.look(SETTLE, B), Some(AgentStatus::Blocked));
        // After a finished turn, Claude's screen changes nothing.
        run.report(AgentStatus::Done);
        assert_eq!(run.look(0, B), None);
        assert_eq!(run.look(0, W), None);
    }
}
