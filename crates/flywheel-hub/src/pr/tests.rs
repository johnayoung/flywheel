//! What gh says, parsed; problems sorted; logs tidied and redacted; the
//! events a change makes. The hub end to end (a scripted `gh`, a real
//! server): `tests/pr.rs`.

use super::gh::{Gh, MergeMethods, classify, log_args, plain, tail, tidy_log};
use super::watch::{Snap, transitions};
use super::*;
use crate::review::Tools;
use crate::review::runner::Output;

fn out(code: Option<i32>, stdout: &str, stderr: &str) -> Output {
    Output {
        code,
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.into(),
        truncated: false,
        timed_out: false,
    }
}

#[test]
fn gh_failures_are_sorted_into_why() {
    let auth = classify(&out(
        Some(4),
        "",
        "To get started with GitHub CLI, please run:  gh auth login\n",
    ));
    assert_eq!(auth.kind, ProblemKind::Unauthenticated);
    assert!(auth.reason.contains("gh auth login"), "{auth}");
    let limited = classify(&out(
        Some(1),
        "",
        "GraphQL: API rate limit exceeded for user ID 1.\n",
    ));
    assert_eq!(limited.kind, ProblemKind::RateLimited);
    let remote = classify(&out(
        Some(1),
        "",
        "none of the git remotes configured for this repository point to a known GitHub host",
    ));
    assert_eq!(remote.kind, ProblemKind::NotGithub);
    let other = classify(&out(Some(1), "", "error connecting to api.github.com\n"));
    assert_eq!(other.kind, ProblemKind::Failed);
    assert_eq!(other.reason, "error connecting to api.github.com");
    let slow = Output {
        timed_out: true,
        ..out(None, "", "")
    };
    assert!(classify(&slow).reason.contains("timed out"));
}

#[test]
fn a_missing_gh_is_named_before_anything_runs() {
    let tools = Tools {
        git: "git".into(),
        gh: "/nonexistent/fw-hub-test-gh".into(),
    };
    let err = Gh::find(&tools, std::path::Path::new("/"))
        .err()
        .expect("missing");
    assert_eq!(err.kind, ProblemKind::Missing);
    assert!(err.reason.contains("cli.github.com"));
}

fn check(name: &str, bucket: &str, link: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name, "state": "X", "bucket": bucket, "link": link,
        "startedAt": "2026-11-01T10:00:00Z", "completedAt": "2026-11-01T10:02:05Z",
        "workflow": "CI", "description": ""
    })
}

fn parse_checks(raw: serde_json::Value) -> Vec<Check> {
    #[derive(serde::Deserialize)]
    struct Wrap(Vec<serde_json::Value>);
    let Wrap(items) = serde_json::from_value(raw).unwrap();
    items
        .into_iter()
        .map(|v| {
            // Through the same parse gh's answer takes.
            let text = serde_json::to_vec(&vec![v]).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let gh = dir.path().join("gh");
            crate::test_support::write_script(
                &gh,
                &format!(
                    "#!/bin/sh\ncat <<'EOF'\n{}\nEOF\n",
                    String::from_utf8(text).unwrap()
                ),
            );
            let tools = Tools {
                git: "git".into(),
                gh: gh.to_string_lossy().into_owned(),
            };
            Gh::find(&tools, dir.path())
                .unwrap()
                .checks(1)
                .unwrap()
                .remove(0)
        })
        .collect()
}

