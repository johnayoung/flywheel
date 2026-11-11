//! The batched landing (spec 00089, D-1..D-4): the merge-lock holder drains
//! up to [`LANDING_BATCH_MAX`] live queue entries as one batch, stacks them
//! by rebase in queue order re-running each entry's command graders at its
//! stacked position, runs ONE standing verify on the stacked tip, and
//! fast-forwards the base through every member with per-commit trailers and
//! a shared batch id.
//!
//! - A failed tip (or a tip that cannot fast-forward) lands every drained
//!   entry one at a time in queue order through the full ladder, each
//!   against the real base at its turn (D-3): n+1 verifies for n entries.
//! - An entry that cannot stack (fails a clean rebase onto the stack, fails
//!   its command graders there, or would park before stacking) is deferred
//!   and settles through the one-at-a-time ladder after the batch (D-4).
//! - Held-out oracles are not re-run on a cleanly stacked tree (D-2).
//!
//! Every tree the base takes is the tree a passing standing verify ran in:
//! stamping is message-only, so the stamped tip's tree is the verified tip.

use super::*;
use crate::trailers::stamp_commit_messages_chained;
use std::collections::BTreeSet;

/// The built-in batch bound (D-1): no config key, no flag.
pub const LANDING_BATCH_MAX: usize = 4;

/// One entry stacked onto the batch.
struct StackMember {
    /// The entry as marked verifying (or as drained, before marking).
    entry: QueueEntry,
    phase: String,
    worktree: PathBuf,
    branch: String,
    /// The branch tip before stacking: restored when the batch does not
    /// land, so the one-at-a-time ladder sees the task's own commits.
    pre_tip: String,
    /// Whether stacking moved the branch (a rebase-rung landing) or it
    /// already sat on the base tip (a fast-forward-rung landing).
    rebased: bool,
}

/// The stack being built for one batch.
#[derive(Default)]
struct Stack {
    /// The landing base every member shares, and its sha at stack start.
    base: Option<(String, String)>,
    members: Vec<StackMember>,
    /// Run ids of the drained entries that could not stack.
    deferred: BTreeSet<String>,
}

impl GitWorktreeSubmitter {
    /// Under the merge lock: drain every live queued entry, up to
    /// [`LANDING_BATCH_MAX`] per batch in `queued_at` order, including
    /// entries other workers queued, recording each settlement for its
    /// owner. An entry whose owner no longer holds a live lease is dropped,
    /// never landed (D-7). Entries queued while a batch lands join the next
    /// batch of the same drain.
    pub(super) fn drain_queue(
        &self,
        queue: &LandingQueue,
        ticket: &LandingTicket,
    ) -> Result<(), SubmitError> {
        if let Err(err) = queue.prune_settlements(ticket.claims.as_ref(), (ticket.now)()) {
            self.log(format!(
                "landing queue: settlement prune failed ({err}); continuing"
            ));
        }
        let mut seen: BTreeSet<String> = BTreeSet::new();
        loop {
            let mut drained = Vec::new();
            for entry in queue.entries()? {
                if drained.len() == LANDING_BATCH_MAX {
                    break;
                }
                if !seen.insert(entry.run_id.clone()) {
                    continue;
                }
                if self.entry_is_landable_by_us(queue, ticket, &entry)? {
                    drained.push(entry);
                }
            }
            if drained.is_empty() {
                return Ok(());
            }
            self.land_batch(queue, ticket, drained)?;
        }
    }

    /// Whether `entry`'s owner holds a live lease. A lapsed owner's entry
    /// is dropped; an unreadable claim leaves the entry queued for its
    /// owner to land itself.
    fn entry_is_landable_by_us(
        &self,
        queue: &LandingQueue,
        ticket: &LandingTicket,
        entry: &QueueEntry,
    ) -> Result<bool, SubmitError> {
        match ticket.entry_is_live(entry) {
            Ok(true) => Ok(true),
            Ok(false) => {
                self.log(format!(
                    "landing queue: dropping {} (run {}): owner {} no longer holds a live lease",
                    entry.task_id, entry.run_id, entry.owner
                ));
                queue.drop_entry(entry)?;
                Ok(false)
            }
            Err(err) => {
                self.log(format!(
                    "landing queue: cannot read the claim of {} ({err}); leaving it queued",
                    entry.task_id
                ));
                Ok(false)
            }
        }
    }

