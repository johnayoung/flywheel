//! Review against real git repositories in temp dirs (a bare repository
//! as the push remote; scripted stand-ins for `gh`).

use super::*;
use super::{actions::*, integrate::*, model::*, plan::*, suspects::LARGE_FILE_BYTES};
use crate::git;
use std::collections::HashMap;
use std::process::Command;

fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn write(dir: &Path, file: &str, text: &str) {
    let path = dir.join(file);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

fn commit_file(dir: &Path, file: &str, text: &str, message: &str) {
    write(dir, file, text);
    run_git(dir, &["add", "--", file]);
    run_git(dir, &["commit", "-q", "-m", message]);
}

/// `<tmp>/code/app` (main, one commit; a local identity so the hub's own
/// commits work without a global one) and a worktree `feat` cut from main.
struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    wt: WorktreeRef,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("code/app");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q", "-b", "main"]);
        run_git(&repo, &["config", "user.name", "Hub Test"]);
        run_git(&repo, &["config", "user.email", "hub@example.com"]);
        commit_file(&repo, "README", "one\ntwo\nthree\n", "init");
        let plan = git::plan(&repo, "feat", "", None).unwrap();
        let wt = git::create(&repo, &plan).unwrap();
        Self {
            _tmp: tmp,
            root,
            repo,
            wt,
        }
    }

    fn target(&self) -> Target {
        Target {
            dir: self.wt.path.clone(),
            base: Some(self.wt.base.clone()),
            base_source: BaseSource::Worktree,
            worktree: Some(self.wt.clone()),
        }
    }

    /// A bare repository as `origin`.
    fn remote(&self) -> PathBuf {
        let bare = self.root.join("remote.git");
        run_git(
            &self.root,
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        run_git(
            &self.repo,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        );
        bare
    }
}

fn tools() -> Tools {
    Tools::default()
}

#[test]
fn a_review_lists_commits_files_and_the_working_tree_apart() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    commit_file(wt, "src/lib.rs", "fn a() {}\nfn b() {}\n", "feat: add lib");
    write(wt, "README", "one\n2\nthree\nfour\n");
    run_git(wt, &["commit", "-qam", "docs: readme"]);
    run_git(wt, &["mv", "src/lib.rs", "src/core.rs"]);
    run_git(wt, &["commit", "-qm", "refactor: rename"]);
    std::fs::write(wt.join("blob.bin"), [0u8, 1, 2, 0, 255, 0]).unwrap();
    run_git(wt, &["add", "blob.bin"]);
    run_git(wt, &["commit", "-qm", "chore: binary"]);
    // Working tree: one staged, one unstaged, one untracked.
    write(wt, "staged.txt", "s\n");
    run_git(wt, &["add", "staged.txt"]);
    write(wt, "README", "one\n2\nthree\nfour\nfive\n");
    write(wt, "notes/todo.md", "x\n");

    let r = read(&tools(), &f.target()).unwrap();
    assert_eq!(r.branch.as_deref(), Some("feat"));
    assert_eq!((r.ahead, r.behind), (4, 0));
    let subjects: Vec<_> = r.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(
        subjects,
        [
            "chore: binary",
            "refactor: rename",
            "docs: readme",
            "feat: add lib"
        ]
    );
    let base = r.base.as_ref().unwrap();
    assert_eq!(base.name, "main");
    assert!(base.is_branch);
    assert_eq!(base.source, BaseSource::Worktree);
    // Committed: the branch against its merge base (the rename collapses
    // into one added file, as base..HEAD sees it).
    let by_path: HashMap<_, _> = r
        .committed
        .files
        .iter()
        .map(|f| (f.path.as_str(), f))
        .collect();
    assert_eq!(by_path["README"].status, "modified");
    assert_eq!(
        (by_path["README"].insertions, by_path["README"].deletions),
        (Some(2), Some(1))
    );
    assert_eq!(by_path["src/core.rs"].status, "added");
    assert_eq!(by_path["src/core.rs"].insertions, Some(2));
    assert!(by_path["blob.bin"].binary);
    assert_eq!(by_path["blob.bin"].insertions, None);
    assert_eq!(r.committed.insertions, 4);
    assert_eq!(r.committed.deletions, 1);
    // Working tree, apart.
    assert_eq!(r.staged.files.len(), 1);
    assert_eq!(r.staged.files[0].path, "staged.txt");
    assert_eq!(r.staged.files[0].status, "added");
    assert_eq!(r.unstaged.files.len(), 1);
    assert_eq!(r.unstaged.files[0].path, "README");
    assert_eq!(r.unstaged.files[0].insertions, Some(1));
    assert_eq!(
        r.untracked,
        [Untracked {
            path: "notes/todo.md".into()
        }]
    );
    assert!(r.conflicts.is_empty());
    assert_eq!(r.operation, None);
    assert!(!r.clean());
}

#[test]
fn renames_within_one_range_keep_their_source() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    run_git(wt, &["mv", "README", "README.md"]);
    let r = read(&tools(), &f.target()).unwrap();
    let staged = &r.staged.files;
    assert_eq!(staged.len(), 1, "{staged:?}");
    assert_eq!(staged[0].status, "renamed");
    assert_eq!(staged[0].path, "README.md");
    assert_eq!(staged[0].old_path.as_deref(), Some("README"));
    assert_eq!(
        (staged[0].insertions, staged[0].deletions),
        (Some(0), Some(0))
    );
}

