//! The `history` and `show` verbs, including `show`'s commit-provenance
//! lookup (spec 00078) and the default redaction of decision records (spec
//! 00073, #10).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use flywheel_core::event_serde::{event_kind, event_payload, payload_from_value};
use flywheel_core::events::DomainEvent;
use flywheel_core::lifecycle::Status;
use flywheel_core::redaction::{Redactor, default_policy};
use flywheel_core::store::{AuditRecord, EventRecord, HarnessStore};

use crate::history::{
    HistoryQuery, RunDetail, TERMINAL_STATUSES, collect_history_rows, collect_run_detail,
    history_rows_to_json, render_history_text, render_run_detail_text, resolve_run_id,
    run_detail_to_json,
};
use crate::policy::WorkPolicy;

use super::args::{HistoryArgs, ShowArgs};
use super::select::git_capture;
use super::{WorkflowError, py_json_dumps_indent2, repo_root_for_tasks_dir};

/// The provenance trailer key `show` reads off a landed commit to resolve
/// its producing run (spec 00078, D-3). Mirrors the worktree layer's
/// stamping engine, which lives one layer up and cannot be imported here.
pub const TRAILER_KEY_RUN: &str = "Flywheel-Run";

/// List finished runs, one line per task (text) or as the `--json` array.
pub fn cmd_history(
    args: &HistoryArgs,
    store: &dyn HarnessStore,
    fallback_phases: &BTreeMap<String, String>,
    out: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let statuses: Vec<Status> = if args.status.is_empty() {
        TERMINAL_STATUSES.to_vec()
    } else {
        args.status
            .iter()
            .filter_map(|s| match s.as_str() {
                "done" => Some(Status::Done),
                "failed" => Some(Status::Failed),
                "failed_validation" => Some(Status::FailedValidation),
                _ => None,
            })
            .collect()
    };
    let mut query = HistoryQuery::default()
        .with_statuses(statuses)
        .with_limit(usize::try_from(args.limit).unwrap_or(0))
        .with_fallback_phases(fallback_phases.clone());
    if let Some(phase) = &args.phase {
        query = query.with_phase(phase.clone());
    }
    let rows = collect_history_rows(store, &query)?;
    if args.json {
        writeln!(
            out,
            "{}",
            py_json_dumps_indent2(&history_rows_to_json(&rows))
        )?;
        return Ok(0);
    }
    writeln!(out, "{}", render_history_text(&rows))?;
    Ok(0)
}

/// How a `show` argument resolved against git as a commit object.
///
/// `is_commit` is true iff the argument names a commit; `run_id` carries the
/// commit's `Flywheel-Run` trailer value when present, else `None` (an
/// un-attributed commit). Resolution is git-truth: the run id is read off
/// the trailer, never guessed from a nearby run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitProvenance {
    pub is_commit: bool,
    pub run_id: Option<String>,
}

/// The repo the commit lookup resolves a SHA against: an explicit
/// `--tasks-dir` wins, then the policy's directory source, else the cwd.
pub fn show_repo_root(tasks_dir: Option<&str>, policy: Option<&WorkPolicy>) -> PathBuf {
    if let Some(dir) = tasks_dir.filter(|d| !d.is_empty()) {
        return repo_root_for_tasks_dir(Path::new(dir));
    }
    if let Some(policy) = policy
        && policy.source_kind == "directory"
        && let Some(dir) = &policy.tasks_dir
    {
        return repo_root_for_tasks_dir(dir);
    }
    std::env::current_dir().unwrap_or_default()
}

