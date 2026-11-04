//! The board's actions: re-grade, ask to fix, pick the winner, rebase,
//! clean up the losers.

use super::{
    CleanupItem, CleanupReport, CleanupRequest, Fanouts, FixPlan, PickPlan, PickReport,
    RebaseReport, conflict, invalid, planned_merge, short_sha,
};
use crate::fanout::facts::{self};
use crate::fanout::{Candidate, Cleaned, Fanout, FanoutView, Trigger, Winner, fix_prompt};
use crate::hub::{Hub, HubError, HubEvent, RemoveOptions};
use crate::review::{Action, MergeVia, Plan};
use crate::state::StateError;
use chrono::Utc;

impl Fanouts {
    /// Grade `label` again now.
    pub fn grade(&self, id: &str, label: &str) -> Result<FanoutView, HubError> {
        let hub = self.hub()?;
        let (_, candidate) = self.get(&hub, id, label)?;
        if candidate.cleaned.is_some() {
            return Err(conflict(format!("{label}'s worktree was removed")));
        }
        if self.runner.is_none() {
            return Err(conflict("this hub cannot grade (no grader runner)"));
        }
        if !self.queue(&hub, id, label, Trigger::Manual)? {
            return Err(conflict(format!("{label} is being graded already")));
        }
        self.view(id)
    }

    /// The prompt asking `label`'s agent to fix what failed; with `run`,
    /// paste it (then Enter). The agent's next finished turn is graded.
    pub fn fix(&self, id: &str, label: &str, run: bool) -> Result<FixPlan, HubError> {
        let hub = self.hub()?;
        let (fanout, candidate) = self.get(&hub, id, label)?;
        let grade = candidate
            .grade
            .as_ref()
            .ok_or_else(|| conflict(format!("{label} has not been graded")))?;
        if grade.operator {
            return Err(conflict(format!(
                "{label} could not be graded, and that is not its agent's to fix: {}",
                grade.error.as_deref().unwrap_or("grading failed")
            )));
        }
        let prompt =
            fix_prompt(grade).ok_or_else(|| conflict(format!("{label} passes: nothing to fix")))?;
        let session = hub.session(&candidate.session_id)?;
        if session.record.state.is_running() && !session.record.runs_agent() {
            return Err(conflict(format!(
                "{label}'s agent did not come back after the hub restarted ({} runs a plain shell \
                 now): relaunch the agent from its session, then ask again",
                session.record.name
            )));
        }
        if !session.record.runs_agent() || !session.record.state.is_running() {
            return Err(conflict(format!(
                "{label}'s agent ({}) is not running: relaunch it first",
                session.record.name
            )));
        }
        let mut plan = FixPlan {
            candidate: label.to_owned(),
            session_id: candidate.session_id.clone(),
            session: session.record.name.clone(),
            prompt,
            paste: None,
        };
        if run {
            let pasted = hub.paste(&candidate.session_id, &plan.prompt, true)?;
            plan.paste = Some(pasted);
            let _ = hub.store().update_fanout(id, |f| {
                if let Some(c) = f.candidate_mut(label) {
                    c.fixes += 1;
                }
            });
            eprintln!(
                "fw hub: fan-out {} {label}: asked {} to fix: {}",
                fanout.name,
                session.record.name,
                grade.failure().unwrap_or_default()
            );
            hub.emit(HubEvent::Fanout { id: id.to_owned() });
        }
        Ok(plan)
    }

    /// Why `candidate` cannot be picked now (`None`: it can), reading its
    /// worktree afresh.
    fn pick_refusal(&self, fanout: &Fanout, candidate: &Candidate) -> Option<String> {
        if let Some(winner) = &fanout.winner {
            return Some(format!("{} was picked already", winner.label));
        }
        if candidate.cleaned.is_some() {
            return Some("its worktree was removed".into());
        }
        if candidate.pending.is_some() {
            return Some("a grading is queued or running: wait for it".into());
        }
        let Some(grade) = &candidate.grade else {
            return Some("it has not been graded".into());
        };
        if let Some(why) = grade.failure() {
            return Some(format!("it does not pass: {why}"));
        }
        if !grade.clean() {
            return Some(
                "it was graded with uncommitted changes: commit them (Review, Commit all), then \
                 re-grade"
                    .into(),
            );
        }
        if grade.no_change() {
            return Some(format!(
                "it changes nothing against {}: there is nothing to merge",
                fanout.base
            ));
        }
        if let Some(why) = self.staleness(candidate, grade, None) {
            return Some(format!("{why}: re-grade it first"));
        }
        // The merge lands the candidate's branch: it must be the commit
        // that passed, checked out in its worktree.
        let dir = &candidate.worktree.path;
        let branch = &candidate.worktree.branch;
        match facts::current_branch(&self.tools, dir) {
            Some(on) if on == *branch => {}
            on => {
                return Some(format!(
                    "its worktree is on {}, not its fan-out branch {branch}: what passed is not \
                     what would be merged (git switch {branch} there, then re-grade)",
                    on.as_deref().unwrap_or("a detached HEAD")
                ));
            }
        }
        let tip = facts::resolve_commit(&self.tools, dir, &format!("refs/heads/{branch}"));
        if tip.is_none() || tip != grade.head {
            return Some(format!(
                "branch {branch} is at {}, not the commit that passed ({}): re-grade it first",
                tip.as_deref().map_or("nothing", short_sha),
                grade.head.as_deref().map_or("none", short_sha)
            ));
        }
        if let Some(why) = super::pins_broken(&fanout.pins) {
            return Some(why);
        }
        None
    }