#[test]
fn diffs_page_and_cover_every_scope() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let big: String = (0..5000).map(|n| format!("line {n}\n")).collect();
    commit_file(wt, "big.txt", &big, "big");
    write(wt, "README", "one\nTWO\nthree\n");
    write(wt, "new.txt", "fresh\n");
    std::fs::write(wt.join("raw.bin"), [0u8, 159, 146, 150, 0]).unwrap();
    let t = f.target();

    let first = diff(&tools(), &t, Scope::Committed, "big.txt", None, 0).unwrap();
    assert!(
        first.lines[0].starts_with("diff --git a/big.txt b/big.txt"),
        "{:?}",
        &first.lines[..3]
    );
    assert_eq!(first.lines.len(), DIFF_PAGE_LINES);
    assert_eq!(first.next, Some(DIFF_PAGE_LINES));
    assert!(!first.binary && !first.truncated);
    let mut seen = first.lines.len();
    let mut next = first.next;
    while let Some(offset) = next {
        let page = diff(&tools(), &t, Scope::Committed, "big.txt", None, offset).unwrap();
        assert_eq!(page.offset, offset);
        seen += page.lines.len();
        next = page.next;
    }
    assert_eq!(seen, first.total_lines);
    assert!(seen > 5000);

    let unstaged = diff(&tools(), &t, Scope::Unstaged, "README", None, 0).unwrap();
    assert!(
        unstaged.lines.contains(&"-two".to_owned()),
        "{:?}",
        unstaged.lines
    );
    assert!(unstaged.lines.contains(&"+TWO".to_owned()));
    assert_eq!(unstaged.next, None);

    let untracked = diff(&tools(), &t, Scope::Untracked, "new.txt", None, 0).unwrap();
    assert!(
        untracked.lines.contains(&"+fresh".to_owned()),
        "{:?}",
        untracked.lines
    );
    let binary = diff(&tools(), &t, Scope::Untracked, "raw.bin", None, 0).unwrap();
    assert!(binary.binary, "{:?}", binary.lines);

    // Nothing outside the work tree, nothing that is no path.
    for bad in ["../code/app/README", "/etc/passwd", "", "a/../../x"] {
        assert!(
            diff(&tools(), &t, Scope::Unstaged, bad, None, 0).is_err(),
            "{bad:?}"
        );
        assert!(
            diff(&tools(), &t, Scope::Untracked, bad, None, 0).is_err(),
            "{bad:?}"
        );
    }
    // A pathspec-looking name is taken literally: no file is named `*`.
    let none = diff(&tools(), &t, Scope::Unstaged, "*", None, 0).unwrap_err();
    assert!(none.contains("has no unstaged change"), "{none}");
    // A path the scope does not change is an error, not an empty diff;
    // so is a tracked file asked for as untracked.
    let err = diff(&tools(), &t, Scope::Committed, "nonexistent", None, 0).unwrap_err();
    assert!(err.contains("has no committed change"), "{err}");
    assert!(diff(&tools(), &t, Scope::Staged, "README", None, 0).is_err());
    assert!(diff(&tools(), &t, Scope::Untracked, "README", None, 0).is_err());
}

#[test]
fn a_renamed_file_is_diffed_as_the_rename_by_either_name() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    run_git(wt, &["mv", "README", "README2.md"]);
    run_git(wt, &["commit", "-qm", "rename"]);
    let t = f.target();
    let r = read(&tools(), &t).unwrap();
    assert_eq!(r.committed.files[0].status, "renamed");
    for asked in ["README2.md", "README"] {
        let page = diff(&tools(), &t, Scope::Committed, asked, None, 0).unwrap();
        assert_eq!(page.path, "README2.md");
        assert!(
            page.lines.iter().any(|l| l == "rename from README"),
            "{asked}: {:?}",
            page.lines
        );
        assert!(
            !page.lines.iter().any(|l| l.starts_with("new file mode")),
            "{asked}: {:?}",
            page.lines
        );
    }
}

#[test]
fn commit_all_runs_what_was_confirmed_and_refuses_a_moved_tree() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    let clean = plan_commit(&tools(), &t, "x", None).unwrap();
    assert!(
        clean
            .refusal
            .as_deref()
            .unwrap()
            .contains("nothing to commit")
    );
    assert!(commit(&tools(), &t, "x", None, &clean.fingerprint).is_err());

    write(wt, "README", "changed\n");
    write(wt, "added.txt", "a\n");
    let plan = plan_commit(&tools(), &t, "feat: change things\n\nBody line.", None).unwrap();
    assert_eq!(plan.refusal, None);
    let shown: Vec<_> = plan.commands.iter().map(|c| c.display.clone()).collect();
    let dir = wt.display().to_string();
    assert_eq!(
        shown,
        [
            format!("git -C {dir} add -A"),
            format!("git -C {dir} commit -F - < the message"),
        ]
    );
    assert_eq!(plan.details.len(), 2, "{:?}", plan.details);
    // No fingerprint, or another one: nothing runs.
    assert!(matches!(
        commit(&tools(), &t, "feat: change things\n\nBody line.", None, ""),
        Err(ActionError::Invalid(_))
    ));
    // The tree moves after the plan: stale.
    write(wt, "late.txt", "late\n");
    let stale = commit(
        &tools(),
        &t,
        "feat: change things\n\nBody line.",
        None,
        &plan.fingerprint,
    )
    .unwrap_err();
    assert!(stale.to_string().contains("changed since"), "{stale}");
    assert_eq!(run_git(wt, &["rev-list", "--count", "main..HEAD"]), "0");

    // An edit keeping the same files and line counts, and an untracked
    // file rewritten, after the plan: stale too (contents are hashed).
    let plan = plan_commit(&tools(), &t, "feat: change things\n\nBody line.", None).unwrap();
    write(wt, "README", "chanGED\n");
    let stale = commit(
        &tools(),
        &t,
        "feat: change things\n\nBody line.",
        None,
        &plan.fingerprint,
    )
    .unwrap_err();
    assert!(stale.to_string().contains("changed since"), "{stale}");
    let plan = plan_commit(&tools(), &t, "feat: change things\n\nBody line.", None).unwrap();
    write(wt, "late.txt", "LATE\n");
    let stale = commit(
        &tools(),
        &t,
        "feat: change things\n\nBody line.",
        None,
        &plan.fingerprint,
    )
    .unwrap_err();
    assert!(stale.to_string().contains("changed since"), "{stale}");
    assert_eq!(run_git(wt, &["rev-list", "--count", "main..HEAD"]), "0");

    let plan = plan_commit(&tools(), &t, "feat: change things\n\nBody line.", None).unwrap();
    let done = commit(
        &tools(),
        &t,
        "feat: change things\n\nBody line.",
        None,
        &plan.fingerprint,
    )
    .unwrap();
    assert!(done.ok);
    assert!(
        done.summary.starts_with("Committed 3 files as "),
        "{}",
        done.summary
    );
    assert_eq!(
        run_git(wt, &["log", "-1", "--format=%B"]),
        "feat: change things\n\nBody line."
    );
    assert_eq!(run_git(wt, &["status", "--porcelain"]), "");
    let r = read(&tools(), &t).unwrap();
    assert!(r.clean());
    assert_eq!(r.ahead, 1);
}

