//! Held-out gate invariants the original suite left unpinned: fail-closed
//! stat errors, registrable-id rules, standing-oracle predicate semantics and
//! load order, and logged (not silent) witness-recording failures.

use super::*;
use flywheel_core::lifecycle::Lifecycle;
use flywheel_core::store::{LifecycleFilter, SpendSummary, SpendWindow};

#[test]
fn empty_and_backslash_task_ids_are_not_registrable() {
    for bad in ["", "a\\b", "..\\escape"] {
        assert_eq!(registration_file_name(bad), None, "{bad:?}");
    }

    let tmp = TempDir::new().unwrap();
    let oracle = write_oracle(&tmp.path().join("oracle_dir"));
    let root = tmp.path().join("held_out");
    for bad in ["", "a\\b"] {
        let err = write_oracle_registration(&root, bad, &oracle, None, Some("sh")).unwrap_err();
        assert!(
            matches!(err, OracleRegistrationError::PathTraversal(_)),
            "{bad:?}: {err}"
        );
        let err =
            write_standing_oracle_registration(&root, bad, &oracle, &[], Some("sh")).unwrap_err();
        assert!(
            matches!(err, OracleRegistrationError::PathTraversal(_)),
            "{bad:?}: {err}"
        );
    }
    assert!(!root.join(".json").exists());
    assert!(!root.join("a\\b.json").exists());
    assert!(!root.join(STANDING_SUBDIR).join(".json").exists());

    // A file that happens to sit at the would-be path is never read for an
    // unregistrable id.
    register(&root, "", json!([cmd("false", "stray")]));
    let source = FilesystemHeldOutGraderSource::new(&root);
    assert_eq!(source.graders_for(""), Ok(None));
}

/// Run `probe` with `dir` stripped of search permission, restoring it after.
/// Returns `None` when the process can stat through the directory anyway
/// (running as root), where the fail-closed branch is unreachable.
fn without_search_permission<T>(dir: &Path, probe: impl FnOnce() -> T) -> Option<T> {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o600)).unwrap();
    let blocked = std::fs::symlink_metadata(dir.join("probe"))
        .is_err_and(|err| err.kind() == std::io::ErrorKind::PermissionDenied);
    let out = blocked.then(probe);
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    out
}

// The root itself stats fine (its parent is searchable) but nothing under it
// can be inspected: that is not "nothing registered", it must fail closed.
#[tokio::test]
async fn an_uninspectable_registration_fails_closed_instead_of_reading_as_unregistered() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("held_out");
    register(&root, "alpha", json!([cmd("false", "would-block")]));
    let committed = tree(&tmp, "tree");
    let source = FilesystemHeldOutGraderSource::new(&root);

    let Some((per_task, standing)) = without_search_permission(&root, || {
        (
            source.graders_for("alpha"),
            source.standing_graders_for(&task("alpha")),
        )
    }) else {
        return;
    };
    let err = per_task.unwrap_err();
    assert!(err.to_string().contains("cannot be inspected"), "{err}");
    let err = standing.unwrap_err();
    assert!(err.to_string().contains("cannot be inspected"), "{err}");

    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o600)).unwrap();
    let verdict = evaluate(&task("alpha"), &root, &committed, "run-1").await;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(verdict.outcome, GateOutcome::Fail, "{}", verdict.reason);
    assert!(
        verdict.reason.contains("failed closed"),
        "{}",
        verdict.reason
    );
    assert!(verdict.results.is_empty());
}

#[tokio::test]
async fn a_standing_oracle_applies_when_the_task_carries_any_one_of_its_tags() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("held-out");
    write_standing(
        &root,
        "money",
        "false",
        Some(json!({"tags": ["netting", "fx"]})),
    );
    let verdict = evaluate(&tagged("gh-7", &["fx"]), &root, tmp.path(), "r1").await;
    assert_eq!(verdict.outcome, GateOutcome::Fail, "{}", verdict.reason);
    assert_eq!(verdict.results.len(), 1);
    assert_eq!(verdict.results[0].grader_name.as_deref(), Some("money"));
}