    /// Land one drained batch. A lone entry, or a policy with no standing
    /// verify to share, lands one at a time exactly as an unbatched landing.
    fn land_batch(
        &self,
        queue: &LandingQueue,
        ticket: &LandingTicket,
        drained: Vec<QueueEntry>,
    ) -> Result<(), SubmitError> {
        if drained.len() == 1 || self.verify_command.is_none() {
            return self.land_one_at_a_time(queue, ticket, &drained, true);
        }
        let mut stack = Stack::default();
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.stack_and_verify(queue, &drained, &mut stack)
        }));
        let landed = match attempt {
            Ok(Ok(landed)) => landed,
            Ok(Err(err)) => {
                self.log(format!(
                    "landing batch: stacking failed ({}: {err}); landing its entries one at a time",
                    err.kind()
                ));
                None
            }
            Err(payload) => {
                self.log(format!(
                    "landing batch: stacking panicked ({}); landing its entries one at a time",
                    panic_message(payload.as_ref())
                ));
                None
            }
        };
        if let Some(stamped) = landed {
            // The base already advanced: settle every member (queue errors
            // propagate as for any settlement), then the deferred entries.
            self.settle_batch(queue, std::mem::take(&mut stack.members), &stamped)?;
            let rest: Vec<QueueEntry> = drained
                .into_iter()
                .filter(|e| stack.deferred.contains(&e.run_id))
                .collect();
            return self.land_one_at_a_time(queue, ticket, &rest, false);
        }
        self.unstack(queue, &stack.members);
        self.land_one_at_a_time(queue, ticket, &drained, false)
    }

    /// Settle `entries` in order through the full ladder, each a size-1
    /// batch against the base as it is at its turn. `fresh` marks entries
    /// whose liveness the caller just read.
    fn land_one_at_a_time(
        &self,
        queue: &LandingQueue,
        ticket: &LandingTicket,
        entries: &[QueueEntry],
        fresh: bool,
    ) -> Result<(), SubmitError> {
        for entry in entries {
            if !fresh && !self.entry_is_landable_by_us(queue, ticket, entry)? {
                continue;
            }
            let outcome = self.land_entry(queue, entry)?;
            self.log_settled(entry, &outcome);
        }
        Ok(())
    }

    fn log_settled(&self, entry: &QueueEntry, outcome: &SubmitOutcome) {
        let verdict = match outcome {
            SubmitOutcome::Landed { .. } => "landed",
            SubmitOutcome::Parked { .. } => "parked",
            SubmitOutcome::NoOp { .. } => "no-op",
        };
        self.log(format!(
            "landing queue: settled {} (run {}, owner {}): {verdict}",
            entry.task_id, entry.run_id, entry.owner
        ));
    }

    /// Stack `drained`, verify the tip once, and on a pass fast-forward the
    /// base through every member, recording and settling each. Returns
    /// the stamped tip of every member once the base fast-forwarded through
    /// them; `None` leaves every member for the caller to unstack and land
    /// one at a time. Fewer than two stackable entries is not a batch:
    /// `None`, and serial landing settles them.
    fn stack_and_verify(
        &self,
        queue: &LandingQueue,
        drained: &[QueueEntry],
        stack: &mut Stack,
    ) -> Result<Option<Vec<String>>, SubmitError> {
        for entry in drained {
            match self.stack_entry(entry, stack) {
                Ok(()) => {}
                Err(why) => {
                    self.log(format!(
                        "landing batch: deferring {} (run {}) to a one-at-a-time landing after \
                         the batch: {why}",
                        entry.task_id, entry.run_id
                    ));
                    stack.deferred.insert(entry.run_id.clone());
                }
            }
        }
        if stack.members.len() < 2 {
            self.log(format!(
                "landing batch: {} of {} entries stacked; landing one at a time",
                stack.members.len(),
                drained.len()
            ));
            return Ok(None);
        }
        let Some((base, base_sha)) = stack.base.clone() else {
            return Ok(None);
        };

        let id = fresh_batch_id();
        let size = u32::try_from(stack.members.len()).unwrap_or(u32::MAX);
        for (i, member) in stack.members.iter_mut().enumerate() {
            let batch = LandedBatch {
                id: id.clone(),
                index: u32::try_from(i + 1).unwrap_or(u32::MAX),
                size,
            };
            member.entry = queue.mark_verifying(&member.entry, batch)?;
        }

        // ONE standing verify, with the stacked tip's exact tree as cwd.
        let tip = stack.members.last().expect("two or more members");
        self.log(format!(
            "landing batch {id}: standing verify on the stacked tip of {size} entries ({})",
            stack
                .members
                .iter()
                .map(|m| m.entry.task_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let verify = self.standing_verify(&tip.entry.request, &tip.worktree);
        if !verify.passed {
            self.log(format!(
                "landing batch {id}: the stacked tip failed the standing verify; landing its \
                 entries one at a time in queue order"
            ));
            return Ok(None);
        }

        let land_ref = self.materialize_landing_base(&tip.phase, &base)?;
        let land_sha = git_checked(&self.repo_root, &["rev-parse", &land_ref])?
            .trimmed()
            .to_owned();
        if land_sha != base_sha {
            self.log(format!(
                "landing batch {id}: {land_ref} moved during the batch ({base_sha} -> \
                 {land_sha}); landing its entries one at a time"
            ));
            return Ok(None);
        }

        // Stamp each member's own commits with its own trailers, chaining
        // each link onto the previous member's stamped tip. Message-only:
        // the final tip's tree is the verified tip's tree.
        let mut remap = BTreeMap::new();
        let mut link_base = base_sha.clone();
        let mut stamped = Vec::with_capacity(stack.members.len());
        for member in &stack.members {
            let req = &member.entry.request;
            let old_tip = git_checked(&member.worktree, &["rev-parse", "HEAD"])?
                .trimmed()
                .to_owned();
            let new_tip = stamp_commit_messages_chained(
                &self.repo_root,
                &link_base,
                &old_tip,
                &provenance_trailers(&req.task_id, &req.run_id, &member.phase),
                &mut remap,
            )?;
            link_base = old_tip;
            stamped.push(new_tip);
        }
        for (member, new_tip) in stack.members.iter().zip(&stamped) {
            git_checked(&member.worktree, &["reset", "--hard", new_tip])?;
        }
        let tip_branch = &stack.members.last().expect("two or more members").branch;
        if !self.ff_merge(tip_branch, &land_ref)? {
            self.log(format!(
                "landing batch {id}: cannot fast-forward {land_ref} to the stacked tip; landing \
                 its entries one at a time"
            ));
            return Ok(None);
        }
        self.log(format!(
            "landing batch {id}: merged {size} entries into {land_ref}"
        ));

        Ok(Some(stamped))
    }

    /// Record, tear down, and settle every member of a batch that landed.
    fn settle_batch(
        &self,
        queue: &LandingQueue,
        members: Vec<StackMember>,
        stamped: &[String],
    ) -> Result<(), SubmitError> {
        for (member, new_tip) in members.iter().zip(stamped) {
            let rung = if member.rebased {
                RUNG_REBASE
            } else {
                RUNG_FAST_FORWARD
            };
            self.record_landing(
                &member.entry.run_id,
                &Landed {
                    strategy: LANDING_STRATEGY_MERGE.to_owned(),
                    landed_ref: new_tip.clone(),
                    rung: rung.to_owned(),
                    agent_turns: None,
                    agent_wall_seconds: None,
                    batch: member.entry.batch.clone(),
                },
            );
            if let Err(err) = self.teardown_on_done(&member.worktree, &member.branch) {
                self.log(format!(
                    "landing batch: {} landed but its worktree teardown failed: {err}",
                    member.entry.task_id
                ));
            }
            let outcome = SubmitOutcome::landed(LANDING_STRATEGY_MERGE, new_tip.clone());
            queue.settle(&member.entry, &outcome)?;
            self.log_settled(&member.entry, &outcome);
        }
        Ok(())
    }

    /// Stack one entry onto `stack`, or refuse it with the reason it must
    /// be deferred (its worktree and branch left exactly as found).
    fn stack_entry(&self, entry: &QueueEntry, stack: &mut Stack) -> Result<(), String> {
        let req = &entry.request;
        let defer = |what: &str, err: &dyn fmt::Display| format!("{what}: {err}");
        let phase = self.phase_of(req.task_file.as_deref());
        let worktree = self.worktree(&req.task_id);
        let branch = self.branch(&req.task_id, &phase);
        let base = self
            .landing_base_for(&phase)
            .map_err(|e| defer("cannot resolve its landing base", &e))?;
        let base_sha = match &stack.base {
            Some((group_base, sha)) if *group_base == base => sha.clone(),
            Some((group_base, _)) => {
                return Err(format!(
                    "it lands onto {base}, not the batch's {group_base}"
                ));
            }
            None => git_checked(&self.repo_root, &["rev-parse", &base])
                .map_err(|e| defer("cannot read its landing base", &e))?
                .trimmed()
                .to_owned(),
        };
        // Would it park (or no-op) before stacking? Then the ladder owns it.
        let verdict = self
            .landability(req)
            .map_err(|e| defer("cannot read its worktree", &e))?;
        if !verdict.landable {
            return Err(verdict.reason);
        }
        let violations = self
            .protected_violations(&branch, &base)
            .map_err(|e| defer("cannot check protected paths", &e))?;
        if !violations.is_empty() {
            return Err(format!(
                "it touches protected path(s) {}",
                violations.join(", ")
            ));
        }

        let pre_tip = git_checked(&worktree, &["rev-parse", "HEAD"])
            .map_err(|e| defer("cannot read its branch tip", &e))?
            .trimmed()
            .to_owned();
        let stack_tip = match stack.members.last() {
            Some(prev) => git_checked(&prev.worktree, &["rev-parse", "HEAD"])
                .map_err(|e| defer("cannot read the stack tip", &e))?
                .trimmed()
                .to_owned(),
            None => base_sha.clone(),
        };
        let on_tip = self
            .is_ancestor(&stack_tip, &pre_tip)
            .map_err(|e| defer("cannot compare with the stack tip", &e))?;
        let rebased = !on_tip;
        if rebased {
            let rebase = git(&worktree, &["rebase", &stack_tip])
                .map_err(|e| defer("cannot rebase onto the stack", &e))?;
            if !rebase.ok() {
                let _ = git(&worktree, &["rebase", "--abort"]);
                self.restore_tip(&worktree, &pre_tip);
                return Err(format!(
                    "it does not rebase cleanly onto the stack ({stack_tip})"
                ));
            }
        }
        // Rebase-rung parity (D-2): its own command graders at its stacked
        // position. A first member already on the base tip is the clean-FF
        // tree its run verified, exactly as the unbatched ladder.
        if rebased || !stack.members.is_empty() {
            match self.reverify(req, &worktree) {
                Ok(check) if check.passed => {}
                Ok(_) => {
                    self.restore_tip(&worktree, &pre_tip);
                    return Err("its command graders fail at its stacked position".to_owned());
                }
                Err(err) => {
                    self.restore_tip(&worktree, &pre_tip);
                    return Err(defer("its command graders could not run", &err));
                }
            }
        }
        if stack.base.is_none() {
            stack.base = Some((base, base_sha));
        }
        stack.members.push(StackMember {
            entry: entry.clone(),
            phase,
            worktree,
            branch,
            pre_tip,
            rebased,
        });
        Ok(())
    }

    /// Return every member that did not land to its pre-stack tip and to
    /// the waiting state, so its one-at-a-time turn sees only its own work.
    fn unstack(&self, queue: &LandingQueue, members: &[StackMember]) {
        for member in members {
            self.restore_tip(&member.worktree, &member.pre_tip);
            if member.entry.is_verifying()
                && let Err(err) = queue.unmark(&member.entry)
            {
                self.log(format!(
                    "landing batch: cannot return {} to the queue's waiting state: {err}",
                    member.entry.task_id
                ));
            }
        }
    }

    fn restore_tip(&self, worktree: &Path, pre_tip: &str) {
        if let Err(err) = git_checked(worktree, &["reset", "--hard", pre_tip]) {
            self.log(format!(
                "landing batch: cannot restore {} to {pre_tip}: {err}",
                worktree.display()
            ));
        }
    }
}