#[test]
fn push_goes_to_the_bare_remote_with_upstream_and_is_never_forced() {
    let f = Fixture::new();
    let bare = f.remote();
    let wt = &f.wt.path;
    let t = f.target();
    commit_file(wt, "a.txt", "a\n", "feat: a");

    let plan = plan_push(&tools(), &t, None).unwrap();
    assert_eq!(plan.refusal, None);
    let head = run_git(wt, &["rev-parse", "HEAD"]);
    // The planned commit, then the upstream (push -u sets none for an id).
    assert_eq!(
        plan.commands[0].display,
        format!("git -C {} push origin {head}:refs/heads/feat", wt.display())
    );
    assert_eq!(
        plan.commands[1].args,
        ["branch", "--set-upstream-to=origin/feat", "feat"]
    );
    assert!(
        plan.details[0].contains("creates origin/feat"),
        "{:?}",
        plan.details
    );
    assert!(
        plan.summary.contains(bare.to_str().unwrap()),
        "{}",
        plan.summary
    );
    let done = push(&tools(), &t, None, &plan.fingerprint).unwrap();
    assert!(done.ok);
    assert_eq!(
        run_git(&bare, &["rev-parse", "refs/heads/feat"]),
        run_git(wt, &["rev-parse", "HEAD"])
    );
    let r = read(&tools(), &t).unwrap();
    let up = r.upstream.unwrap();
    assert_eq!(
        (up.name.as_str(), up.ahead, up.behind),
        ("origin/feat", 0, 0)
    );

    // Up to date: nothing to push.
    let again = plan_push(&tools(), &t, None).unwrap();
    assert!(again.refusal.as_deref().unwrap().contains("up to date"));

    // Tracked now: no -u. The remote moved on meanwhile: the push is
    // refused by git (never forced) and says so.
    commit_file(wt, "b.txt", "b\n", "feat: b");
    let other = f.root.join("other");
    run_git(
        &f.root,
        &[
            "clone",
            "-q",
            "-b",
            "feat",
            bare.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    commit_file(&other, "c.txt", "c\n", "elsewhere");
    run_git(&other, &["push", "-q", "origin", "feat"]);
    run_git(wt, &["fetch", "-q", "origin"]);
    let plan = plan_push(&tools(), &t, None).unwrap();
    let head = run_git(wt, &["rev-parse", "HEAD"]);
    assert_eq!(
        plan.commands[0].args,
        ["push", "origin", &format!("{head}:refs/heads/feat")]
    );
    assert_eq!(plan.commands.len(), 1, "tracked already: no upstream step");
    assert!(
        plan.details.iter().any(|d| d.contains("never forced")),
        "{:?}",
        plan.details
    );
    let err = push(&tools(), &t, None, &plan.fingerprint).unwrap_err();
    assert!(err.to_string().contains("rejected"), "{err}");
    assert!(!plan.commands.iter().any(|c| {
        c.args
            .iter()
            .any(|a| a.contains("force") || a.starts_with('+'))
    }));

    // An unknown remote is refused before git runs.
    let plan = plan_push(&tools(), &t, Some("nope")).unwrap();
    assert!(plan.refusal.unwrap().contains("no remote named"));
}

#[test]
fn push_needs_a_remote() {
    let f = Fixture::new();
    commit_file(&f.wt.path, "a.txt", "a\n", "a");
    let plan = plan_push(&tools(), &f.target(), None).unwrap();
    assert!(plan.refusal.unwrap().contains("no remotes"));
    assert!(plan.commands.is_empty());
}

/// A stand-in `gh`: `auth status` exits `auth` ; `pr create` records its
/// argv and stdin and prints a URL.
fn fake_gh(dir: &Path, auth: u8) -> String {
    let path = dir.join("gh");
    let log = dir.join("gh.log");
    crate::test_support::write_script(
        &path,
        &format!(
            "#!/bin/sh\n\
             if [ \"$1\" = auth ]; then echo 'not logged in' >&2; exit {auth}; fi\n\
             printf '%s\\n' \"$@\" > '{log}'\n\
             cat > '{log}.stdin'\n\
             echo 'Creating pull request...'\n\
             echo 'https://github.com/o/r/pull/7'\n",
            log = log.display()
        ),
    );
    path.to_string_lossy().into_owned()
}

#[test]
fn a_pull_request_needs_gh_signed_in_and_the_branch_pushed() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    commit_file(wt, "a.txt", "a\n", "feat: one thing");
    let request = PrRequest::default();

    // No remote at all: said so (never "not pushed to  yet").
    let plan = plan_pr(&tools(), &t, &request).unwrap();
    assert!(
        plan.refusal.as_deref().unwrap().contains("no remotes"),
        "{:?}",
        plan.refusal
    );
    f.remote();
    let t = f.target();
    // An option-looking base is no branch name.
    let dashed = PrRequest {
        base: Some("--help".into()),
        ..PrRequest::default()
    };
    let plan = plan_pr(&tools(), &t, &dashed).unwrap();
    assert!(
        plan.refusal
            .as_deref()
            .unwrap_or_default()
            .contains("not a branch name"),
        "{:?}",
        plan.refusal
    );

    // Not pushed yet.
    let plan = plan_pr(&tools(), &t, &request).unwrap();
    assert!(plan.refusal.unwrap().contains("not pushed"));
    run_git(wt, &["push", "-q", "-u", "origin", "feat"]);
    // A remote that is a local path has no host for gh.
    let plan = plan_pr(&tools(), &t, &request).unwrap();
    assert!(plan.refusal.unwrap().contains("not on a host"));
    // Pretend it is on GitHub (the tracking ref stays).
    run_git(
        &f.repo,
        &["remote", "set-url", "origin", "https://github.com/o/r.git"],
    );

    // gh not installed.
    let missing = Tools {
        gh: f.root.join("no-such-gh").to_string_lossy().into_owned(),
        ..Tools::default()
    };
    let plan = plan_pr(&missing, &t, &request).unwrap();
    assert!(
        plan.refusal.as_deref().unwrap().contains("not installed"),
        "{:?}",
        plan.refusal
    );
    assert!(pr(&missing, &t, &request, &plan.fingerprint).is_err());

    // gh installed, not signed in.
    let bin = f.root.join("bin-noauth");
    std::fs::create_dir_all(&bin).unwrap();
    let noauth = Tools {
        gh: fake_gh(&bin, 1),
        ..Tools::default()
    };
    let plan = plan_pr(&noauth, &t, &request).unwrap();
    assert!(
        plan.refusal.as_deref().unwrap().contains("not signed in"),
        "{:?}",
        plan.refusal
    );

    // Signed in: the defaults, the exact command, the URL.
    let bin = f.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let authed = Tools {
        gh: fake_gh(&bin, 0),
        ..Tools::default()
    };
    let plan = plan_pr(&authed, &t, &request).unwrap();
    assert_eq!(plan.refusal, None);
    let defaults = plan.defaults.clone().unwrap();
    assert_eq!(defaults.title, "feat: one thing");
    assert_eq!(defaults.base, "main");
    assert!(
        defaults.body.contains("- feat: one thing ("),
        "{}",
        defaults.body
    );
    assert_eq!(
        plan.commands[0].args,
        [
            "pr",
            "create",
            "--base",
            "main",
            "--head",
            "feat",
            "--title",
            "feat: one thing",
            "--body-file",
            "-"
        ]
    );
    assert!(
        plan.commands[0]
            .display
            .starts_with(&format!("(cd {} && gh pr create", wt.display()))
    );
    let request = PrRequest {
        title: Some("Better title".into()),
        body: Some("Why.\n".into()),
        base: None,
        draft: true,
    };
    let plan = plan_pr(&authed, &t, &request).unwrap();
    let done = pr(&authed, &t, &request, &plan.fingerprint).unwrap();
    assert_eq!(done.url.as_deref(), Some("https://github.com/o/r/pull/7"));
    let argv = std::fs::read_to_string(bin.join("gh.log")).unwrap();
    assert_eq!(
        argv.lines().collect::<Vec<_>>(),
        [
            "pr",
            "create",
            "--base",
            "main",
            "--head",
            "feat",
            "--title",
            "Better title",
            "--body-file",
            "-",
            "--draft"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(bin.join("gh.log.stdin")).unwrap(),
        "Why.\n"
    );
    // A multi-line title is refused before anything runs.
    let bad = PrRequest {
        title: Some("a\nb".into()),
        ..PrRequest::default()
    };
    assert!(matches!(
        pr(&authed, &t, &bad, &plan.fingerprint),
        Err(ActionError::Invalid(_))
    ));
}

#[test]
fn merge_fast_forwards_the_base_in_the_main_checkout() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    let nothing = plan_merge(&tools(), &t, false).unwrap();
    assert!(nothing.refusal.unwrap().contains("nothing to merge"));

    commit_file(wt, "a.txt", "a\n", "feat: a");
    commit_file(wt, "b.txt", "b\n", "feat: b");

    // The main checkout off its base: refused.
    run_git(&f.repo, &["checkout", "-q", "-b", "side"]);
    let plan = plan_merge(&tools(), &t, false).unwrap();
    assert!(plan.refusal.unwrap().contains("is on side, not main"));
    run_git(&f.repo, &["checkout", "-q", "main"]);

    // Dirty main checkout: refused (untracked files alone are not dirt).
    write(&f.repo, "README", "local edit\n");
    let plan = plan_merge(&tools(), &t, false).unwrap();
    assert!(plan.refusal.unwrap().contains("1 uncommitted change"));
    run_git(&f.repo, &["checkout", "--", "README"]);
    write(&f.repo, "scratch", "x\n");

    let plan = plan_merge(&tools(), &t, false).unwrap();
    assert_eq!(plan.refusal, None, "{plan:?}");
    assert!(!plan.needs_rebase);
    assert_eq!(
        plan.commands[0].display,
        format!(
            "git -C {} merge --ff-only {}",
            f.repo.display(),
            run_git(&f.repo, &["rev-parse", "refs/heads/feat"])
        )
    );
    assert_eq!(plan.details.len(), 2);
    // The main checkout moves meanwhile: stale.
    std::fs::remove_file(f.repo.join("scratch")).unwrap();
    commit_file(&f.repo, "moved.txt", "m\n", "main moves first");
    let stale = merge(&tools(), &t, false, &plan.fingerprint);
    assert!(stale.is_err());
    assert_eq!(
        run_git(&f.repo, &["rev-list", "--count", "HEAD..feat"]),
        "2"
    );

    // Base moved on: not a fast-forward, rebase first.
    commit_file(&f.repo, "main.txt", "m\n", "main moves");
    let plan = plan_merge(&tools(), &t, false).unwrap();
    assert!(plan.needs_rebase);
    assert!(
        plan.refusal
            .unwrap()
            .contains("rebase feat onto main first")
    );

    let rebase_plan = plan_rebase(&tools(), &t).unwrap();
    assert_eq!(rebase_plan.refusal, None);
    assert_eq!(
        rebase_plan.commands[0].display,
        format!("git -C {} rebase main", wt.display())
    );
    let done = rebase(&tools(), &t, &rebase_plan.fingerprint).unwrap();
    assert!(done.ok, "{done:?}");

    let plan = plan_merge(&tools(), &t, false).unwrap();
    assert_eq!(plan.refusal, None);
    let done = merge(&tools(), &t, false, &plan.fingerprint).unwrap();
    assert!(done.ok);
    assert_eq!(
        run_git(&f.repo, &["rev-parse", "main"]),
        run_git(wt, &["rev-parse", "HEAD"])
    );
    // Nothing lost by removing it now.
    let check = git::removal_check(&f.wt).unwrap();
    assert_eq!(check.refusal(), None);
}

#[test]
fn a_rebase_says_which_commits_the_base_already_had() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    // The branch's change is in the base already, inside a larger commit
    // (not the same patch): the replayed commit is empty, and git drops it.
    commit_file(wt, "same.txt", "x\n", "edit 1");
    write(&f.repo, "other.txt", "y\n");
    run_git(&f.repo, &["add", "--", "other.txt"]);
    commit_file(&f.repo, "same.txt", "x\n", "edit 1 and more");
    let plan = plan_rebase(&tools(), &t).unwrap();
    assert_eq!(plan.refusal, None);
    assert!(plan.summary.contains("1 commit"), "{}", plan.summary);
    let done = rebase(&tools(), &t, &plan.fingerprint).unwrap();
    assert!(done.ok, "{done:?}");
    assert!(
        done.summary
            .contains("1 commit was already in main and was dropped")
            && done.summary.contains("now equals main"),
        "{}",
        done.summary
    );
    assert!(!done.summary.contains("Replayed"), "{}", done.summary);
}