/// Resolve `reference` to its producing run via its `Flywheel-Run` trailer:
/// `rev-parse` peels it to a commit (anything else is `is_commit=false`),
/// then `git show` extracts the trailer with git's own parser.
pub fn resolve_commit_provenance(repo_root: &Path, reference: &str) -> CommitProvenance {
    let (code, _) = git_capture(
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{reference}^{{commit}}"),
        ],
    );
    if code != 0 {
        return CommitProvenance {
            is_commit: false,
            run_id: None,
        };
    }
    let (code, out) = git_capture(
        repo_root,
        &[
            "show",
            "--no-patch",
            &format!("--format=%(trailers:key={TRAILER_KEY_RUN},valueonly)"),
            reference,
        ],
    );
    if code != 0 {
        return CommitProvenance {
            is_commit: false,
            run_id: None,
        };
    }
    let run_id = out
        .trim()
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned);
    CommitProvenance {
        is_commit: true,
        run_id,
    }
}

/// Pass each decision event's payload through `redactor` exactly as the
/// audit stream does (wrapped in an `EventRecord`, every string leaf
/// scrubbed) and rebuild the typed event, so the text and JSON views redact
/// identically.
fn redact_decisions(detail: &mut RunDetail, redactor: &dyn Redactor) {
    for event in &mut detail.decisions {
        let Ok(payload) = event_payload(&event.payload) else {
            continue;
        };
        let kind = event_kind(event);
        let record = AuditRecord::Event(EventRecord {
            run_id: event.run_id.clone(),
            ts: event.ts,
            kind: kind.to_owned(),
            payload,
            attempt_number: event.attempt_number,
            id: None,
            sequence: event.sequence,
        });
        let AuditRecord::Event(redacted) = redactor.redact(&record) else {
            continue;
        };
        if let Ok(rebuilt) = payload_from_value(kind, redacted.payload) {
            *event = DomainEvent {
                payload: rebuilt,
                ..event.clone()
            };
        }
    }
}