    /// The merge plan's refusal when it would not land exactly `head`.
    fn merges_graded(merge: &Plan, head: Option<&str>) -> Option<String> {
        let planned = planned_merge(merge);
        if planned.is_some() && planned == head {
            return None;
        }
        Some(format!(
            "the merge would land {}, not the commit that passed ({}): re-grade it first",
            planned.map_or("nothing", short_sha),
            head.map_or("none", short_sha)
        ))
    }

    fn losers(&self, hub: &Hub, fanout: &Fanout, winner: &str) -> Vec<CleanupItem> {
        fanout
            .candidates
            .iter()
            .filter(|c| c.label != winner && c.cleaned.is_none())
            .map(|c| self.cleanup_one(hub, fanout, c, true, false, true))
            .collect()
    }

    /// What merging `label` into the base would run, or why it cannot.
    pub fn pick_plan(
        &self,
        id: &str,
        label: &str,
        include_suspects: bool,
    ) -> Result<PickPlan, HubError> {
        let hub = self.hub()?;
        let (fanout, candidate) = self.get(&hub, id, label)?;
        let mut plan = PickPlan {
            fanout: fanout.id.clone(),
            candidate: label.to_owned(),
            session_id: candidate.session_id.clone(),
            refusal: self.pick_refusal(&fanout, &candidate),
            needs_rebase: false,
            merge: None,
            losers: self.losers(&hub, &fanout, label),
            warnings: promoted_warnings(&fanout, label),
        };
        if plan.refusal.is_some() {
            return Ok(plan);
        }
        if hub.session(&candidate.session_id).is_err() {
            plan.refusal = Some("its session was removed: merge it from the main checkout".into());
            return Ok(plan);
        }
        let merge = hub.review_plan_via(
            &self.tools,
            &candidate.session_id,
            &Action::Merge { include_suspects },
            MergeVia::FanoutPick,
        )?;
        plan.needs_rebase = merge.needs_rebase;
        let graded = candidate.grade.as_ref().and_then(|g| g.head.as_deref());
        plan.refusal = merge
            .refusal
            .clone()
            .or_else(|| self.pick_refusal(&fanout, &candidate))
            // The plan merges the commit it read: the one that passed?
            .or_else(|| Self::merges_graded(&merge, graded));
        plan.merge = Some(merge);
        Ok(plan)
    }

