//! Commit provenance trailers (spec 00078).
//!
//! The shared vocabulary and mechanism for stamping harness-authoritative
//! git trailers onto the commits a landing introduces into the base.
//! Consumed by the merge submit strategy (`crate::submitter`), the pr
//! strategy, and the `fw show` commit lookup -- the trailer key names and the
//! value derivation live here so those surfaces agree on one vocabulary.
//!
//! Stamping is message-only: [`stamp_commit_messages`] recreates every commit
//! in `base..branch` with an *identical tree object* and *identical
//! author/committer identity*, changing only the message to carry the
//! authoritative trailers. The agent is untrusted (`docs/vision.md`): any
//! agent-authored `Flywheel-*` trailer is stripped and replaced with the
//! harness-derived value (spec 00078, D-2), so a forged provenance value
//! cannot survive on the landed commit.
//!
//! Port of the Python `flywheel_worktree._trailers` module. Where the Python
//! issued four `git` reads per rewritten commit (parents, tree, message,
//! identity), this port reads all of them through one `git log -1` call, so
//! a stamp costs three git invocations per commit instead of six; the
//! rewritten messages are byte-identical.

use regex::Regex;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

/// The task-id provenance trailer key.
pub const TRAILER_KEY_TASK: &str = "Flywheel-Task";
/// The run-id provenance trailer key. `fw show` reads this back off a landed
/// commit to resolve its run.
pub const TRAILER_KEY_RUN: &str = "Flywheel-Run";
/// The phase-directory provenance trailer key.
pub const TRAILER_KEY_PHASE: &str = "Flywheel-Phase";

/// All provenance keys, in the order they are stamped onto a commit.
pub const PROVENANCE_TRAILER_KEYS: [&str; 3] =
    [TRAILER_KEY_TASK, TRAILER_KEY_RUN, TRAILER_KEY_PHASE];

/// Any trailer whose token is `Flywheel-<word>` is harness-owned provenance:
/// stamping strips every one the agent may have authored before writing the
/// authoritative set, so a forged value (`Flywheel-Run: forged-123`) cannot
/// survive on the landed commit (spec 00078, D-2). Anchored at line start
/// (after optional leading whitespace) and case-insensitive, since git
/// trailer tokens match case-blind.
static FLYWHEEL_TRAILER_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*Flywheel-[A-Za-z0-9-]+\s*:").expect("static regex"));

/// NUL is not a legal byte in a git ident name/email or in a commit message
/// (git rejects it), so it is a safe field separator for reading a commit's
/// parents, tree, identity, and body in one call. The format string carries
/// git's `%x00` token (git emits a real NUL there); the argument itself holds
/// no NUL, since an embedded NUL is illegal in argv.
const FIELD_SEP: char = '\0';
const COMMIT_FORMAT: &str = "%P%x00%T%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B";

/// A git invocation the stamp expected to succeed did not, or could not be
/// spawned. The submit path swallows it into a park (the land is suppressed,
/// never a broken land).
#[derive(Debug, thiserror::Error)]
pub enum TrailerError {
    #[error("git {args} failed in {repo} (exit {code}): {stderr}")]
    Git {
        repo: String,
        args: String,
        code: String,
        stderr: String,
    },
    #[error("cannot run git {args} in {repo}: {source}")]
    Spawn {
        repo: String,
        args: String,
        #[source]
        source: std::io::Error,
    },
    #[error("git {args} in {repo} returned a malformed record: {detail}")]
    Malformed {
        repo: String,
        args: String,
        detail: String,
    },
}

/// The authoritative `(key, value)` trailers for a landed commit.
///
/// The value derivation every stamping surface shares: the landing task's
/// id, its run id, and the phase directory name the task lives under.
/// Ordered to match [`PROVENANCE_TRAILER_KEYS`]; the pr strategy and
/// `fw show` consume this same derivation so the record is uniform across
/// land paths.
pub fn provenance_trailers(task_id: &str, run_id: &str, phase: &str) -> Vec<(String, String)> {
    vec![
        (TRAILER_KEY_TASK.to_owned(), task_id.to_owned()),
        (TRAILER_KEY_RUN.to_owned(), run_id.to_owned()),
        (TRAILER_KEY_PHASE.to_owned(), phase.to_owned()),
    ]
}

