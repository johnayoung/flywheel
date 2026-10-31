//! Manual-gate decision binding invariants: a decision names one parked
//! gate instance (gate ordinal + attempt) and applies to nothing else.

use super::*;
use crate::invoker_client::{
    EVENT_CONTROL_NOT_APPLICABLE, GateBinding, GateDecisionError, enqueue_gate_decision,
};

fn payload(value: serde_json::Value) -> JsonObject {
    value.as_object().unwrap().clone()
}

fn queued(h: &Harness, run_id: &str) -> usize {
    h.store.claim_commands(run_id, Utc::now()).unwrap().len()
}

#[tokio::test]
async fn a_decision_on_one_attempts_gate_never_resolves_the_retried_attempts_repark() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let run_id = "run-gate-across-attempts";
    let task = task(vec![
        ok_command(),
        rubric("r0", true),
        manual("confirm", "Confirm."),
    ]);
    let mut lc = lifecycle(run_id);
    let judge = ScriptedJudge::new(vec![pass_verdict("ok"), pass_verdict("ok")]);
    let (invoke, _) = scripted_invoker(vec![verify_iteration(), verify_iteration()]);
    let config = judge_config(&dir, judge, 1);
    let first = h.run(&task, &mut lc, &config, invoke.clone()).await;
    assert_eq!(first.lifecycle.status, Status::AwaitingApproval);
    let mut lc = first.lifecycle;
    enqueue_reject(h.store.as_ref(), run_id, Some("redo"), Utc::now()).unwrap();
    assert_eq!(resolve(&h, &mut lc, &task, 1).reason, "rejected_retry");
    let second = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(second.lifecycle.status, Status::AwaitingApproval);
    let mut lc = second.lifecycle;
    // Attempt 2 re-parked the same gate ordinal attempt 1 parked.
    assert_eq!(lc.awaiting_manual_ordinal, Some(2));
    assert_eq!(
        GateBinding::of_parked(&lc),
        Some(GateBinding {
            awaiting_ordinal: 2,
            attempt_number: 2,
        })
    );

    // A decision pinned to attempt 1's park is refused at enqueue ...
    let err = enqueue_gate_decision(
        h.store.as_ref(),
        run_id,
        CONTROL_COMMAND_APPROVE,
        payload(json!({"awaiting_ordinal": 2, "attempt_number": 1})),
        Utc::now(),
    )
    .unwrap_err();
    assert!(
        matches!(err, GateDecisionError::GateMismatch { .. }),
        "{err}"
    );
    assert_eq!(queued(&h, run_id), 0);

    // ... and one that reached the queue anyway never applies to attempt 2.
    let stale = h
        .store
        .enqueue_command(
            run_id,
            CONTROL_COMMAND_APPROVE,
            payload(json!({"awaiting_ordinal": 2, "attempt_number": 1})),
            Utc::now(),
        )
        .unwrap();
    let result = resolve(&h, &mut lc, &task, 1);
    assert!(!result.applied, "{result:?}");
    assert_eq!(lc.status, Status::AwaitingApproval);
    assert!(manual_rows(&h, run_id, 2).is_empty());
    assert_eq!(not_applicable_ids(&h, run_id), vec![json!(stale.id)]);

    // A decision bound to the gate parked now carries attempt 2 and applies.
    let fresh = enqueue_approve(h.store.as_ref(), run_id, Utc::now()).unwrap();
    assert_eq!(
        GateBinding::of_command(&fresh),
        Some(GateBinding {
            awaiting_ordinal: 2,
            attempt_number: 2,
        })
    );
    let result = resolve(&h, &mut lc, &task, 1);
    assert_eq!(result.reason, "approved_done");
    assert_eq!(result.command_id, fresh.id);
    let rows = manual_rows(&h, run_id, 2);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].passed);
}