    /// Merge `label` (the plan with fingerprint `confirm`) and record it
    /// the winner.
    pub fn pick(
        &self,
        id: &str,
        label: &str,
        confirm: &str,
        include_suspects: bool,
    ) -> Result<PickReport, HubError> {
        let hub = self.hub()?;
        let (fanout, candidate) = self.get(&hub, id, label)?;
        if let Some(why) = self.pick_refusal(&fanout, &candidate) {
            return Err(conflict(format!("cannot pick {label}: {why}")));
        }
        let head = candidate
            .grade
            .as_ref()
            .and_then(|g| g.head.clone())
            .ok_or_else(|| conflict(format!("{label} has no commit to merge")))?;
        // `confirm` must be a plan that merges exactly the graded commit: the
        // run below re-plans and refuses any other fingerprint, and equal
        // fingerprints plan the same commit.
        let planned = hub.review_plan_via(
            &self.tools,
            &candidate.session_id,
            &Action::Merge { include_suspects },
            MergeVia::FanoutPick,
        )?;
        if planned.fingerprint != confirm {
            return Err(conflict(format!(
                "{label}'s merge plan changed since it was shown: look again (fw hub fanout pick \
                 without --yes, or reopen the dialog)"
            )));
        }
        if let Some(why) = Self::merges_graded(&planned, Some(&head)) {
            return Err(conflict(format!("cannot pick {label}: {why}")));
        }
        let merge = hub.review_run_via(
            &self.tools,
            &candidate.session_id,
            &Action::Merge { include_suspects },
            confirm,
            MergeVia::FanoutPick,
        )?;
        if merge.outcome.ok {
            let winner = Winner {
                label: label.to_owned(),
                head: merge.outcome.head.clone().unwrap_or(head),
                base: fanout.base.clone(),
                at: Utc::now(),
            };
            hub.store().update_fanout(id, |f| f.winner = Some(winner))?;
            eprintln!(
                "fw hub: fan-out {} ({id}): picked {label}, merged into {}",
                fanout.name, fanout.base
            );
            hub.emit(HubEvent::Fanout { id: id.to_owned() });
        }
        let fanout = hub.store().fanout(id)?.clone();
        Ok(PickReport {
            losers: self.losers(&hub, &fanout, label),
            merge,
            fanout: self.view(id)?,
        })
    }

    /// Rebase `label` onto the base (dry run: the plan); a rebase that
    /// went through queues a grading of the new commits.
    pub fn rebase(
        &self,
        id: &str,
        label: &str,
        confirm: Option<&str>,
    ) -> Result<RebaseReport, HubError> {
        let hub = self.hub()?;
        let (fanout, candidate) = self.get(&hub, id, label)?;
        if let Some(winner) = &fanout.winner {
            return Err(conflict(format!("{} was picked already", winner.label)));
        }
        if candidate.cleaned.is_some() {
            return Err(conflict(format!("{label}'s worktree was removed")));
        }
        if candidate.pending.is_some() {
            return Err(conflict(format!(
                "{label} is being graded: wait for it to finish, then rebase"
            )));
        }
        let Some(confirm) = confirm else {
            let plan = hub.review_plan(&self.tools, &candidate.session_id, &Action::Rebase)?;
            return Ok(RebaseReport {
                plan: Some(plan),
                rebase: None,
                fanout: self.view(id)?,
            });
        };
        let report =
            hub.review_run(&self.tools, &candidate.session_id, &Action::Rebase, confirm)?;
        if report.outcome.ok {
            self.inner().trees.remove(&candidate.worktree.path);
            if !self.queue(&hub, id, label, Trigger::Rebase)? {
                // A grading was queued meanwhile (a finished turn): it may
                // read the tree from before the rebase, so grade again after.
                self.inner().again.insert(candidate.session_id.clone());
            }
        }
        Ok(RebaseReport {
            plan: None,
            rebase: Some(report),
            fanout: self.view(id)?,
        })
    }