#[test]
fn checks_parse_with_runs_jobs_durations_and_a_rollup() {
    let checks = parse_checks(serde_json::json!([
        check(
            "test",
            "fail",
            "https://github.com/o/r/actions/runs/123/job/456"
        ),
        check(
            "lint",
            "pass",
            "https://github.com/o/r/actions/runs/123/job/457"
        ),
        check("deploy", "pending", "https://ci.example.com/build/9"),
        check("docs", "skipping", ""),
    ]));
    assert_eq!(checks[0].bucket, Bucket::Fail);
    assert_eq!((checks[0].run_id, checks[0].job_id), (Some(123), Some(456)));
    assert_eq!(checks[0].duration_secs, Some(125));
    assert_eq!(checks[0].workflow.as_deref(), Some("CI"));
    // Not an Actions link: no run, no log.
    assert_eq!((checks[2].run_id, checks[2].job_id), (None, None));
    assert!(log_args(&checks[2]).is_none());
    assert_eq!(
        log_args(&checks[0]).unwrap(),
        ["run", "view", "--job", "456", "--log-failed"]
    );
    let summary = ChecksSummary::of(&checks);
    assert_eq!((summary.total, summary.passing, summary.failing), (4, 1, 1));
    assert_eq!((summary.pending, summary.skipped), (1, 1));
    // A failure wins over checks still running.
    assert_eq!(summary.rollup, Rollup::Failing);
    assert_eq!(ChecksSummary::of(&checks[1..3]).rollup, Rollup::Pending);
    assert_eq!(ChecksSummary::of(&checks[1..2]).rollup, Rollup::Passing);
    assert_eq!(ChecksSummary::of(&checks[3..]).rollup, Rollup::None);
    assert_eq!(ChecksSummary::of(&[]).rollup, Rollup::None);
}