/// Rewrite every commit in `base..branch` to carry `trailers`.
///
/// Message-only: each commit is recreated with `git commit-tree` over the
/// *original commit's exact tree object* and its original author/committer
/// identity, so the ordered tree list of the rewritten range is
/// byte-identical to the original (spec 00078, criterion 4). Any
/// agent-authored `Flywheel-*` trailer is stripped before the authoritative
/// trailers are appended (D-2), and each key ends up exactly once.
///
/// Returns the rewritten branch-tip sha (equal to the current tip when the
/// range is empty). Does not move any ref -- the caller advances the branch
/// and worktree onto the returned sha. Runs against `repo`'s shared object
/// DB, so the new objects are visible from every linked worktree.
pub fn stamp_commit_messages(
    repo: &Path,
    base: &str,
    branch: &str,
    trailers: &[(String, String)],
) -> Result<String, TrailerError> {
    let range = format!("{base}..{branch}");
    let revs = run(
        repo,
        &["rev-list", "--topo-order", "--reverse", range.as_str()],
        None,
        &[],
    )?;
    let old_tip = run(repo, &["rev-parse", branch], None, &[])?;
    let revs: Vec<&str> = revs.split_whitespace().collect();
    if revs.is_empty() {
        return Ok(old_tip);
    }
    let mut remap: std::collections::BTreeMap<String, String> = Default::default();
    for sha in revs {
        let record = read_commit(repo, sha)?;
        let message = stamped_message(repo, &record.body, trailers)?;
        let mut args: Vec<String> = vec!["commit-tree".to_owned(), record.tree.clone()];
        // Parents inside the rewritten range are remapped to their new shas;
        // the base (and any parent outside the range) is left as-is, so
        // `base` stays the root ancestor and the fast-forward holds.
        for parent in &record.parents {
            args.push("-p".to_owned());
            args.push(remap.get(parent).cloned().unwrap_or_else(|| parent.clone()));
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let new_sha = run(repo, &arg_refs, Some(&message), &record.identity_env())?;
        remap.insert(sha.to_owned(), new_sha);
    }
    Ok(remap
        .get(&old_tip)
        .cloned()
        .unwrap_or_else(|| old_tip.clone()))
}

/// One commit's rewrite inputs, read in a single `git log -1` call.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitRecord {
    parents: Vec<String>,
    tree: String,
    author_name: String,
    author_email: String,
    author_date: String,
    committer_name: String,
    committer_email: String,
    committer_date: String,
    /// The raw `%B` body exactly as git printed it (trailing newline kept).
    body: String,
}

impl CommitRecord {
    /// The environment that reproduces the commit's author and committer
    /// identity (name, email, and date, timezone included) for
    /// `git commit-tree`, layered over the current process env so git still
    /// resolves `PATH` etc.
    fn identity_env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("GIT_AUTHOR_NAME", self.author_name.clone()),
            ("GIT_AUTHOR_EMAIL", self.author_email.clone()),
            ("GIT_AUTHOR_DATE", self.author_date.clone()),
            ("GIT_COMMITTER_NAME", self.committer_name.clone()),
            ("GIT_COMMITTER_EMAIL", self.committer_email.clone()),
            ("GIT_COMMITTER_DATE", self.committer_date.clone()),
        ]
    }
}

fn read_commit(repo: &Path, sha: &str) -> Result<CommitRecord, TrailerError> {
    let format = format!("--format={COMMIT_FORMAT}");
    let args = ["log", "-1", format.as_str(), sha];
    let raw = run_raw(repo, &args, None, &[])?;
    let fields: Vec<&str> = raw.splitn(9, FIELD_SEP).collect();
    if fields.len() != 9 {
        return Err(TrailerError::Malformed {
            repo: repo.display().to_string(),
            args: args.join(" "),
            detail: format!("expected 9 NUL-separated fields, got {}", fields.len()),
        });
    }
    Ok(CommitRecord {
        parents: fields[0].split_whitespace().map(str::to_owned).collect(),
        tree: fields[1].trim().to_owned(),
        author_name: fields[2].to_owned(),
        author_email: fields[3].to_owned(),
        author_date: fields[4].to_owned(),
        committer_name: fields[5].to_owned(),
        committer_email: fields[6].to_owned(),
        committer_date: fields[7].to_owned(),
        body: fields[8].to_owned(),
    })
}

