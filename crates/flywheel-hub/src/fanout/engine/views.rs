//! Views: worktree state and staleness, the ranked board, removal and
//! lookups the actions share.

use super::{Fanouts, LIST_TREE_CACHE, TREE_CACHE, session_map, session_state};
use crate::fanout::facts::{self, TreeState};
use crate::fanout::{
    Candidate, Counts, Fanout, FanoutView, Grade, Phase, Verdict, phase_of, rank, verdict_of,
};
use crate::hub::{Hub, HubError, HubEvent, SessionView};
use crate::state::StateError;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

impl Fanouts {
    /// A worktree's status, reused for `max_age`.
    fn tree(&self, dir: &Path, max_age: Duration) -> Result<TreeState, String> {
        if let Some((at, state)) = self.inner().trees.get(dir)
            && at.elapsed() < max_age
        {
            return state.clone();
        }
        let state = facts::tree_state(&self.tools, dir);
        self.inner()
            .trees
            .insert(dir.to_path_buf(), (Instant::now(), state.clone()));
        state
    }

    /// Why `grade` no longer describes the worktree (`None`: it does).
    /// `max_age`: how old a cached status read may be (`None`: read now).
    pub(crate) fn staleness(
        &self,
        candidate: &Candidate,
        grade: &Grade,
        max_age: Option<Duration>,
    ) -> Option<String> {
        if grade.moved {
            return Some("it changed while its graders ran".into());
        }
        let dir = &candidate.worktree.path;
        let now = match max_age {
            None => facts::tree_state(&self.tools, dir),
            Some(age) => self.tree(dir, age),
        };
        match now {
            Err(e) => Some(format!("its worktree cannot be read: {e}")),
            Ok(now) if now.fingerprint == grade.tree => None,
            Ok(now) if now.head != grade.head => Some("new commits since it was graded".into()),
            Ok(_) => Some("its working tree changed since it was graded".into()),
        }
    }

    /// Every fan-out, newest first, as summaries: no grader output (fetch
    /// one board for it) and worktree reads reused for [`LIST_TREE_CACHE`].
    pub fn list(&self) -> Result<Vec<FanoutView>, HubError> {
        let hub = self.hub()?;
        let fanouts: Vec<Fanout> = hub.store().fanouts().to_vec();
        let sessions = session_map(&hub);
        let workspaces = hub.workspaces();
        let mut views: Vec<FanoutView> = fanouts
            .iter()
            .map(|f| {
                let mut view = self.build(f, &sessions, &workspaces, LIST_TREE_CACHE);
                view.summarize();
                view
            })
            .collect();
        views.reverse();
        Ok(views)
    }

    /// One fan-out as stored.
    pub fn record(&self, id: &str) -> Result<Fanout, HubError> {
        Ok(self.hub()?.store().fanout(id)?.clone())
    }

    /// Note a task file written from fan-out `id`.
    pub fn note_promotion(&self, id: &str, promotion: super::Promotion) -> Result<(), HubError> {
        let hub = self.hub()?;
        hub.store()
            .update_fanout(id, |f| f.promoted.push(promotion))?;
        hub.emit(HubEvent::Fanout { id: id.to_owned() });
        Ok(())
    }

    /// One fan-out's board.
    pub fn view(&self, id: &str) -> Result<FanoutView, HubError> {
        let hub = self.hub()?;
        let fanout = hub.store().fanout(id)?.clone();
        Ok(self.build(&fanout, &session_map(&hub), &hub.workspaces(), TREE_CACHE))
    }