/// Show one run in full (text or `--json`). `ID` is a run id, a task id
/// (its latest run), or a landed commit whose `Flywheel-Run` trailer names
/// the run. Decision-record excerpts are redacted unless `--raw`.
pub fn cmd_show(
    args: &ShowArgs,
    store: &dyn HarnessStore,
    repo_root: &Path,
    fallback_phases: &BTreeMap<String, String>,
    out: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let arg = &args.run_or_task_id;
    let mut commit: Option<CommitProvenance> = None;
    let run_id = match resolve_run_id(store, arg)? {
        Some(run_id) => run_id,
        None => {
            let provenance = resolve_commit_provenance(repo_root, arg);
            if !provenance.is_commit {
                writeln!(out, "{arg}: no run or task with that id")?;
                return Ok(1);
            }
            let Some(run_id) = provenance.run_id.clone() else {
                writeln!(
                    out,
                    "{arg}: un-attributed commit (no {TRAILER_KEY_RUN} trailer)"
                )?;
                return Ok(1);
            };
            commit = Some(provenance);
            run_id
        }
    };
    let Some(mut detail) = collect_run_detail(store, &run_id, fallback_phases)? else {
        if commit.is_some() {
            writeln!(
                out,
                "{arg}: {TRAILER_KEY_RUN} names run {run_id}, which is not in the store"
            )?;
            return Ok(1);
        }
        writeln!(out, "{arg}: no run or task with that id")?;
        return Ok(1);
    };
    if !args.raw {
        redact_decisions(&mut detail, &default_policy(None));
    }
    if args.json {
        writeln!(
            out,
            "{}",
            py_json_dumps_indent2(&run_detail_to_json(&detail))
        )?;
        return Ok(0);
    }
    writeln!(out, "{}", render_run_detail_text(&detail))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use chrono::Duration;
    use flywheel_core::events::{
        DomainEventPayload, GateGraderReceipt, HeldOutGateEvaluated, Landed, LandingParked,
        LandingRedriven,
    };
    use flywheel_core::lifecycle::{Attempt, Lifecycle, Outcome};
    use flywheel_core::store::{AttemptStore, DomainEventStore, LifecycleStore};
    use flywheel_core::store_memory::InMemoryStore;
    use serde_json::Value;
    use std::process::Command;
    use tempfile::TempDir;

    const SECRET: &str = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123";
    const REDACTED: &str = "[REDACTED:github_token]";

    fn done_run(store: &InMemoryStore, task_id: &str, run_id: &str) -> Lifecycle {
        let mut lc = Lifecycle::new(task_id).with_run_id(run_id);
        lc.transition_to(Status::Ready, "", Some(t0())).unwrap();
        lc.transition_to(Status::Running, "", Some(t0() + Duration::seconds(1)))
            .unwrap();
        lc.transition_to(Status::Validating, "", Some(t0() + Duration::seconds(2)))
            .unwrap();
        lc.transition_to(Status::Done, "", Some(t0() + Duration::minutes(5)))
            .unwrap();
        store.create_lifecycle(&lc).unwrap();
        lc
    }

    fn append(
        store: &InMemoryStore,
        lc: &mut Lifecycle,
        ts: chrono::DateTime<chrono::Utc>,
        payload: DomainEventPayload,
    ) {
        let event = DomainEvent::new(&lc.run_id, ts, payload);
        *lc = store.append_domain_event(&event, lc.version).unwrap();
    }

    fn seed_decisions(store: &InMemoryStore, run_id: &str, excerpt: &str) {
        let mut lc = done_run(store, "t-decide", run_id);
        append(
            store,
            &mut lc,
            t0() + Duration::minutes(6),
            DomainEventPayload::HeldOutGateEvaluated(HeldOutGateEvaluated {
                outcome: "fail".into(),
                reason: "held-out oracle failed".into(),
                receipts: vec![GateGraderReceipt {
                    grader_name: Some("oracle-x".into()),
                    passed: false,
                    output_excerpt: excerpt.into(),
                }],
            }),
        );
        append(
            store,
            &mut lc,
            t0() + Duration::minutes(6) + Duration::seconds(1),
            DomainEventPayload::LandingParked(LandingParked {
                park_kind: "held-out-gate".into(),
                detail: "gate blocked the land".into(),
                ..LandingParked::default()
            }),
        );
        append(
            store,
            &mut lc,
            t0() + Duration::minutes(7),
            DomainEventPayload::LandingRedriven(LandingRedriven {
                result: "landed".into(),
                park_kind: "held-out-gate".into(),
            }),
        );
        append(
            store,
            &mut lc,
            t0() + Duration::minutes(7) + Duration::seconds(1),
            DomainEventPayload::Landed(Landed {
                strategy: "merge".into(),
                landed_ref: "abc123".into(),
                ..Landed::default()
            }),
        );
    }

    fn show(store: &InMemoryStore, repo: &Path, arg: &str, json: bool, raw: bool) -> (i32, String) {
        let args = ShowArgs {
            run_or_task_id: arg.into(),
            json,
            raw,
            ..ShowArgs::default()
        };
        let mut out = Vec::new();
        let code = cmd_show(&args, store, repo, &BTreeMap::new(), &mut out).unwrap();
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn show_json_surfaces_every_decision_in_ledger_order_and_redacts_by_default() {
        let store = InMemoryStore::new();
        seed_decisions(
            &store,
            "run-1",
            &format!("grader stderr near {SECRET} then more"),
        );
        let tmp = TempDir::new().unwrap();
        let (code, text) = show(&store, tmp.path(), "run-1", true, false);
        assert_eq!(code, 0);
        let payload: Value = serde_json::from_str(&text).unwrap();
        let kinds: Vec<&str> = payload["decisions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "held_out_gate_evaluated",
                "landing_parked",
                "landing_redriven",
                "landed"
            ]
        );
        let gate = &payload["decisions"][0];
        assert_eq!(gate["outcome"], "fail");
        assert_eq!(gate["reason"], "held-out oracle failed");
        let excerpt = gate["receipts"][0]["output_excerpt"].as_str().unwrap();
        assert!(
            !excerpt.contains(SECRET)
                && excerpt.contains(REDACTED)
                && excerpt.contains("grader stderr near")
        );
        assert!(!text.contains(SECRET));
        assert_eq!(payload["decisions"][1]["park_kind"], "held-out-gate");
        assert_eq!(payload["decisions"][2]["result"], "landed");
        assert_eq!(payload["decisions"][3]["landed_ref"], "abc123");
        // By task id resolves to the latest run.
        let (code, by_task) = show(&store, tmp.path(), "t-decide", true, false);
        assert_eq!(code, 0);
        assert_eq!(by_task, text);
        // --raw reveals the stored value verbatim.
        let (_, raw) = show(&store, tmp.path(), "run-1", true, true);
        assert!(raw.contains(SECRET) && !raw.contains(REDACTED));
    }

    #[test]
    fn show_text_renders_and_redacts_decisions_and_omits_the_empty_section() {
        let store = InMemoryStore::new();
        seed_decisions(&store, "run-1", &format!("stderr {SECRET} tail"));
        let tmp = TempDir::new().unwrap();
        let (code, text) = show(&store, tmp.path(), "run-1", false, false);
        assert_eq!(code, 0);
        assert!(text.contains("decisions:\n  gate      fail  held-out oracle failed\n      FAIL  oracle-x  stderr [REDACTED:github_token] tail\n  parked    held-out-gate  gate blocked the land\n  redriven  landed  (was held-out-gate)\n  landed    merge  abc123\n"));
        assert!(!text.contains(SECRET));
        let (_, raw) = show(&store, tmp.path(), "run-1", false, true);
        assert!(raw.contains(SECRET));

        let plain = InMemoryStore::new();
        done_run(&plain, "t-plain", "run-1");
        let (code, text) = show(&plain, tmp.path(), "run-1", false, false);
        assert_eq!(code, 0);
        assert!(!text.contains("decisions:"));
        let (_, json_text) = show(&plain, tmp.path(), "run-1", true, false);
        assert!(json_text.contains("\"decisions\": []"));
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Flywheel Test")
            .env("GIT_AUTHOR_EMAIL", "test@flywheel.invalid")
            .env("GIT_COMMITTER_NAME", "Flywheel Test")
            .env("GIT_COMMITTER_EMAIL", "test@flywheel.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn init_repo(repo: &Path) -> String {
        std::fs::create_dir_all(repo).unwrap();
        git(repo, &["init", "-q"]);
        git(repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("seed.txt"), "seed\n").unwrap();
        git(repo, &["add", "seed.txt"]);
        git(repo, &["commit", "-q", "-m", "seed the repo"]);
        git(repo, &["rev-parse", "HEAD"])
    }

    fn stamped_commit(repo: &Path, name: &str, run_id: &str) -> String {
        std::fs::write(repo.join(name), format!("content of {name}\n")).unwrap();
        git(repo, &["add", name]);
        git(
            repo,
            &[
                "commit",
                "-q",
                "-m",
                &format!(
                    "implement {name}\n\nFlywheel-Task: t-land\n{TRAILER_KEY_RUN}: {run_id}\nFlywheel-Phase: 00078-provenance"
                ),
            ],
        );
        git(repo, &["rev-parse", "HEAD"])
    }

    fn seed_landed(store: &InMemoryStore, run_id: &str, landed_ref: &str) {
        let mut lc = done_run(store, "t-land", run_id);
        append(
            store,
            &mut lc,
            t0() + Duration::minutes(6),
            DomainEventPayload::Landed(Landed {
                strategy: "merge".into(),
                landed_ref: landed_ref.into(),
                ..Landed::default()
            }),
        );
    }

    #[test]
    fn show_by_commit_sha_resolves_the_trailer_or_names_the_failure() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let human_sha = init_repo(&repo);
        let sha = stamped_commit(&repo, "feature.py", "run-1");
        let ghost = stamped_commit(&repo, "orphan.py", "run-ghost");
        let store = InMemoryStore::new();
        seed_landed(&store, "run-1", "cafef00d");

        let provenance = resolve_commit_provenance(&repo, &sha);
        assert_eq!(
            provenance,
            CommitProvenance {
                is_commit: true,
                run_id: Some("run-1".into())
            }
        );
        assert_eq!(
            resolve_commit_provenance(&repo, &human_sha),
            CommitProvenance {
                is_commit: true,
                run_id: None
            }
        );
        assert!(!resolve_commit_provenance(&repo, "not-a-run-task-or-commit").is_commit);

        let (code, by_sha) = show(&store, &repo, &sha, false, false);
        assert_eq!(code, 0);
        assert!(
            by_sha.contains("run-1") && by_sha.contains("landed") && by_sha.contains("cafef00d")
        );
        let (_, by_run) = show(&store, &repo, "run-1", false, false);
        assert_eq!(by_sha, by_run);

        let (code, out) = show(&store, &repo, &human_sha, false, false);
        assert_eq!(code, 1);
        assert_eq!(
            out,
            format!("{human_sha}: un-attributed commit (no Flywheel-Run trailer)\n")
        );

        let (code, out) = show(&store, &repo, &ghost, false, false);
        assert_eq!(code, 1);
        assert_eq!(
            out,
            format!("{ghost}: Flywheel-Run names run run-ghost, which is not in the store\n")
        );

        let (code, out) = show(&store, &repo, "not-a-run-task-or-commit", false, false);
        assert_eq!(code, 1);
        assert_eq!(
            out,
            "not-a-run-task-or-commit: no run or task with that id\n"
        );
    }

    #[test]
    fn history_renders_text_and_json_with_status_filters() {
        let store = InMemoryStore::new();
        let mut lc = done_run(&store, "alpha", "run-alpha");
        let mut attempt = Attempt::new(1, t0(), "run-alpha");
        attempt.ended_at = Some(t0() + Duration::minutes(4));
        attempt.outcome = Some(Outcome::Succeeded);
        attempt.input_tokens = 1000;
        attempt.iterations_completed = 1;
        attempt.turns = 3;
        attempt.total_cost_usd = 0.25;
        store.save_attempt("run-alpha", &attempt, None).unwrap();
        lc.attempts.push(attempt);
        store.update_lifecycle(&lc, lc.version).unwrap();
        let failed = walk(
            "beta",
            "run-beta",
            &[Status::Ready, Status::Running, Status::Failed],
            "boom",
        );
        store.create_lifecycle(&failed).unwrap();

        let mut out = Vec::new();
        let args = HistoryArgs::default();
        assert_eq!(
            cmd_history(&args, &store, &BTreeMap::new(), &mut out).unwrap(),
            0
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("alpha  done"));
        assert!(text.contains("beta   failed"));

        let mut out = Vec::new();
        let args = HistoryArgs {
            status: vec!["failed".into()],
            json: true,
            ..HistoryArgs::default()
        };
        assert_eq!(
            cmd_history(&args, &store, &BTreeMap::new(), &mut out).unwrap(),
            0
        );
        let payload: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(payload.as_array().unwrap().len(), 1);
        assert_eq!(payload[0]["latest"]["run_id"], "run-beta");

        let empty = InMemoryStore::new();
        let mut out = Vec::new();
        assert_eq!(
            cmd_history(&HistoryArgs::default(), &empty, &BTreeMap::new(), &mut out).unwrap(),
            0
        );
        assert_eq!(String::from_utf8(out).unwrap(), "(no finished runs)\n");
        let mut out = Vec::new();
        let args = HistoryArgs {
            json: true,
            ..HistoryArgs::default()
        };
        assert_eq!(
            cmd_history(&args, &empty, &BTreeMap::new(), &mut out).unwrap(),
            0
        );
        assert_eq!(String::from_utf8(out).unwrap(), "[]\n");
    }
}