#[tokio::test]
async fn a_standing_match_with_a_non_string_tag_fails_closed() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("held-out");
    write_standing(&root, "bad", "true", Some(json!({"tags": ["netting", 7]})));
    let verdict = evaluate(&tagged("gh-1", &["netting"]), &root, tmp.path(), "r1").await;
    assert_eq!(verdict.outcome, GateOutcome::Fail);
    assert!(
        verdict.reason.contains("failed closed"),
        "{}",
        verdict.reason
    );
    assert!(verdict.reason.contains("'tags' list"), "{}", verdict.reason);
    assert!(verdict.results.is_empty());
}

#[test]
fn standing_oracles_load_in_file_name_order_not_directory_order() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("held-out");
    // Created in neither sorted nor reverse-sorted order, and enough entries
    // that a hashed directory listing coming back sorted is not plausible.
    let created: Vec<String> = "qwertyuiopasdfghjklzxcvbnm"
        .chars()
        .map(|c| format!("{c}-oracle"))
        .collect();
    for name in &created {
        write_standing(&root, name, "true", None);
    }
    let source = FilesystemHeldOutGraderSource::new(&root);
    let names: Vec<String> = source
        .standing_graders_for(&tagged("t", &[]))
        .unwrap()
        .into_iter()
        .map(|g| g.name.unwrap())
        .collect();
    let mut sorted = created.clone();
    sorted.sort();
    assert_eq!(names, sorted);
}

/// Lifecycle reads delegate to an in-memory store; every event append is
/// refused, as a store outage at gate time would.
struct RefusingAppendStore(InMemoryStore);

impl LifecycleStore for RefusingAppendStore {
    fn create_lifecycle(&self, lifecycle: &Lifecycle) -> Result<(), StoreError> {
        self.0.create_lifecycle(lifecycle)
    }

    fn update_lifecycle(
        &self,
        lifecycle: &Lifecycle,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        self.0.update_lifecycle(lifecycle, expected_version)
    }

    fn load_lifecycle(&self, run_id: &str) -> Result<Option<Lifecycle>, StoreError> {
        self.0.load_lifecycle(run_id)
    }

    fn list_lifecycles(&self, filter: &LifecycleFilter) -> Result<Vec<Lifecycle>, StoreError> {
        self.0.list_lifecycles(filter)
    }

    fn summarize_spend(&self, window: &SpendWindow) -> Result<SpendSummary, StoreError> {
        self.0.summarize_spend(window)
    }
}

impl DomainEventStore for RefusingAppendStore {
    fn append_domain_event(
        &self,
        _event: &DomainEvent,
        _expected_version: u64,
    ) -> Result<Lifecycle, StoreError> {
        Err(StoreError::backend(std::io::Error::other(
            "event log unavailable",
        )))
    }

    fn list_domain_events(&self, run_id: &str) -> Result<Vec<DomainEvent>, StoreError> {
        self.0.list_domain_events(run_id)
    }
}

#[tokio::test]
async fn witness_recording_failures_are_logged_and_the_park_still_holds() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("held_out");
    register(&root, "alpha", json!([cmd("exit 1", "gate-check")]));
    let source = FilesystemHeldOutGraderSource::new(&root);
    let request = submit_request(&tmp, "alpha", Status::Done);
    let store = RefusingAppendStore(seeded_store(&request.run_id, "alpha"));
    let mut lines = Vec::new();
    let decision = gate_landing(
        &store,
        Some(&source),
        &request,
        &GateOptions::default(),
        Utc::now,
        &mut |line| lines.push(line.to_owned()),
    )
    .await;
    assert!(!decision.lands());
    assert_eq!(decision.gate(), Some(GateOutcome::Fail));
    let logged = |needle: &str| {
        lines
            .iter()
            .any(|line| line.contains("alpha: ") && line.contains(needle))
    };
    assert!(
        logged(
            "failed to record held-out-gate verdict event (store backend: event log unavailable)"
        ),
        "{lines:?}"
    );
    assert!(
        logged(
            "failed to record held-out-gate landing-parked event (store backend: event log unavailable)"
        ),
        "{lines:?}"
    );
    assert_eq!(
        kinds(&store.0, &request.run_id),
        [DomainEventKind::LifecycleInitialized]
    );
}