#[test]
fn the_merge_method_is_the_default_when_allowed_else_the_first() {
    let methods = |merge, squash, rebase, default: Option<MergeMethod>| MergeMethods {
        allowed: [
            (merge, MergeMethod::Merge),
            (squash, MergeMethod::Squash),
            (rebase, MergeMethod::Rebase),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .map(|(_, m)| m)
        .collect(),
        default,
    };
    assert_eq!(
        methods(true, true, true, Some(MergeMethod::Rebase)).pick(),
        Some(MergeMethod::Rebase)
    );
    assert_eq!(
        methods(false, true, false, Some(MergeMethod::Merge)).pick(),
        Some(MergeMethod::Squash)
    );
    assert_eq!(methods(false, false, false, None).pick(), None);
}

#[test]
fn failed_logs_are_tidied_tailed_and_redacted() {
    let token = format!("ghp_{}", "aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bC1dE3fG5");
    let raw = format!(
        "build\tSet up job\t2026-11-01T10:00:00.1234567Z Runner 2.3\n\
         build\tRun tests\t2026-11-01T10:00:01.0000000Z running 3 tests\n\
         build\tRun tests\t2026-11-01T10:00:02.0000000Z env GITHUB_TOKEN={token}\n\
         build\tRun tests\t2026-11-01T10:00:03.0000000Z Authorization: Bearer abcdef0123456789\n\
         build\tRun tests\t2026-11-01T10:00:04.0000000Z SyntaxError: Unexpected token '}}'\n\
         build\tRun tests\t2026-11-01T10:00:05.0000000Z fetching https://bot:hunter2secret@example.com/x\n\
         build\tRun tests\t2026-11-01T10:00:06.0000000Z test retry ... FAILED\n"
    );
    let lines = tidy_log(&raw);
    assert_eq!(lines[0], "== build / Set up job ==");
    assert_eq!(lines[1], "Runner 2.3");
    assert_eq!(lines[2], "== build / Run tests ==");
    let (text, shown) = tail(&lines, 6, 64 * 1024);
    assert_eq!(shown, 6);
    assert!(text.starts_with("running 3 tests"), "{text}");
    let redacted = crate::activity::redact(&text);
    assert!(!redacted.contains(&token), "{redacted}");
    assert!(redacted.contains("GITHUB_TOKEN=[redacted]"), "{redacted}");
    assert!(
        redacted.contains("Authorization: Bearer [redacted]"),
        "{redacted}"
    );
    assert!(!redacted.contains("hunter2secret"), "{redacted}");
    assert!(
        redacted.contains("https://bot:[redacted]@example.com/x"),
        "{redacted}"
    );
    // What an agent needs to fix it stays.
    assert!(
        redacted.contains("SyntaxError: Unexpected token '}'"),
        "{redacted}"
    );
    assert!(redacted.contains("test retry ... FAILED"));
    // Line structure kept.
    assert_eq!(redacted.lines().count(), 6);
    // Bounded by bytes too.
    let (small, n) = tail(&lines, 100, 40);
    assert!(small.len() <= 40 && n >= 1, "{small}");
    // A single line past the budget: its end, not nothing.
    let long = vec![format!("{}END", "x".repeat(100))];
    let (end, n) = tail(&long, 10, 20);
    assert_eq!((end.len(), n), (20, 1));
    assert!(end.ends_with("END"));
}

fn snap(state: &str, review: Option<&str>, buckets: &[&str]) -> Snap {
    let raw = serde_json::json!({
        "number": 7, "title": "t", "url": "u", "state": state, "isDraft": false,
        "reviewDecision": review, "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
        "headRefName": "feat", "baseRefName": "main", "headRefOid": "abc"
    });
    let dir = tempfile::tempdir().unwrap();
    let gh = dir.path().join("gh");
    crate::test_support::write_script(&gh, &format!("#!/bin/sh\ncat <<'EOF'\n[{raw}]\nEOF\n"));
    let tools = Tools {
        git: "git".into(),
        gh: gh.to_string_lossy().into_owned(),
    };
    let pr = Gh::find(&tools, dir.path())
        .unwrap()
        .pr_view("feat")
        .unwrap()
        .unwrap();
    let checks: Vec<Check> = if buckets.is_empty() {
        Vec::new()
    } else {
        parse_checks(serde_json::Value::Array(
            buckets
                .iter()
                .enumerate()
                .map(|(i, b)| check(&format!("c{i}"), b, ""))
                .collect(),
        ))
    };
    let summary = ChecksSummary::of(&checks);
    Snap {
        pr,
        checks,
        summary,
    }
}

#[test]
fn changes_make_events_and_a_first_look_makes_none() {
    let pending = snap("OPEN", None, &["pending"]);
    let failing = snap("OPEN", None, &["fail", "pending"]);
    let passing = snap("OPEN", Some("APPROVED"), &["pass"]);
    let asked = snap("OPEN", Some("CHANGES_REQUESTED"), &["pass"]);
    let merged = snap("MERGED", Some("APPROVED"), &["pass"]);
    assert_eq!(pending.pr.state, "open");
    assert_eq!(asked.pr.review.as_deref(), Some("changes_requested"));
    assert!(transitions(None, &failing).is_empty());
    assert_eq!(transitions(Some(&pending), &failing), ["pr.checks_failed"]);
    assert!(transitions(Some(&failing), &failing).is_empty());
    assert_eq!(transitions(Some(&failing), &passing), ["pr.checks_passed"]);
    // Passing all along is not news.
    assert!(transitions(Some(&passing), &passing).is_empty());
    assert_eq!(
        transitions(Some(&passing), &asked),
        ["pr.review_changes_requested"]
    );
    assert_eq!(transitions(Some(&passing), &merged), ["pr.merged"]);
    // Another pull request on the branch: a first look.
    let mut other = failing.clone();
    other.pr.number = 8;
    assert!(transitions(Some(&pending), &other).is_empty());
}

/// A gh answering `answer` and writing its argv to `<dir>/args`.
fn gh_answering(dir: &std::path::Path, answer: &str) -> Tools {
    let gh = dir.join("gh");
    crate::test_support::write_script(
        &gh,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}/args'\ncat <<'EOF'\n{answer}\nEOF\n",
            dir.display()
        ),
    );
    Tools {
        git: "git".into(),
        gh: gh.to_string_lossy().into_owned(),
    }
}

fn raw_pr(number: u64, head: &str, state: &str, cross: bool) -> serde_json::Value {
    serde_json::json!({
        "number": number, "title": "t", "url": format!("https://github.com/o/r/pull/{number}"),
        "state": state, "isDraft": false, "headRefName": head, "baseRefName": "main",
        "headRefOid": "abc", "isCrossRepository": cross
    })
}