#[test]
fn a_rebase_stops_on_conflict_and_is_left_for_the_agent() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    commit_file(wt, "README", "one\nbranch\nthree\n", "branch edit");
    commit_file(&f.repo, "README", "one\nmain\nthree\n", "main edit");
    let plan = plan_rebase(&tools(), &t).unwrap();
    assert_eq!(plan.refusal, None);
    let done = rebase(&tools(), &t, &plan.fingerprint).unwrap();
    assert!(!done.ok);
    assert_eq!(done.conflicts, ["README"]);
    assert!(
        done.summary.contains("left in progress"),
        "{}",
        done.summary
    );
    let r = read(&tools(), &t).unwrap();
    assert_eq!(r.operation.as_deref(), Some("rebase"));
    assert_eq!(r.conflicts, ["README"]);
    // Committing is refused mid-rebase; so is another rebase.
    let commit_plan = plan_commit(&tools(), &t, "x", None).unwrap();
    assert!(
        commit_plan
            .refusal
            .unwrap()
            .contains("rebase is in progress")
    );
    assert!(
        plan_rebase(&tools(), &t)
            .unwrap()
            .refusal
            .unwrap()
            .contains("in progress")
    );
}

#[test]
fn merging_is_for_worktree_sessions_on_a_branch_base() {
    let f = Fixture::new();
    let plain = Target {
        dir: f.repo.clone(),
        base: None,
        base_source: BaseSource::Upstream,
        worktree: None,
    };
    assert!(
        plan_merge(&tools(), &plain, false)
            .unwrap()
            .refusal
            .unwrap()
            .contains("worktree sessions")
    );
    assert!(
        plan_rebase(&tools(), &plain)
            .unwrap()
            .refusal
            .unwrap()
            .contains("worktree sessions")
    );
    // A detached base (a commit) has no branch to merge into.
    let sha = run_git(&f.repo, &["rev-parse", "HEAD"]);
    let wt = WorktreeRef {
        base: sha.clone(),
        ..f.wt.clone()
    };
    commit_file(&f.wt.path, "a.txt", "a\n", "a");
    let t = Target {
        base: Some(sha),
        worktree: Some(wt),
        ..f.target()
    };
    assert!(
        plan_merge(&tools(), &t, false)
            .unwrap()
            .refusal
            .unwrap()
            .contains("is a commit, not a branch")
    );
    // Without a base or upstream, the review still shows the working tree.
    let r = read(&tools(), &plain).unwrap();
    assert!(r.base.is_none());
    assert!(r.commits.is_empty());
}