    pub(crate) fn build(
        &self,
        fanout: &Fanout,
        sessions: &HashMap<String, SessionView>,
        workspaces: &[crate::state::Workspace],
        tree_age: Duration,
    ) -> FanoutView {
        let mut views: Vec<super::CandidateView> = fanout
            .candidates
            .iter()
            .map(|c| {
                let session = sessions.get(&c.session_id);
                let stale = match (&c.grade, &c.pending, &c.cleaned) {
                    (Some(grade), None, None) => self.staleness(c, grade, Some(tree_age)),
                    _ => None,
                };
                let phase = phase_of(
                    c,
                    session.map(|s| {
                        (
                            session_state(&s.record.state),
                            s.record.runs_agent(),
                            s.agent.as_ref().map(|a| a.status),
                        )
                    }),
                );
                let verdict = verdict_of(c, stale.as_deref());
                let (cost_usd, tokens) = match session.and_then(|s| s.usage.as_ref()) {
                    Some(u) => (
                        u.total.priced.then_some(u.total.cost_usd),
                        u.total.total_tokens,
                    ),
                    None => (
                        c.grade.as_ref().and_then(|g| g.cost_usd),
                        c.grade.as_ref().map_or(0, |g| g.tokens),
                    ),
                };
                let head = if c.cleaned.is_some() {
                    None
                } else {
                    self.tree(&c.worktree.path, tree_age)
                        .ok()
                        .and_then(|t| t.head)
                };
                let winner = fanout.winner.as_ref().is_some_and(|w| w.label == c.label);
                super::CandidateView {
                    label: c.label.clone(),
                    session_id: c.session_id.clone(),
                    session: session.map(|s| s.record.name.clone()),
                    kind: c.kind,
                    model: c.launch.model.clone(),
                    branch: c.worktree.branch.clone(),
                    path: c.worktree.path.clone(),
                    phase,
                    verdict,
                    rank: 0,
                    why: Vec::new(),
                    agent: session.and_then(|s| s.agent.clone()),
                    session_state: session.map(|s| {
                        if s.record.state.is_running() && !s.record.runs_agent() {
                            // Restored without its agent (a fresh shell).
                            "shell".to_owned()
                        } else {
                            session_state(&s.record.state).to_owned()
                        }
                    }),
                    pickable: verdict == Verdict::Pass
                        && fanout.winner.is_none()
                        && session.is_some()
                        && c.grade
                            .as_ref()
                            .is_some_and(|g| g.clean() && !g.no_change()),
                    grade: c.grade.clone(),
                    stale,
                    pending: c.pending.clone(),
                    head,
                    cost_usd,
                    tokens,
                    gradings: c.gradings,
                    fixes: c.fixes,
                    winner,
                    cleaned: c.cleaned.clone(),
                }
            })
            .collect();
        rank(&mut views);
        let mut counts = Counts {
            candidates: views.len(),
            ..Counts::default()
        };
        for v in &views {
            match v.verdict {
                Verdict::Pass => counts.passing += 1,
                Verdict::Fail => counts.failing += 1,
                Verdict::Pending => counts.grading += 1,
                Verdict::Stale | Verdict::None => {}
            }
            if matches!(v.phase, Phase::Working | Phase::Blocked | Phase::Waiting) {
                counts.running += 1;
            }
        }
        let mut warnings = Vec::new();
        if let Some(noop) = views.iter().find(|v| {
            v.verdict == Verdict::Pass
                && v.grade.as_ref().is_some_and(|g| g.clean() && g.no_change())
        }) {
            warnings.push(format!(
                "the graders pass on {} unchanged ({} changes nothing and passes): they do not \
                 tell the candidates apart",
                fanout.base, noop.label
            ));
        }
        let settled = views.iter().all(|v| {
            matches!(v.verdict, Verdict::Pass | Verdict::Fail)
                || matches!(v.phase, Phase::Ended | Phase::Removed)
        });
        FanoutView {
            id: fanout.id.clone(),
            name: fanout.name.clone(),
            workspace_id: fanout.workspace_id.clone(),
            workspace: workspaces
                .iter()
                .find(|w| w.id == fanout.workspace_id)
                .map(|w| w.name.clone()),
            folder: fanout.repo.clone(),
            repo: fanout.repo.clone(),
            base: fanout.base.clone(),
            base_sha: fanout.base_sha.clone(),
            prompt: fanout.prompt.clone(),
            sent_prompt: fanout.sent_prompt(),
            graders: fanout.graders.clone(),
            timeout_secs: fanout.timeout_secs,
            protect: fanout.protect.clone(),
            created_at: fanout.created_at,
            winner: fanout.winner.clone(),
            leader: views
                .first()
                .filter(|v| v.verdict == Verdict::Pass)
                .map(|v| v.label.clone()),
            candidates: views,
            counts,
            settled,
            graded_at: fanout.graded_at(),
            needs_you: fanout.needs_you(),
            runner: self.runner_name(),
            warnings,
            summary: false,
            origin: fanout.origin.clone(),
            promoted: fanout.promoted.iter().map(super::Promotion::view).collect(),
        }
    }

    /// Forget a fan-out (its sessions and worktrees stay).
    pub fn remove(&self, id: &str) -> Result<(), HubError> {
        let hub = self.hub()?;
        let removed = hub.store().remove_fanout(id)?;
        {
            let mut inner = self.inner();
            inner.queue.retain(|(f, _)| f != id);
            inner.announced.remove(id);
        }
        eprintln!("fw hub: fan-out {} ({id}) forgotten", removed.name);
        hub.emit(HubEvent::Fanout { id: id.to_owned() });
        Ok(())
    }

    /// Someone looked at the board's grading of `graded_at` (the board's
    /// `graded_at` when they opened it): it no longer needs you, unless a
    /// later grading finished since. An earlier mark never undoes a later.
    pub fn seen(&self, id: &str, graded_at: chrono::DateTime<chrono::Utc>) -> Result<(), HubError> {
        let hub = self.hub()?;
        let changed = hub.store().update_fanout(id, |f| f.mark_seen(graded_at))?;
        if changed {
            hub.emit(HubEvent::Fanout { id: id.to_owned() });
        }
        Ok(())
    }

    pub(crate) fn get(
        &self,
        hub: &Hub,
        id: &str,
        label: &str,
    ) -> Result<(Fanout, Candidate), HubError> {
        let fanout = hub.store().fanout(id)?.clone();
        let candidate = fanout.candidate(label).cloned().ok_or_else(|| {
            HubError::from(StateError::NotFound(format!(
                "candidate {label} of fan-out {}",
                fanout.name
            )))
        })?;
        Ok((fanout, candidate))
    }
}
