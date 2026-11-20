use super::*;
use crate::store::TrialStatus;
use std::os::unix::fs::PermissionsExt;

fn trial(id: &str, task: &str, variant: &str, oracle: &str) -> TrialRow {
    TrialRow {
        trial_id: id.into(),
        task_id: task.into(),
        variant_hash: variant.into(),
        status: Some(TrialStatus::Done),
        oracle: Some(oracle.into()),
        trace_json: "{}".into(),
        ..TrialRow::default()
    }
}

fn envelope(json: &str) -> String {
    format!("thinking...\n{OPEN}\n{json}\n{CLOSE}\n")
}

#[test]
fn envelopes_parse_closed() {
    let v = parse(&envelope(r#"{"verdict": "fake_pass", "confidence": "high", "evidence": [{"at": "final.diff", "quote": "assert!(true)"}], "notes": "n"}"#)).unwrap();
    assert_eq!(v.verdict, "fake_pass");
    for (text, why) in [
        ("no envelope".to_owned(), "no review envelope"),
        (
            format!("{}{}", envelope("{}"), envelope("{}")),
            "2 review envelopes",
        ),
        (
            envelope(r#"{"verdict": "great", "confidence": "high"}"#),
            "is not one of",
        ),
        (
            envelope(r#"{"verdict": "clean", "confidence": "sure"}"#),
            "confidence",
        ),
        (envelope("not json"), "malformed"),
    ] {
        assert!(parse(&text).unwrap_err().contains(why), "{text}");
    }
}

#[test]
fn every_failure_and_every_pass_is_read_in_a_small_run() {
    let trials = vec![
        trial("a", "t1", "va", "pass"),
        trial("b", "t1", "vb", "fail"),
        {
            let mut t = trial("c", "t2", "va", "pass");
            t.status = Some(TrialStatus::InfraError);
            t
        },
    ];
    assert_eq!(pick(&trials, 1), ["a", "b"]);
}

#[test]
fn a_large_run_samples_a_quarter_of_each_cells_passes() {
    let mut trials = Vec::new();
    for i in 0..240 {
        trials.push(trial(
            &format!("p{i:03}"),
            &format!("t{}", i % 4),
            if i % 2 == 0 { "va" } else { "vb" },
            "pass",
        ));
    }
    trials.push(trial("f", "t0", "va", "fail"));
    let picked = pick(&trials, 3);
    assert!(picked.contains(&"f".to_owned()));
    // Each (variant, task) cell gives up a quarter of its passes, rounded up.
    let mut cells: std::collections::BTreeMap<(String, String), usize> =
        std::collections::BTreeMap::new();
    for t in trials
        .iter()
        .filter(|t| t.oracle.as_deref() == Some("pass"))
    {
        *cells
            .entry((t.variant_hash.clone(), t.task_id.clone()))
            .or_insert(0) += 1;
    }
    let expected: usize = cells
        .values()
        .map(|n| (*n as f64 * PASS_SAMPLE).ceil() as usize)
        .sum();
    assert_eq!(picked.len(), 1 + expected);
    assert!(picked.len() < 100, "a sample, not every pass");
    assert_eq!(pick(&trials, 3), picked, "the seed decides");
}

#[test]
fn only_a_confident_fake_on_a_pass_makes_it_suspect() {
    let v = |verdict: &str, confidence: &str| ReaderVerdict {
        verdict: verdict.into(),
        confidence: confidence.into(),
        evidence: Vec::new(),
        notes: String::new(),
    };
    assert!(suspect(
        &trial("a", "t", "v", "pass"),
        &v("fake_pass", "high")
    ));
    assert!(!suspect(
        &trial("a", "t", "v", "pass"),
        &v("fake_pass", "medium")
    ));
    assert!(!suspect(
        &trial("a", "t", "v", "fail"),
        &v("fake_pass", "high")
    ));
    assert!(!suspect(
        &trial("a", "t", "v", "pass"),
        &v("discovery_bug", "high")
    ));
}

/// The production reader end to end against a scripted CLI: it runs
/// read-only (its argv says so) and its envelope is parsed; a reader whose
/// first answer is malformed is asked once more.
#[test]
fn the_agent_reader_runs_read_only_and_is_retried_once() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("claude");
    let argv_log = dir.path().join("argv.log");
    let count = dir.path().join("count");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
echo "$*" >> '{argv}'
n=$(cat '{count}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{count}'
if [ $n = 1 ]; then T='no envelope here'; else T='<!-- EVAL_REVIEW -->{{\"verdict\":\"clean\",\"confidence\":\"medium\",\"notes\":\"ok\"}}<!-- /EVAL_REVIEW -->'; fi
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"s","model":"claude-haiku-fake"}}'
printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"result":"'"$T"'","num_turns":1,"total_cost_usd":0.01,"usage":{{"input_tokens":1,"output_tokens":1}}}}'
"#,
            argv = argv_log.display(),
            count = count.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let reader = AgentReader::new("haiku", 20, Some(vec![script.display().to_string()]));
    let t = trial("a", "t1", "va", "pass");
    let v = review_one(&reader, &t, dir.path(), "make it so").unwrap();
    assert_eq!(v.verdict, "clean");
    let argv = std::fs::read_to_string(&argv_log).unwrap();
    assert!(argv.contains("--tools Read,Grep,Glob"), "{argv}");
    assert!(argv.contains("--setting-sources=project"), "{argv}");
    assert_eq!(argv.lines().count(), 2, "retried once");
}