#[test]
fn credentials_in_remote_urls_are_masked_and_hosts_found() {
    assert_eq!(
        mask_url("https://u:tok@github.com/o/r.git"),
        "https://***@github.com/o/r.git"
    );
    assert_eq!(mask_url("git@github.com:o/r.git"), "git@github.com:o/r.git");
    assert_eq!(
        url_host("git@github.com:o/r.git").as_deref(),
        Some("github.com")
    );
    assert_eq!(
        url_host("https://***@gh.example.com/o/r").as_deref(),
        Some("gh.example.com")
    );
    assert_eq!(url_host("/srv/git/r.git"), None);
}

#[test]
fn one_action_at_a_time_per_repository() {
    let f = Fixture::new();
    let lock = RepoLock::take(&tools(), &f.wt.path).unwrap();
    // The main checkout shares the worktree's repository.
    let again = RepoLock::take(&tools(), &f.repo);
    assert!(matches!(again, Err(ActionError::Conflict(_))));
    drop(lock);
    assert!(RepoLock::take(&tools(), &f.repo).is_ok());
}

#[test]
fn a_partly_staged_file_counts_once() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    write(wt, "README", "one\ntwo\nthree\nx\n");
    run_git(wt, &["add", "README"]);
    write(wt, "README", "one\ntwo\nthree\nx\ny\n");
    let plan = plan_commit(&tools(), &t, "fix: one file", None).unwrap();
    assert_eq!(plan.summary, "Stage all 1 file and commit them on feat");
    let done = commit(&tools(), &t, "fix: one file", None, &plan.fingerprint).unwrap();
    assert!(
        done.summary.starts_with("Committed 1 file as "),
        "{}",
        done.summary
    );
}

