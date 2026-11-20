//! Base mirrors: a repository holding a commit and its ancestors only, so a
//! trial cut from it cannot read the answer from history that came after
//! (the noise floor's eval clone carried `main`, with every landed answer).
//!
//! A mirror is a fresh `git init` that fetches exactly one ref from the
//! source repo (no tags), which brings the base and its ancestry and
//! nothing else; [`future_free`] proves it (V4). Trials clone their own
//! working repository from the mirror (`git clone --local`, objects
//! hardlinked), so trials of one task never share branch names.

use std::path::Path;

use crate::git::{self, GitError};

/// The branch the mirror's base sits on.
pub const MIRROR_BRANCH: &str = "eval/base";

/// Build (or reuse) a mirror of `base` from `repo` at `dest`. `pin` is a
/// ref in `repo` pointing at `base` (a frozen set's pinned ref); without
/// one, a temporary ref is made for the fetch and removed after.
pub fn base_mirror(
    repo: &Path,
    base: &str,
    pin: Option<&str>,
    dest: &Path,
) -> Result<(), GitError> {
    if dest.join(".git").exists()
        && git::rev_parse(dest, MIRROR_BRANCH).ok().as_deref() == Some(base)
    {
        return Ok(());
    }
    if dest.exists() {
        let _ = std::fs::remove_dir_all(dest);
    }
    std::fs::create_dir_all(dest).map_err(|e| GitError {
        args: "mkdir".into(),
        dir: dest.display().to_string(),
        detail: e.to_string(),
    })?;
    // Initial branch other than the one fetched into: git refuses to fetch
    // into the checked-out branch.
    git::run(dest, &["init", "--quiet", "-b", "eval/init"])?;
    let temp = format!("refs/flywheel/eval-fetch/{}", &base[..base.len().min(12)]);
    let source_ref = match pin {
        Some(pin) => pin.to_owned(),
        None => {
            git::update_ref(repo, &temp, base)?;
            temp.clone()
        }
    };
    let fetched = git::run(
        dest,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            &repo.display().to_string(),
            &format!("{source_ref}:refs/heads/{MIRROR_BRANCH}"),
        ],
    );
    if pin.is_none() {
        let _ = git::run(repo, &["update-ref", "-d", &temp]);
    }
    fetched?;
    git::run(dest, &["checkout", "--quiet", "--force", MIRROR_BRANCH])?;
    Ok(())
}

/// Whether every commit the repository at `dir` can reach is `base` or an
/// ancestor of it (V4: no future history).
pub fn future_free(dir: &Path, base: &str) -> Result<bool, GitError> {
    let beyond = git::run(dir, &["rev-list", "--all", "--not", base])?;
    Ok(beyond.trim().is_empty())
}

/// A trial's own working repository, cloned from `mirror`, with no remote
/// (nothing it does can reach the mirror or the source repo).
pub fn trial_clone(mirror: &Path, dest: &Path) -> Result<(), GitError> {
    if dest.exists() {
        let _ = std::fs::remove_dir_all(dest);
    }
    let parent = dest.parent().unwrap_or(Path::new("/"));
    std::fs::create_dir_all(parent).map_err(|e| GitError {
        args: "mkdir".into(),
        dir: parent.display().to_string(),
        detail: e.to_string(),
    })?;
    git::run(
        parent,
        &[
            "clone",
            "--quiet",
            "--local",
            "--no-tags",
            "--branch",
            MIRROR_BRANCH,
            &mirror.display().to_string(),
            &dest.display().to_string(),
        ],
    )?;
    git::run(dest, &["remote", "remove", "origin"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;

    #[test]
    fn a_mirror_holds_the_base_and_its_past_only() {
        let repo = TempRepo::new();
        let old = repo.commit("old", &[("f", "1")]);
        let base = repo.commit("base", &[("f", "2")]);
        let future = repo.commit("future answer", &[("f", "3")]);
        repo.git(&["tag", "v-future", &future]);
        let dir = tempfile::tempdir().unwrap();
        let mirror = dir.path().join("mirror");
        base_mirror(repo.path(), &base, None, &mirror).unwrap();
        assert!(future_free(&mirror, &base).unwrap());
        assert!(git::ok(&mirror, &["cat-file", "-e", &old]));
        assert!(
            !git::ok(&mirror, &["cat-file", "-e", &future]),
            "no future objects"
        );
        assert!(
            git::run(repo.path(), &["for-each-ref", "refs/flywheel"])
                .unwrap()
                .is_empty()
        );
        // Reuse is a no-op; a clone has no remote and starts at the base.
        base_mirror(repo.path(), &base, None, &mirror).unwrap();
        let trial = dir.path().join("trial");
        trial_clone(&mirror, &trial).unwrap();
        assert_eq!(git::rev_parse(&trial, "HEAD").unwrap(), base);
        assert!(git::run(&trial, &["remote"]).unwrap().is_empty());
        assert!(future_free(&trial, &base).unwrap());
        // A repository that can see past the base is caught.
        assert!(!future_free(repo.path(), &base).unwrap());
    }
}