#[tokio::test]
async fn a_lifecycle_off_the_gate_takes_no_decision_even_with_a_stale_gate_ordinal() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let run_id = "run-gate-stale-ordinal";
    let (task, parked) = park(&h, &dir, run_id, vec![("confirm", "Confirm.")]).await;
    let mut off_gate = parked.clone();
    off_gate.status = Status::Validating;
    assert_eq!(off_gate.awaiting_manual_ordinal, Some(2));
    assert_eq!(GateBinding::of_parked(&off_gate), None);

    // The producer refuses a lifecycle that is not parked, whatever ordinal
    // it still carries.
    let version = parked.version;
    h.store.update_lifecycle(&off_gate, version).unwrap();
    match enqueue_approve(h.store.as_ref(), run_id, Utc::now()) {
        Err(GateDecisionError::NotParked { status, .. }) => assert_eq!(status, "validating"),
        other => panic!("expected NotParked, got {other:?}"),
    }
    assert_eq!(queued(&h, run_id), 0);

    // The resolver does not apply even a decision bound to that ordinal.
    let bound = h
        .store
        .enqueue_command(
            run_id,
            CONTROL_COMMAND_APPROVE,
            payload(json!({"awaiting_ordinal": 2, "attempt_number": 1})),
            Utc::now(),
        )
        .unwrap();
    let mut lc = h.store.load_lifecycle(run_id).unwrap().unwrap();
    let result = resolve(&h, &mut lc, &task, 0);
    assert_eq!(
        result,
        ResolveApprovalOutcome {
            applied: false,
            reason: "not_awaiting".to_owned(),
            command_id: None,
        }
    );
    assert_eq!(lc.status, Status::Validating);
    assert!(manual_rows(&h, run_id, 1).is_empty());
    let pending = h.store.claim_commands(run_id, Utc::now()).unwrap();
    assert_eq!(pending.iter().map(|c| c.id).collect::<Vec<_>>(), [bound.id]);
}

#[tokio::test]
async fn a_gate_ordinal_that_names_no_manual_grader_is_never_decided() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let run_id = "run-gate-not-manual";
    let (task, parked) = park(&h, &dir, run_id, vec![("confirm", "Confirm.")]).await;
    // Ordinal 0 is the command grader, not a manual gate; ordinal 9 names
    // no grader at all. Neither may fall through to the real manual gate.
    for ordinal in [0_u32, 9] {
        let mut lc = parked.clone();
        lc.awaiting_manual_ordinal = Some(ordinal);
        h.store
            .enqueue_command(
                run_id,
                CONTROL_COMMAND_APPROVE,
                payload(json!({"awaiting_ordinal": ordinal, "attempt_number": 1})),
                Utc::now(),
            )
            .unwrap();
        let result = resolve(&h, &mut lc, &task, 0);
        assert!(!result.applied, "ordinal {ordinal}: {result:?}");
        assert_eq!(result.reason, "missing_gate");
        assert_eq!(lc.status, Status::AwaitingApproval);
        assert!(manual_rows(&h, run_id, 1).is_empty());
        assert!(h.sink.of_kind(run_id, "harness.manual_approved").is_empty());
        assert_eq!(queued(&h, run_id), 1);
    }
}

#[tokio::test]
async fn a_malformed_binding_never_matches_the_parked_gate() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let run_id = "run-gate-malformed";
    let (task, mut lc) = park(&h, &dir, run_id, vec![("confirm", "Confirm.")]).await;
    let parked = GateBinding::of_parked(&lc).unwrap();
    assert_eq!(
        parked,
        GateBinding {
            awaiting_ordinal: 2,
            attempt_number: 1,
        }
    );
    // An ordinal past u32 must not wrap onto the parked gate, and a
    // stringly-typed one is no binding at all.
    let wrapped = h
        .store
        .enqueue_command(
            run_id,
            CONTROL_COMMAND_APPROVE,
            payload(json!({"awaiting_ordinal": (1_u64 << 32) + 2, "attempt_number": 1})),
            Utc::now(),
        )
        .unwrap();
    let stringly = h
        .store
        .enqueue_command(
            run_id,
            CONTROL_COMMAND_APPROVE,
            payload(json!({"awaiting_ordinal": "2", "attempt_number": 1})),
            Utc::now(),
        )
        .unwrap();
    assert_eq!(GateBinding::of_command(&wrapped), None);
    assert_eq!(GateBinding::of_command(&stringly), None);

    let result = resolve(&h, &mut lc, &task, 0);
    assert!(!result.applied, "{result:?}");
    assert_eq!(lc.status, Status::AwaitingApproval);
    assert!(manual_rows(&h, run_id, 1).is_empty());
    assert_eq!(
        not_applicable_ids(&h, run_id),
        vec![json!(wrapped.id), json!(stringly.id)]
    );
}