#[test]
fn a_branch_tracking_another_name_is_pushed_to_its_own_name() {
    let f = Fixture::new();
    let bare = f.remote();
    run_git(&f.repo, &["push", "-q", "origin", "main"]);
    run_git(&f.repo, &["fetch", "-q", "origin"]);
    let main_before = run_git(&bare, &["rev-parse", "refs/heads/main"]);
    // A worktree cut from origin/main does not track it.
    let plan = git::plan(&f.repo, "task", "", Some("origin/main")).unwrap();
    let task = git::create(&f.repo, &plan).unwrap();
    assert!(
        Command::new("git")
            .args([
                "-C",
                task.path.to_str().unwrap(),
                "rev-parse",
                "@{upstream}"
            ])
            .output()
            .unwrap()
            .status
            .code()
            != Some(0),
        "a new worktree branch tracks nothing"
    );
    // A branch that does track origin/main (git checkout -b x origin/main).
    let wt = &f.wt.path;
    run_git(wt, &["branch", "--set-upstream-to", "origin/main"]);
    commit_file(wt, "a.txt", "a\n", "feat: a");
    let t = f.target();
    let plan = plan_push(&tools(), &t, None).unwrap();
    assert_eq!(plan.refusal, None, "{plan:?}");
    let head = run_git(wt, &["rev-parse", "HEAD"]);
    assert_eq!(
        plan.commands[0].args,
        ["push", "origin", &format!("{head}:refs/heads/feat")]
    );
    assert_eq!(
        plan.commands[1].args,
        ["branch", "--set-upstream-to=origin/feat", "feat"]
    );
    assert!(
        plan.details
            .iter()
            .any(|d| d.contains("it tracks origin/main now")),
        "{:?}",
        plan.details
    );
    push(&tools(), &t, None, &plan.fingerprint).unwrap();
    assert_eq!(
        run_git(&bare, &["rev-parse", "refs/heads/main"]),
        main_before
    );
    assert_eq!(
        run_git(&bare, &["rev-parse", "refs/heads/feat"]),
        run_git(wt, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        run_git(wt, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/feat"
    );

    // Another remote: pushed to its same-named branch, the upstream kept.
    let backup = f.root.join("backup.git");
    run_git(&f.root, &["init", "-q", "--bare", backup.to_str().unwrap()]);
    run_git(
        &f.repo,
        &["remote", "add", "backup", backup.to_str().unwrap()],
    );
    let plan = plan_push(&tools(), &t, Some("backup")).unwrap();
    let head = run_git(wt, &["rev-parse", "HEAD"]);
    assert_eq!(
        plan.commands[0].args,
        ["push", "backup", &format!("{head}:refs/heads/feat")]
    );
    assert_eq!(plan.commands.len(), 1, "the upstream is kept");
    assert!(
        plan.details
            .iter()
            .any(|d| d.contains("keeps tracking origin/feat")),
        "{:?}",
        plan.details
    );
    push(&tools(), &t, Some("backup"), &plan.fingerprint).unwrap();
    assert_eq!(
        run_git(wt, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/feat"
    );
}

#[test]
fn a_pull_request_opens_from_the_same_named_branch_as_confirmed() {
    let f = Fixture::new();
    f.remote();
    run_git(&f.repo, &["push", "-q", "origin", "main"]);
    let wt = &f.wt.path;
    commit_file(wt, "a.txt", "a\n", "feat: a");
    run_git(wt, &["push", "-q", "origin", "feat"]);
    run_git(wt, &["fetch", "-q", "origin"]);
    run_git(wt, &["branch", "--set-upstream-to", "origin/main"]);
    run_git(
        &f.repo,
        &["remote", "set-url", "origin", "https://github.com/o/r.git"],
    );
    let bin = f.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let authed = Tools {
        gh: fake_gh(&bin, 0),
        ..Tools::default()
    };
    let t = f.target();
    let a = PrRequest {
        title: Some("Title A".into()),
        draft: true,
        ..PrRequest::default()
    };
    let plan = plan_pr(&authed, &t, &a).unwrap();
    assert_eq!(plan.refusal, None, "{plan:?}");
    let head = plan.commands[0]
        .args
        .iter()
        .position(|x| x == "--head")
        .unwrap();
    assert_eq!(plan.commands[0].args[head + 1], "feat");
    // Confirming another title or draft than the plan showed: stale.
    for other in [
        PrRequest {
            title: Some("Title B".into()),
            draft: true,
            ..PrRequest::default()
        },
        PrRequest {
            title: Some("Title A".into()),
            draft: false,
            ..PrRequest::default()
        },
        PrRequest {
            body: Some("other body".into()),
            ..a.clone()
        },
    ] {
        let err = pr(&authed, &t, &other, &plan.fingerprint).unwrap_err();
        assert!(err.to_string().contains("changed since"), "{err}");
    }
    assert!(!bin.join("gh.log").exists());
    pr(&authed, &t, &a, &plan.fingerprint).unwrap();
    assert!(bin.join("gh.log").exists());
}

#[test]
fn a_stopped_rebase_can_be_aborted_as_confirmed() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    assert!(
        plan_abort(&tools(), &t)
            .unwrap()
            .refusal
            .unwrap()
            .contains("no rebase")
    );
    commit_file(wt, "README", "one\nbranch\nthree\n", "branch edit");
    let before = run_git(wt, &["rev-parse", "HEAD"]);
    commit_file(&f.repo, "README", "one\nmain\nthree\n", "main edit");
    let plan = plan_rebase(&tools(), &t).unwrap();
    assert!(!rebase(&tools(), &t, &plan.fingerprint).unwrap().ok);
    let r = read(&tools(), &t).unwrap();
    assert_eq!(r.branch, None);
    assert_eq!(r.rebasing.as_deref(), Some("feat"));
    let plan = plan_abort(&tools(), &t).unwrap();
    assert_eq!(plan.refusal, None);
    assert_eq!(plan.commands[0].args, ["rebase", "--abort"]);
    let done = abort(&tools(), &t, &plan.fingerprint).unwrap();
    assert!(done.ok, "{done:?}");
    let r = read(&tools(), &t).unwrap();
    assert_eq!(r.operation, None);
    assert_eq!(r.branch.as_deref(), Some("feat"));
    assert_eq!(run_git(wt, &["rev-parse", "HEAD"]), before);
}

#[test]
fn status_paths_cover_every_entry_kind() {
    let status = b"# branch.oid abc\x001 .M N... 100644 100644 100644 a a sub dir/one file\x002 R. N... 100644 100644 100644 a a R100 new name\x00old name\x00u UU N... 100644 100644 100644 100644 a b c both\x00? fresh one\x00! ignored\x00";
    assert_eq!(
        status_paths(status),
        ["sub dir/one file", "new name", "both", "fresh one"]
    );
}

/// A change landing after the confirm's re-plan but before `git add -A`
/// (an agent still writing) is not committed: the index goes back as it
/// was and nothing is committed.
#[test]
fn a_change_racing_the_staging_is_never_committed() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    write(wt, "README", "reviewed\n");
    write(wt, "staged.txt", "staged\n");
    run_git(wt, &["add", "staged.txt"]);
    let plan = plan_commit(&tools(), &t, "feat: reviewed", None).unwrap();
    assert_eq!(plan.refusal, None, "{plan:?}");
    let index_before = run_git(wt, &["write-tree"]);
    // The race: after the plan was checked, before staging.
    write(wt, "README", "unreviewed\n");
    let err = run_staged_as_planned(&tools(), &t, &plan).unwrap_err();
    assert!(err.to_string().contains("nothing was committed"), "{err}");
    assert_eq!(run_git(wt, &["rev-list", "--count", "main..HEAD"]), "0");
    assert_eq!(
        run_git(wt, &["write-tree"]),
        index_before,
        "the index is put back"
    );
    // A file appearing meanwhile is caught the same way.
    write(wt, "README", "reviewed\n");
    let plan = plan_commit(&tools(), &t, "feat: reviewed", None).unwrap();
    write(wt, "late.txt", "late\n");
    assert!(run_staged_as_planned(&tools(), &t, &plan).is_err());
    assert_eq!(run_git(wt, &["rev-list", "--count", "main..HEAD"]), "0");
    // Unraced, the plan commits exactly what it named.
    std::fs::remove_file(wt.join("late.txt")).unwrap();
    let plan = plan_commit(&tools(), &t, "feat: reviewed", None).unwrap();
    run_staged_as_planned(&tools(), &t, &plan).unwrap();
    assert_eq!(run_git(wt, &["rev-list", "--count", "main..HEAD"]), "1");
    assert_eq!(run_git(wt, &["status", "--porcelain"]), "");
}

/// A commit landing on the branch after the merge plan is not merged:
/// the plan fast-forwards to the commit it named.
#[test]
fn merge_runs_the_planned_commit_not_the_moving_branch() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    commit_file(wt, "a.txt", "a\n", "feat: a");
    let planned = run_git(wt, &["rev-parse", "HEAD"]);
    let plan = plan_merge(&tools(), &t, false).unwrap();
    assert_eq!(plan.refusal, None, "{plan:?}");
    commit_file(wt, "b.txt", "b\n", "unreviewed");
    // Run the plan as planned (what a confirm racing the commit runs).
    run_plan(&plan.commands).unwrap();
    assert_eq!(run_git(&f.repo, &["rev-parse", "HEAD"]), planned);
}