#[test]
fn a_branch_named_like_a_number_is_never_bound_to_that_pull_request() {
    let dir = tempfile::tempdir().unwrap();
    // `gh pr view 10` would answer pull request #10 of another branch;
    // listed by head, only a pull request whose head is `10` counts.
    let tools = gh_answering(
        dir.path(),
        &serde_json::json!([raw_pr(10, "rust-rewrite", "OPEN", false)]).to_string(),
    );
    let gh = Gh::find(&tools, dir.path()).unwrap();
    assert_eq!(gh.pr_view("10").unwrap(), None);
    let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(&args[..4], ["pr", "list", "--head", "10"], "{args:?}");
    assert!(!args.contains(&"view"), "{args:?}");
    assert_eq!(gh.pr_view("#10").unwrap(), None);

    // Open first (even when older), never a fork's same-named branch.
    let tools = gh_answering(
        dir.path(),
        &serde_json::json!([
            raw_pr(30, "feat", "OPEN", true),
            raw_pr(21, "feat", "CLOSED", false),
            raw_pr(12, "feat", "OPEN", false),
        ])
        .to_string(),
    );
    let gh = Gh::find(&tools, dir.path()).unwrap();
    assert_eq!(gh.pr_view("feat").unwrap().unwrap().number, 12);
    // None open: the newest.
    let tools = gh_answering(
        dir.path(),
        &serde_json::json!([
            raw_pr(21, "feat", "MERGED", false),
            raw_pr(12, "feat", "CLOSED", false)
        ])
        .to_string(),
    );
    let gh = Gh::find(&tools, dir.path()).unwrap();
    let pr = gh.pr_view("feat").unwrap().unwrap();
    assert_eq!((pr.number, pr.state.as_str()), (21, "merged"));
    let tools = gh_answering(dir.path(), "[]");
    let gh = Gh::find(&tools, dir.path()).unwrap();
    assert_eq!(gh.pr_view("feat").unwrap(), None);
}

#[test]
fn terminal_control_from_github_is_stripped() {
    // Bracketed-paste end, an OSC 52 clipboard write, a carriage return,
    // colour codes, a title set ended by ESC backslash, a C1 CSI.
    let raw = "evil \x1b[201~rm -rf ~ \r\x1b]52;c;ZWNobyBwd25lZA==\x07 tail \
               \x1b[31mred\x1b[0m \x1b]0;t\x1b\\x \u{9b}2Jy \x1b(Bz\ttab";
    let clean = plain(raw);
    assert_eq!(clean, "evil rm -rf ~  tail red x y z\ttab");
    assert!(!clean.chars().any(|c| c.is_control() && c != '\t'));
    // A log, line by line.
    let lines = tidy_log(&format!(
        "build\tRun tests\t2026-11-01T10:00:01.0000000Z \x1b[31merror[E0308]\x1b[0m: mismatched types\n{raw}\n"
    ));
    assert_eq!(lines[1], "error[E0308]: mismatched types");
    assert!(lines.iter().all(|l| !l.contains('\x1b')), "{lines:?}");

    // Check names and links: plain text, web links only.
    let checks = parse_checks(serde_json::json!([
        check(
            "lint\u{1b}]0;pwned\u{7}",
            "fail",
            "javascript:alert(document.cookie)"
        ),
        check(
            "test",
            "pass",
            "https://github.com/o/r/actions/runs/1/job/2"
        ),
    ]));
    assert_eq!(checks[0].name, "lint");
    assert_eq!(checks[0].link, None);
    assert_eq!(
        checks[1].link.as_deref(),
        Some("https://github.com/o/r/actions/runs/1/job/2")
    );
}

#[test]
fn ci_log_secret_shapes_are_redacted() {
    let text = "aws AKIAIOSFODNN7EXAMPLE secret wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n\
                mysql --password s3cr3tpass and --client-secret=abcd1234efgh\n\
                commit 0123456789abcdef0123456789abcdef01234567 ok\n";
    let redacted = crate::activity::redact(text);
    assert!(!redacted.contains("wJalrXUtnFEMI"), "{redacted}");
    assert!(!redacted.contains("AKIAIOSFODNN7EXAMPLE"), "{redacted}");
    assert!(!redacted.contains("s3cr3tpass"), "{redacted}");
    assert!(!redacted.contains("abcd1234efgh"), "{redacted}");
    assert!(redacted.contains("--password [redacted] and"), "{redacted}");
    // A commit is not a secret.
    assert!(
        redacted.contains("0123456789abcdef0123456789abcdef01234567"),
        "{redacted}"
    );
}
