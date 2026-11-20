use super::*;

#[test]
fn claude_project_keys_follow_claude_codes_rule() {
    assert_eq!(
        claude_project_key(Path::new("/home/me/code/flywheel")),
        "-home-me-code-flywheel"
    );
    assert_eq!(
        claude_project_key(Path::new("/tmp/fw-canary.1jKK/work")),
        "-tmp-fw-canary-1jKK-work"
    );
    // A trial's directories never share a key with the repo they replay.
    let repo = Path::new("/home/me/code/flywheel");
    let trial = Path::new("/home/me/.flywheel/projects/p/evals/runs/r/trials/t/work");
    assert!(!claude_project_key(trial).starts_with(&claude_project_key(repo)));
}