#[test]
fn commit_all_leaves_out_build_output_and_secrets_unless_told() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    write(wt, "calc.py", "def add(a, b):\n    return a + b\n");
    write(wt, "__pycache__/calc.cpython-313.pyc", "bytecode");
    write(wt, "app/.env", "TOKEN=hunter2\n");
    write(wt, ".env.example", "TOKEN=\n");
    std::fs::write(
        wt.join("big.bin"),
        vec![7u8; usize::try_from(LARGE_FILE_BYTES).unwrap()],
    )
    .unwrap();
    // A key someone staged by hand is left out too.
    write(wt, "certs/server.pem", "-----BEGIN PRIVATE KEY-----\n");
    run_git(wt, &["add", "--", "certs/server.pem"]);

    let plan = plan_commit(&tools(), &t, "feat: add", None).unwrap();
    assert_eq!(plan.refusal, None);
    let kinds: Vec<(&str, &str, bool)> = plan
        .suspects
        .iter()
        .map(|s| (s.path.as_str(), s.kind, s.excluded))
        .collect();
    let mut sorted = kinds.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        [
            ("__pycache__/", "generated", true),
            ("app/.env", "secret", true),
            ("big.bin", "large", false),
            ("certs/server.pem", "secret", true),
        ]
    );
    let mut excluded = plan.excluded.clone();
    excluded.sort();
    assert_eq!(excluded, ["__pycache__/", "app/.env", "certs/server.pem"]);
    assert!(
        plan.details
            .iter()
            .all(|d| !d.contains(".pyc") && !d.contains("app/.env") && !d.contains("server.pem")),
        "{:?}",
        plan.details
    );
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.starts_with("3 files left out")),
        "{:?}",
        plan.warnings
    );
    assert_eq!(
        plan.summary,
        "Stage 3 files (leaving 3 out) and commit them on feat"
    );
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.starts_with("big.bin looks large")),
        "{:?}",
        plan.warnings
    );
    let shown: Vec<_> = plan.commands.iter().map(|c| c.display.clone()).collect();
    assert_eq!(shown.len(), 3, "{shown:?}");
    assert!(
        shown[0].contains("add -A -- :/") && shown[0].contains("':(top,literal,exclude)app/.env'"),
        "{shown:?}"
    );
    assert!(
        shown[1].contains("reset -q -- ':(top,literal)certs/server.pem'"),
        "{shown:?}"
    );

    // Another exclusion than planned: another fingerprint.
    let none: Vec<String> = Vec::new();
    let all = plan_commit(&tools(), &t, "feat: add", Some(&none)).unwrap();
    assert_ne!(all.fingerprint, plan.fingerprint);
    assert!(all.suspects.iter().all(|s| !s.excluded));
    assert!(commit(&tools(), &t, "feat: add", Some(&none), &plan.fingerprint).is_err());
    // A path with no change cannot be left out.
    let odd = plan_commit(&tools(), &t, "feat: add", Some(&["README.md".to_owned()])).unwrap();
    assert!(
        odd.refusal
            .as_deref()
            .unwrap()
            .contains("README.md has no change"),
        "{:?}",
        odd.refusal
    );

    let done = commit(&tools(), &t, "feat: add", None, &plan.fingerprint).unwrap();
    assert!(done.ok, "{done:?}");
    let committed = run_git(wt, &["show", "--name-only", "--format=", "HEAD"]);
    let mut committed: Vec<&str> = committed.lines().collect();
    committed.sort_unstable();
    assert_eq!(committed, [".env.example", "big.bin", "calc.py"]);
    // Left in the work tree, untracked: nothing lost.
    let status = run_git(wt, &["status", "--porcelain", "--untracked-files=all"]);
    for path in [
        "__pycache__/calc.cpython-313.pyc",
        "app/.env",
        "certs/server.pem",
    ] {
        assert!(status.contains(&format!("?? {path}")), "{path}: {status}");
    }

    // Everything left out: nothing to commit.
    let only = plan_commit(
        &tools(),
        &t,
        "x",
        Some(&[
            "__pycache__/".to_owned(),
            "app/.env".to_owned(),
            "certs/server.pem".to_owned(),
        ]),
    )
    .unwrap();
    assert!(
        only.refusal
            .as_deref()
            .unwrap()
            .contains("every change is left out"),
        "{:?}",
        only.refusal
    );
}

#[test]
fn commit_all_leaves_a_staged_secret_out_of_a_first_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().canonicalize().unwrap();
    run_git(&repo, &["init", "-q", "-b", "main"]);
    run_git(&repo, &["config", "user.name", "Hub Test"]);
    run_git(&repo, &["config", "user.email", "hub@example.com"]);
    write(&repo, "main.rs", "fn main() {}\n");
    write(&repo, ".env", "KEY=1\n");
    run_git(&repo, &["add", "--", ".env"]);
    let t = Target {
        dir: repo.clone(),
        base: None,
        base_source: BaseSource::None,
        worktree: None,
    };
    let plan = plan_commit(&tools(), &t, "init", None).unwrap();
    assert_eq!(plan.refusal, None);
    assert!(
        plan.commands[1]
            .display
            .contains("rm --cached -q -r -- ':(top,literal).env'"),
        "{:?}",
        plan.commands
    );
    commit(&tools(), &t, "init", None, &plan.fingerprint).unwrap();
    assert_eq!(
        run_git(&repo, &["show", "--name-only", "--format=", "HEAD"]),
        "main.rs"
    );
    assert!(run_git(&repo, &["status", "--porcelain"]).contains("?? .env"));
}

#[test]
fn commit_all_classifies_every_change_past_the_review_cut() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    write(wt, "app.js", "console.log(1)\n");
    // More generated files than the review lists hold, and a secret and
    // a Cargo target sorted after all of them.
    for i in 0..(MAX_FILES + 200) {
        write(wt, &format!("node_modules/pkg/dist/m{i:05}.js"), "x");
    }
    write(wt, "zz/.env", "TOKEN=1\n");
    write(wt, "zz/target/x86_64-unknown-linux-musl/release/app", "elf");
    // Cargo tags every target directory it makes.
    write(
        wt,
        "zz/target/CACHEDIR.TAG",
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    );
    let plan = plan_commit(&tools(), &t, "feat: app", None).unwrap();
    assert_eq!(plan.refusal, None, "{plan:?}");
    let kinds: Vec<(&str, &str, bool, usize)> = plan
        .suspects
        .iter()
        .map(|s| (s.path.as_str(), s.kind, s.excluded, s.files))
        .collect();
    assert_eq!(
        kinds,
        [
            ("node_modules/", "generated", true, MAX_FILES + 200),
            ("zz/.env", "secret", true, 1),
            ("zz/target/", "generated", true, 2),
        ]
    );
    assert_eq!(
        plan.summary,
        format!(
            "Stage 1 file (leaving {} out) and commit it on feat",
            MAX_FILES + 203
        )
    );
    let done = commit(&tools(), &t, "feat: app", None, &plan.fingerprint).unwrap();
    assert!(done.ok, "{done:?}");
    assert_eq!(
        run_git(wt, &["show", "--name-only", "--format=", "HEAD"]),
        "app.js"
    );
    // A directory named for leaving out is checked against every change.
    let named = plan_commit(&tools(), &t, "x", Some(&["zz/target/".to_owned()])).unwrap();
    assert_eq!(named.refusal, None, "{:?}", named.refusal);
    assert_eq!(named.excluded, ["zz/target/"]);
}

