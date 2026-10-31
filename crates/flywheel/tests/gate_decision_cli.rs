//! `fw approve` / `fw reject` tell the operator which manual-gate instance
//! the decision was bound to, and say nothing of the kind when refused.

use chrono::Utc;
use flywheel_core::lifecycle::{Attempt, Lifecycle, Outcome, Status};
use flywheel_core::store::{AttemptStore, ControlCommandStore, LifecycleStore};
use flywheel_core::store_sqlite::SqliteStore;
use std::path::Path;
use std::process::{Command, Output};

fn seed_parked(db: &Path, run_id: &str, ordinal: u32, attempts: u32) {
    let store = SqliteStore::open(db).unwrap();
    let now = Utc::now();
    let mut lc = Lifecycle::new("task-gated").with_run_id(run_id);
    for status in [
        Status::Ready,
        Status::Running,
        Status::Validating,
        Status::AwaitingApproval,
    ] {
        lc.transition_to(status, "", Some(now)).unwrap();
    }
    lc.awaiting_manual_ordinal = Some(ordinal);
    store.create_lifecycle(&lc).unwrap();
    for number in 1..=attempts {
        let mut attempt = Attempt::new(number, now, run_id);
        attempt.outcome = Some(Outcome::Succeeded);
        store.save_attempt(run_id, &attempt, None).unwrap();
    }
}

fn fw(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fw"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run fw")
}

fn queued(db: &Path, run_id: &str) -> Vec<(String, serde_json::Value)> {
    SqliteStore::open(db)
        .unwrap()
        .claim_commands(run_id, Utc::now())
        .unwrap()
        .into_iter()
        .map(|c| (c.kind, serde_json::Value::Object(c.payload)))
        .collect()
}

#[test]
fn a_decision_reports_the_gate_instance_it_was_bound_to() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("fw.sqlite");
    let db_arg = db.to_str().unwrap();
    seed_parked(&db, "run-gated", 3, 2);

    let approved = fw(&["approve", "run-gated", "--db", db_arg], tmp.path());
    let stderr = String::from_utf8_lossy(&approved.stderr);
    assert_eq!(approved.status.code(), Some(0), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&approved.stdout),
        "enqueued #1 kind=approve run_id=run-gated\n"
    );
    assert!(
        stderr.contains("bound to manual gate awaiting_ordinal=3 attempt_number=2"),
        "{stderr}"
    );
    assert_eq!(
        queued(&db, "run-gated"),
        vec![(
            "approve".to_owned(),
            serde_json::json!({"awaiting_ordinal": 3, "attempt_number": 2})
        )]
    );

    // A pin naming another attempt is refused: no binding note, nothing queued.
    let refused = fw(
        &[
            "reject",
            "run-gated",
            "--gate",
            "3",
            "--attempt",
            "1",
            "--db",
            db_arg,
        ],
        tmp.path(),
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(1), "{stderr}");
    assert!(refused.stdout.is_empty());
    assert!(!stderr.contains("bound to manual gate"), "{stderr}");
    assert!(
        stderr.contains("parked on manual gate awaiting_ordinal=3 attempt_number=2"),
        "{stderr}"
    );
    assert!(queued(&db, "run-gated").is_empty());
}