/// The commit's message with agent `Flywheel-*` trailers stripped and the
/// authoritative `trailers` appended as a well-formed trailer block.
///
/// `raw_body` is the `%B` render of the message. Lines are split and
/// re-joined with `\n` exactly as the Python did (dropping the render's
/// trailing newline), then handed to `git interpret-trailers`, so the
/// resulting message is byte-identical to the Python stamp.
fn stamped_message(
    repo: &Path,
    raw_body: &str,
    trailers: &[(String, String)],
) -> Result<String, TrailerError> {
    let stripped = raw_body
        .lines()
        .filter(|line| !FLYWHEEL_TRAILER_LINE.is_match(line))
        .collect::<Vec<_>>()
        .join("\n");
    let mut args: Vec<String> = vec![
        "interpret-trailers".to_owned(),
        "--if-exists".to_owned(),
        "replace".to_owned(),
        "--if-missing".to_owned(),
        "add".to_owned(),
    ];
    for (key, value) in trailers {
        args.push("--trailer".to_owned());
        args.push(format!("{key}: {value}"));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_raw(repo, &arg_refs, Some(&stripped), &[])
}

/// Run a git command and return its stdout stripped of trailing whitespace
/// (the common case: a sha, a field list, a rev range).
fn run(
    repo: &Path,
    args: &[&str],
    input: Option<&str>,
    env: &[(&str, String)],
) -> Result<String, TrailerError> {
    Ok(run_raw(repo, args, input, env)?.trim().to_owned())
}

/// Run `git -C <repo> <args>` and return raw stdout. A non-zero exit is an
/// error: a failure here is unexpected (the caller holds the merge lock and
/// has already validated the branch), so it surfaces rather than silently
/// mis-stamping.
fn run_raw(
    repo: &Path,
    args: &[&str],
    input: Option<&str>,
    env: &[(&str, String)],
) -> Result<String, TrailerError> {
    let joined = args.join(" ");
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().map_err(|source| TrailerError::Spawn {
        repo: repo.display().to_string(),
        args: joined.clone(),
        source,
    })?;
    if let Some(text) = input
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin
            .write_all(text.as_bytes())
            .map_err(|source| TrailerError::Spawn {
                repo: repo.display().to_string(),
                args: joined.clone(),
                source,
            })?;
    }
    let output = child
        .wait_with_output()
        .map_err(|source| TrailerError::Spawn {
            repo: repo.display().to_string(),
            args: joined.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(TrailerError::Git {
            repo: repo.display().to_string(),
            args: joined,
            code: output
                .status
                .code()
                .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn git_ok(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .expect("git spawns");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn init_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git_ok(path, &["init", "-b", "main"]);
        git_ok(path, &["config", "user.email", "trailer-test@example.com"]);
        git_ok(path, &["config", "user.name", "trailer test"]);
        git_ok(path, &["config", "commit.gpgsign", "false"]);
        fs::write(path.join("README.md"), "base\n").unwrap();
        git_ok(path, &["add", "-A"]);
        git_ok(path, &["commit", "-m", "init"]);
    }

    fn commit(repo: &Path, file: &str, body: &str, messages: &[&str]) -> String {
        fs::write(repo.join(file), body).unwrap();
        git_ok(repo, &["add", "-A"]);
        let mut args = vec!["commit"];
        for part in messages {
            args.push("-m");
            args.push(part);
        }
        git_ok(repo, &args);
        git_ok(repo, &["rev-parse", "HEAD"])
    }

    fn trailer_value(repo: &Path, sha: &str, key: &str) -> String {
        git_ok(
            repo,
            &[
                "show",
                "-s",
                &format!("--format=%(trailers:key={key},valueonly)"),
                sha,
            ],
        )
    }

    fn trailer_count(repo: &Path, sha: &str, key: &str) -> usize {
        let body = git_ok(repo, &["show", "-s", "--format=%B", sha]);
        let prefix = format!("{}:", key.to_lowercase());
        body.lines()
            .filter(|l| l.trim().to_lowercase().starts_with(&prefix))
            .count()
    }

    fn range_shas(repo: &Path, range: &str) -> Vec<String> {
        let out = git_ok(repo, &["rev-list", "--reverse", range]);
        out.split_whitespace().map(str::to_owned).collect()
    }

    fn trailers() -> Vec<(String, String)> {
        provenance_trailers("t1", "run-1", "01-phase")
    }

    fn assert_stamped(repo: &Path, sha: &str) {
        for (key, value) in trailers() {
            assert_eq!(trailer_value(repo, sha, &key), value, "{key} on {sha}");
            assert_eq!(trailer_count(repo, sha, &key), 1, "{key} count on {sha}");
        }
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo");
        init_repo(&path);
        (dir, path)
    }

    #[test]
    fn provenance_trailers_follow_key_order() {
        let t = trailers();
        let keys: Vec<&str> = t.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, PROVENANCE_TRAILER_KEYS.to_vec());
        assert_eq!(t[0].1, "t1");
        assert_eq!(t[1].1, "run-1");
        assert_eq!(t[2].1, "01-phase");
    }

    #[test]
    fn stamps_every_commit_in_range_and_keeps_trees() {
        let (_dir, repo) = repo();
        let base = git_ok(&repo, &["rev-parse", "main"]);
        git_ok(&repo, &["checkout", "-q", "-b", "work"]);
        commit(&repo, "a.txt", "1", &["feat: a"]);
        commit(&repo, "b.txt", "2", &["feat: b"]);
        commit(&repo, "c.txt", "3", &["feat: c"]);
        let pre = range_shas(&repo, "main..work");
        let pre_trees: Vec<String> = pre
            .iter()
            .map(|c| git_ok(&repo, &["rev-parse", &format!("{c}^{{tree}}")]))
            .collect();

        let tip = stamp_commit_messages(&repo, "main", "work", &trailers()).unwrap();
        assert_ne!(tip, pre[2]);
        // The branch ref is NOT moved by the stamp; the caller does that.
        assert_eq!(git_ok(&repo, &["rev-parse", "work"]), pre[2]);

        let post = range_shas(&repo, &format!("{base}..{tip}"));
        assert_eq!(post.len(), 3);
        let post_trees: Vec<String> = post
            .iter()
            .map(|c| git_ok(&repo, &["rev-parse", &format!("{c}^{{tree}}")]))
            .collect();
        assert_eq!(post_trees, pre_trees);
        for sha in &post {
            assert_stamped(&repo, sha);
        }
        assert_eq!(
            git_ok(&repo, &["show", "-s", "--format=%s", &post[0]]),
            "feat: a"
        );
        // The base stays the root ancestor so a fast-forward holds.
        assert!(
            Command::new("git")
                .args([
                    "-C",
                    repo.to_str().unwrap(),
                    "merge-base",
                    "--is-ancestor",
                    &base,
                    &tip
                ])
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn preserves_author_and_committer_identity_and_dates() {
        let (_dir, repo) = repo();
        git_ok(&repo, &["checkout", "-q", "-b", "work"]);
        let sha = {
            fs::write(repo.join("a.txt"), "1").unwrap();
            git_ok(&repo, &["add", "-A"]);
            let out = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["commit", "-m", "feat: dated"])
                .env("GIT_AUTHOR_NAME", "Agent Author")
                .env("GIT_AUTHOR_EMAIL", "agent@example.com")
                .env("GIT_AUTHOR_DATE", "2024-01-02T03:04:05+02:00")
                .env("GIT_COMMITTER_NAME", "Agent Committer")
                .env("GIT_COMMITTER_EMAIL", "committer@example.com")
                .env("GIT_COMMITTER_DATE", "2024-01-02T03:04:06+02:00")
                .output()
                .unwrap();
            assert!(out.status.success());
            git_ok(&repo, &["rev-parse", "HEAD"])
        };
        let before = git_ok(
            &repo,
            &["show", "-s", "--format=%an|%ae|%aI|%cn|%ce|%cI", &sha],
        );
        let tip = stamp_commit_messages(&repo, "main", "work", &trailers()).unwrap();
        let after = git_ok(
            &repo,
            &["show", "-s", "--format=%an|%ae|%aI|%cn|%ce|%cI", &tip],
        );
        assert_eq!(after, before);
        assert_eq!(
            after,
            "Agent Author|agent@example.com|2024-01-02T03:04:05+02:00|Agent Committer|committer@example.com|2024-01-02T03:04:06+02:00"
        );
    }

    #[test]
    fn forged_trailers_are_stripped_and_replaced() {
        let (_dir, repo) = repo();
        git_ok(&repo, &["checkout", "-q", "-b", "work"]);
        commit(
            &repo,
            "a.txt",
            "1",
            &[
                "feat: launder provenance",
                "Flywheel-Run: forged-123\nflywheel-task: forged-task\n  Flywheel-Phase : forged-phase",
            ],
        );
        let tip = stamp_commit_messages(&repo, "main", "work", &trailers()).unwrap();
        assert_stamped(&repo, &tip);
        let body = git_ok(&repo, &["show", "-s", "--format=%B", &tip]);
        for forged in ["forged-123", "forged-task", "forged-phase"] {
            assert!(!body.contains(forged), "{forged} survived in {body:?}");
        }
    }

    #[test]
    fn already_correct_trailers_are_normalized_not_duplicated() {
        let (_dir, repo) = repo();
        git_ok(&repo, &["checkout", "-q", "-b", "work"]);
        commit(
            &repo,
            "a.txt",
            "1",
            &[
                "feat: already stamped",
                "Flywheel-Task: t1\nFlywheel-Run: run-1\nFlywheel-Phase: 01-phase",
            ],
        );
        let tip = stamp_commit_messages(&repo, "main", "work", &trailers()).unwrap();
        assert_stamped(&repo, &tip);
    }

    #[test]
    fn stamped_message_is_byte_identical_to_the_python_rendering() {
        // The Python port: `%B` render -> splitlines -> join("\n") ->
        // interpret-trailers. For a plain subject the result is the subject,
        // a blank line, and the three trailer lines.
        let (_dir, repo) = repo();
        let message = stamped_message(&repo, "feat: a\n\n", &trailers()).unwrap();
        assert_eq!(
            message,
            "feat: a\n\nFlywheel-Task: t1\nFlywheel-Run: run-1\nFlywheel-Phase: 01-phase\n"
        );
        let with_body = stamped_message(
            &repo,
            "feat: a\n\nSome body.\n\nFlywheel-Run: forged\nSigned-off-by: x <x@y>\n\n",
            &trailers(),
        )
        .unwrap();
        assert_eq!(
            with_body,
            "feat: a\n\nSome body.\n\nSigned-off-by: x <x@y>\nFlywheel-Task: t1\nFlywheel-Run: run-1\nFlywheel-Phase: 01-phase\n"
        );
    }

    #[test]
    fn empty_range_returns_the_current_tip_untouched() {
        let (_dir, repo) = repo();
        git_ok(&repo, &["checkout", "-q", "-b", "work"]);
        let tip_before = git_ok(&repo, &["rev-parse", "work"]);
        let tip = stamp_commit_messages(&repo, "main", "work", &trailers()).unwrap();
        assert_eq!(tip, tip_before);
    }

    #[test]
    fn unknown_branch_is_a_git_error() {
        let (_dir, repo) = repo();
        let err = stamp_commit_messages(&repo, "main", "nope", &trailers()).unwrap_err();
        assert!(matches!(err, TrailerError::Git { .. }), "{err}");
        assert!(err.to_string().contains("rev-list"));
    }

    #[test]
    fn missing_repo_is_a_git_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let err = stamp_commit_messages(&dir.path().join("absent"), "main", "work", &trailers())
            .unwrap_err();
        assert!(matches!(err, TrailerError::Git { .. }), "{err}");
    }
}