#[test]
fn commit_all_unstages_a_staged_generated_directory() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    write(wt, "lib.py", "x = 1\n");
    write(wt, "pkg/__pycache__/lib.cpython-313.pyc", "bytecode");
    run_git(wt, &["add", "-A"]);
    let plan = plan_commit(&tools(), &t, "feat: lib", None).unwrap();
    assert_eq!(plan.excluded, ["pkg/__pycache__/"]);
    commit(&tools(), &t, "feat: lib", None, &plan.fingerprint).unwrap();
    assert_eq!(
        run_git(wt, &["show", "--name-only", "--format=", "HEAD"]),
        "lib.py"
    );
    assert!(
        run_git(wt, &["status", "--porcelain", "--untracked-files=all"])
            .contains("?? pkg/__pycache__/lib.cpython-313.pyc")
    );
}

#[test]
fn commit_all_past_the_review_cut_says_what_it_lists_and_leaves_a_late_secret_out() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    // 1500 plain source files (none generated): the review lists cut at
    // MAX_FILES, and a .env sorted after every one of them.
    for i in 0..1500 {
        write(wt, &format!("src/m{i:04}.rs"), "pub fn f() {}\n");
    }
    write(wt, "zz/.env", "TOKEN=hunter2\n");
    write(wt, "zz/prod.env", "TOKEN=hunter3\n");
    let plan = plan_commit(&tools(), &t, "feat: many", None).unwrap();
    assert_eq!(plan.refusal, None, "{:?}", plan.refusal);
    let kinds: Vec<(&str, &str, bool)> = plan
        .suspects
        .iter()
        .map(|s| (s.path.as_str(), s.kind, s.excluded))
        .collect();
    assert_eq!(
        kinds,
        [("zz/.env", "secret", true), ("zz/prod.env", "secret", true)]
    );
    assert_eq!(
        plan.files,
        Some(FileCount {
            total: 1500,
            listed: MAX_FILES
        })
    );
    assert_eq!(plan.details.len(), MAX_FILES);
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.starts_with(&format!("1500 files to commit, {MAX_FILES} of them listed"))),
        "{:?}",
        plan.warnings
    );
    assert_eq!(
        plan.summary,
        "Stage 1500 files (leaving 2 out) and commit them on feat"
    );
    let done = commit(&tools(), &t, "feat: many", None, &plan.fingerprint).unwrap();
    assert!(done.ok, "{done:?}");
    let committed = run_git(wt, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(committed.lines().count(), 1500);
    assert!(!committed.contains(".env"), "the secret was committed");
    assert!(
        run_git(wt, &["status", "--porcelain", "--untracked-files=all"]).contains("?? zz/.env")
    );
}

#[test]
fn a_listed_rename_past_a_cut_set_is_not_called_missing() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    commit_file(wt, "a.rs", "pub fn a() {}\n", "add a");
    run_git(wt, &["mv", "a.rs", "b.rs"]);
    // Untracked past the cut, every one of them left out.
    for i in 0..(MAX_FILES + 200) {
        write(wt, &format!("node_modules/p{i:04}.js"), "x\n");
    }
    let plan = plan_commit(&tools(), &t, "refactor: rename", None).unwrap();
    assert_eq!(plan.refusal, None, "{:?}", plan.refusal);
    assert_eq!(plan.excluded, ["node_modules/"]);
    assert_eq!(
        plan.files,
        Some(FileCount {
            total: 2,
            listed: 2
        })
    );
    assert!(
        !plan.warnings.iter().any(|w| w.contains("listed (cut")),
        "{:?}",
        plan.warnings
    );
}

#[test]
fn too_many_changes_names_the_untracked_directories() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    write(wt, "node_modules/@scope/pkg/index.js", "x\n");
    write(wt, "dist/app.js", "x\n");
    write(wt, "app.js", "x\n");
    let tools = tools();
    let why = too_many_changes(&Git::new(&tools, wt));
    assert!(why.starts_with("too many changes to plan against (git status is over 64 MiB): untracked directories dist/, node_modules/;"), "{why}");
    assert!(why.contains(".gitignore"), "{why}");
}

#[test]
fn merge_refuses_suspect_files_unless_landed_on_purpose() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    commit_file(wt, "src/a.rs", "fn a() {}\n", "feat: a");
    commit_file(wt, ".env", "API_KEY=x\n", "oops: env");
    // Forced: a global excludes file may ignore it.
    write(wt, ".claude/settings.local.json", "{}\n");
    run_git(wt, &["add", "-f", "--", ".claude/settings.local.json"]);
    run_git(wt, &["commit", "-q", "-m", "oops: perms"]);
    let plan = plan_merge(&tools(), &t, false).unwrap();
    let why = plan.refusal.clone().unwrap();
    assert!(
        why.contains(".env") && why.contains(".claude/settings.local.json"),
        "{why}"
    );
    assert!(!why.contains("src/a.rs"), "{why}");
    let refused = merge(&tools(), &t, false, &plan.fingerprint);
    assert!(refused.is_err());
    let plan = plan_merge(&tools(), &t, true).unwrap();
    assert_eq!(plan.refusal, None, "{plan:?}");
    assert!(plan.warnings.iter().any(|w| w.contains("landed anyway")));
    // A fingerprint from the refusing plan does not run the other.
    let done = merge(&tools(), &t, true, &plan.fingerprint).unwrap();
    assert!(done.ok);
}

#[test]
fn a_failed_commit_puts_the_index_back_and_no_identity_is_refused_first() {
    let f = Fixture::new();
    let wt = &f.wt.path;
    let t = f.target();
    write(wt, "new.txt", "n\n");
    write(&f.repo, ".git/hooks/pre-commit", "#!/bin/sh\nexit 1\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            f.repo.join(".git/hooks/pre-commit"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let plan = plan_commit(&tools(), &t, "feat: new", None).unwrap();
    assert_eq!(plan.refusal, None, "{plan:?}");
    let err = commit(&tools(), &t, "feat: new", None, &plan.fingerprint).unwrap_err();
    assert!(err.to_string().contains("put back"), "{err}");
    assert_eq!(run_git(wt, &["status", "--porcelain"]), "?? new.txt");
    // No identity: refused before anything is staged.
    std::fs::remove_file(f.repo.join(".git/hooks/pre-commit")).unwrap();
    run_git(wt, &["config", "user.name", ""]);
    let plan = plan_commit(&tools(), &t, "feat: new", None).unwrap();
    assert!(plan.refusal.unwrap().contains("no identity"));
}