    /// One candidate's cleanup: remove its worktree (and session), or stop
    /// its session; `dry_run` only says what would happen.
    fn cleanup_one(
        &self,
        hub: &Hub,
        fanout: &Fanout,
        candidate: &Candidate,
        remove: bool,
        force: bool,
        dry_run: bool,
    ) -> CleanupItem {
        let session = hub.session(&candidate.session_id).ok();
        let mut item = CleanupItem {
            candidate: candidate.label.clone(),
            session: session.as_ref().map(|s| s.record.name.clone()),
            path: candidate.worktree.path.clone(),
            branch: candidate.worktree.branch.clone(),
            action: if remove { "remove_worktree" } else { "stop" },
            done: false,
            refusal: None,
            error: None,
            branch_deleted: None,
        };
        if !remove {
            let running = session.as_ref().is_some_and(|s| {
                s.record.state.is_running() && s.live.as_ref().is_some_and(|l| l.pid.is_some())
            });
            if dry_run || !running {
                item.done = !dry_run;
                return item;
            }
            match hub.kill_session(&candidate.session_id) {
                Ok(()) => item.done = true,
                Err(e) => item.error = Some(e.to_string()),
            }
            return item;
        }
        // Unique work in a candidate's worktree is that candidate's attempt:
        // keep it, or let it go knowingly.
        let unique = || {
            if !candidate.worktree.path.is_dir() {
                return None;
            }
            match crate::git::removal_check(&candidate.worktree) {
                Ok(check) => {
                    check.refusal_advising("force the removal if that attempt is not wanted")
                }
                Err(e) => Some(e),
            }
        };
        let refusal = match &session {
            Some(_) => match hub.worktree_info(&candidate.session_id) {
                Ok(info) => info
                    .blocking
                    .first()
                    .map(|other| format!("session {other:?} also uses it"))
                    .or_else(|| info.refusal.and_then(|_| unique())),
                Err(e) => Some(e.to_string()),
            },
            None => unique(),
        };
        item.refusal = refusal.clone();
        if dry_run {
            return item;
        }
        if refusal.is_some() && !force {
            return item;
        }
        // Hanging up its agent must not grade a worktree being removed.
        self.inner().retiring.insert(candidate.session_id.clone());
        let removed = match &session {
            Some(_) => hub
                .remove_session_with(
                    &candidate.session_id,
                    RemoveOptions {
                        remove_worktree: true,
                        force,
                    },
                )
                .map(|outcome| outcome.worktree.map(|w| w.branch_deleted)),
            None if candidate.worktree.path.is_dir() => {
                crate::git::remove(&candidate.worktree, force)
                    .map(|r| Some(r.branch_deleted))
                    .map_err(HubError::Internal)
            }
            None => Ok(None),
        };
        match removed {
            Ok(branch_deleted) => {
                item.done = true;
                item.branch_deleted = branch_deleted;
                let cleaned = Cleaned {
                    at: Utc::now(),
                    branch_deleted: branch_deleted.unwrap_or(false),
                };
                let label = candidate.label.clone();
                let _ = hub.store().update_fanout(&fanout.id, |f| {
                    if let Some(c) = f.candidate_mut(&label) {
                        c.cleaned = Some(cleaned);
                        c.pending = None;
                    }
                });
                self.inner()
                    .queue
                    .retain(|(f, l)| !(f == &fanout.id && l == &candidate.label));
                eprintln!(
                    "fw hub: fan-out {} {}: removed worktree {}{}",
                    fanout.name,
                    candidate.label,
                    candidate.worktree.path.display(),
                    if force && refusal.is_some() {
                        " (forced)"
                    } else {
                        ""
                    }
                );
            }
            Err(e) => {
                self.inner().retiring.remove(&candidate.session_id);
                item.error = Some(e.to_string());
            }
        }
        item
    }

    /// Remove (or stop) the candidates `request` names -- by default every
    /// one but the winner.
    pub fn cleanup(&self, id: &str, request: &CleanupRequest) -> Result<CleanupReport, HubError> {
        let hub = self.hub()?;
        let fanout = hub.store().fanout(id)?.clone();
        let labels: Vec<String> = match &request.candidates {
            Some(labels) => {
                for label in labels {
                    if fanout.candidate(label).is_none() {
                        return Err(StateError::NotFound(format!("candidate {label}")).into());
                    }
                }
                labels.clone()
            }
            None => {
                let Some(winner) = &fanout.winner else {
                    return Err(invalid(
                        "no winner yet: pick one first, or name the candidates to clean up",
                    ));
                };
                fanout
                    .candidates
                    .iter()
                    .filter(|c| c.label != winner.label && c.cleaned.is_none())
                    .map(|c| c.label.clone())
                    .collect()
            }
        };
        let mut items = Vec::new();
        for label in &labels {
            let Some(candidate) = fanout.candidate(label) else {
                continue;
            };
            if candidate.cleaned.is_some() {
                continue;
            }
            items.push(self.cleanup_one(
                &hub,
                &fanout,
                candidate,
                request.remove_worktrees,
                request.force,
                request.dry_run,
            ));
        }
        if !request.dry_run {
            hub.emit(HubEvent::Fanout { id: id.to_owned() });
        }
        Ok(CleanupReport {
            items,
            fanout: self.view(id)?,
        })
    }
}

/// What picking `label` does to the fan-out's promoted flywheel tasks still
/// active: their work lands on the base outside them, so each one's worker
/// finds nothing to do and stops.
fn promoted_warnings(fanout: &Fanout, label: &str) -> Vec<String> {
    fanout
        .promoted
        .iter()
        .map(crate::fanout::Promotion::view)
        .filter(|p| p.state == crate::fanout::promote::PromotedState::Active)
        .map(|p| {
            format!(
                "this fan-out was promoted to flywheel task {id} ({path}): merging {label} puts \
                 its work on {base} outside that task, so its worker finds nothing left to do \
                 and stops. Pick only if you mean to drop the task (remove its file), or leave \
                 the landing to `fw worker`",
                id = p.promotion.task_id,
                path = p.promotion.path.display(),
                base = fanout.base,
            )
        })
        .collect()
}