#[tokio::test]
async fn a_pin_that_is_not_an_integer_is_refused_and_nothing_is_queued() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let run_id = "run-gate-non-int-pin";
    park(&h, &dir, run_id, vec![("confirm", "Confirm.")]).await;
    for pin in [
        json!({"awaiting_ordinal": "2"}),
        json!({"attempt_number": "1"}),
        json!({"awaiting_ordinal": 2, "attempt_number": null}),
        json!({"awaiting_ordinal": 2.0}),
        json!({"attempt_number": -1}),
    ] {
        let err = enqueue_gate_decision(
            h.store.as_ref(),
            run_id,
            CONTROL_COMMAND_REJECT,
            payload(pin.clone()),
            Utc::now(),
        )
        .unwrap_err();
        assert!(
            matches!(err, GateDecisionError::GateMismatch { .. }),
            "{pin}: {err}"
        );
    }
    assert_eq!(queued(&h, run_id), 0);
}

#[tokio::test]
async fn the_producer_reports_the_binding_it_stamped() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let run_id = "run-gate-reported-binding";
    park(&h, &dir, run_id, vec![("confirm", "Confirm.")]).await;
    let (record, binding) = enqueue_gate_decision(
        h.store.as_ref(),
        run_id,
        CONTROL_COMMAND_APPROVE,
        payload(json!({"awaiting_ordinal": 2})),
        Utc::now(),
    )
    .unwrap();
    let expected = GateBinding {
        awaiting_ordinal: 2,
        attempt_number: 1,
    };
    assert_eq!(binding, expected);
    assert_eq!(GateBinding::of_command(&record), Some(expected));
    assert_eq!(binding.to_string(), "awaiting_ordinal=2 attempt_number=1");
}

#[tokio::test]
async fn every_unapplied_row_records_why_it_was_not_applied() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let run_id = "run-gate-not-applicable-reasons";
    let (task, mut lc) = park(&h, &dir, run_id, vec![("confirm", "Confirm.")]).await;
    let unbound = h
        .store
        .enqueue_command(
            run_id,
            CONTROL_COMMAND_REJECT,
            JsonObject::new(),
            Utc::now(),
        )
        .unwrap();
    let say =
        crate::invoker_client::enqueue_say(h.store.as_ref(), run_id, "hello", Utc::now()).unwrap();
    let applied = enqueue_approve(h.store.as_ref(), run_id, Utc::now()).unwrap();
    let duplicate = enqueue_reject(h.store.as_ref(), run_id, None, Utc::now()).unwrap();

    let result = resolve(&h, &mut lc, &task, 0);
    assert_eq!(result.reason, "approved_done");
    assert_eq!(result.command_id, applied.id);
    let reasons: Vec<_> = h
        .sink
        .of_kind(run_id, EVENT_CONTROL_NOT_APPLICABLE)
        .into_iter()
        .map(|r| (r.payload["command_id"].clone(), r.payload["reason"].clone()))
        .collect();
    assert_eq!(
        reasons,
        vec![
            (json!(unbound.id), json!("not_bound_to_this_gate")),
            (json!(say.id), json!("not_a_gate_decision")),
            (json!(duplicate.id), json!("superseded")),
        ]
    );
}
