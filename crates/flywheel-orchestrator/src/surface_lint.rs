//! Cross-task surface-overlap lint (authoring-time gate).
//!
//! Two active tasks that write the *same* file surface with no declared
//! coordination are a data-loss trap: they can be dispatched concurrently,
//! both edit the shared file, and the loser's rebase collides -- the infrared
//! incident of 2026-07-09 (`wsteth-goldens` and `balancer-bpt-base-golden`
//! carried disjoint conflict-key sets, both wrote the shared golden-harness
//! surface, ran concurrently, and the loser's rebase collision triggered the
//! P5 data loss).
//!
//! This module is the static, pre-dispatch detector for that shape. It is
//! pairwise over the active listing only: it never executes a grader, never
//! reads the store, and never checks whether a derived path exists on disk
//! (an output path a task will create is still a surface -- the same
//! rationale that tabled `flywheel_core::validation`'s missing-path check).
//!
//! A task's **surface** is the union of the file paths it declares it
//! touches:
//!
//! * `context.relevant` entries, each with a trailing ` -- comment`
//!   annotation stripped (no other code parses that convention -- it is
//!   implemented here);
//! * path tokens pulled from every `command` grader's `run` string, with
//!   `pytest` `::node-ids` reduced to the file path.
//!
//! Every candidate passes the one true normalization autopilot uses
//! ([`conflict_key_for_path`]): empty, absolute, parent-escaping,
//! directory-like, and manifest/lockfile paths drop out, and any candidate
//! containing `://` (a URL) contributes nothing.
//!
//! A pair is **flagged** when their surfaces intersect AND their
//! `conflict_keys` sets are disjoint (a shared key already serializes them at
//! claim time) AND at least one overlapping path is listed in neither task's
//! top-level `overlap_ok` allow-list AND neither task is reachable from the
//! other over the active listing's `prerequisites` edges (transitively -- a
//! chained pair already runs in sequence). `overlap_ok` is a
//! validate-time-only orchestration key; this lint is its only consumer.

use crate::sources::{read_overlap_ok, read_prerequisites, read_scheduling_metadata};
use flywheel_core::task::{Grader, Task};
use flywheel_core::validation::TaskDefect;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Build-manifest / lockfile basenames that make poor conflict resources:
/// many legitimately-distinct tasks land against the same manifest, so
/// treating it as a shared surface would flag unrelated work. The one true
/// list, shared with autopilot's claim-key derivation.
pub const MANIFEST_BASENAMES: [&str; 15] = [
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "go.mod",
    "go.sum",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
];

/// The suffix of a path's final component the way `PurePosixPath.suffix`
/// reports it: the text from the last `.` on, unless that dot is the first
/// or last character of the name (so `.bashrc` and `name.` have none).
fn posix_suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[i..],
        _ => "",
    }
}

/// Normalize a repo-relative path into a stable conflict-key token, or
/// `None` when it is too coarse to be a useful contended resource.
///
/// Returns `None` for a path that is empty, absolute, parent-escaping,
/// directory-like (no file suffix -- e.g. `crates/`), or a shared build
/// manifest / lockfile (see [`MANIFEST_BASENAMES`]). A specific source file
/// (`crates/infrared-feed/src/tycho.rs`) returns its normalized posix form
/// (`.` components and empty segments collapsed, trailing slash dropped) so
/// two tasks scoped to it are seen as the same surface. This is the single
/// normalization autopilot's claim keys and this lint's surfaces both pass
/// through.
pub fn conflict_key_for_path(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = s
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.contains(&"..") {
        return None;
    }
    let name = parts.last()?;
    if posix_suffix(name).is_empty() {
        return None;
    }
    if MANIFEST_BASENAMES.contains(name) {
        return None;
    }
    Some(parts.join("/"))
}

/// One active task's derived file surface plus its coordination metadata.
///
/// `paths` is the normalized surface (see the module docs). `conflict_keys`
/// (a shared key exempts the pair -- claim-time serialization already covers
/// it), `overlap_ok` (per-path allow-list, exempts only the listed path), and
/// `prerequisites` (a chain exempts the pair) are the three coordination
/// signals the pairwise check consults.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskSurface {
    pub task_id: String,
    pub paths: BTreeSet<String>,
    pub conflict_keys: BTreeSet<String>,
    pub overlap_ok: BTreeSet<String>,
    pub prerequisites: Vec<String>,
}

impl TaskSurface {
    /// A surface with only the two load-bearing fields set; the three
    /// coordination signals default to empty.
    pub fn new<I, S>(task_id: impl Into<String>, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            task_id: task_id.into(),
            paths: paths.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }
}

/// Return a `context.relevant` entry with any ` -- comment` suffix cut.
///
/// The ` -- ` convention (path, then a free-text note) is documented but
/// parsed nowhere else, so the strip is implemented here. Only the first
/// occurrence splits, so a note that itself contains ` -- ` is discarded
/// whole.
fn strip_relevant_annotation(entry: &str) -> &str {
    match entry.find(" -- ") {
        Some(idx) => entry[..idx].trim(),
        None => entry.trim(),
    }
}

/// Split `input` the way Python's `shlex.split` does in POSIX mode (no
/// comment handling, whitespace-only token boundaries).
///
/// Single quotes protect everything; inside double quotes a backslash
/// escapes only `"` and `\` (any other backslash is kept literally); outside
/// quotes a backslash escapes the next character. `None` for an unclosed
/// quote or a trailing backslash -- the shapes `shlex` raises `ValueError`
/// on.
fn shlex_split(input: &str) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Space,
        Word,
        Single,
        Double,
        Escape,
    }
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut state = State::Space;
    let mut escaped_from = State::Word;
    let mut chars = input.chars().map(Some).chain(std::iter::once(None));
    loop {
        let next = chars.next()?;
        match state {
            State::Space => match next {
                None => break,
                Some(ch) if is_shell_whitespace(ch) => {
                    if !token.is_empty() || quoted {
                        tokens.push(std::mem::take(&mut token));
                        quoted = false;
                    }
                }
                Some('\\') => {
                    escaped_from = State::Word;
                    state = State::Escape;
                }
                Some('\'') => {
                    quoted = true;
                    state = State::Single;
                }
                Some('"') => {
                    quoted = true;
                    state = State::Double;
                }
                Some(ch) => {
                    token.push(ch);
                    state = State::Word;
                }
            },
            State::Single | State::Double => match next {
                None => return None,
                Some('\'') if state == State::Single => state = State::Word,
                Some('"') if state == State::Double => state = State::Word,
                Some('\\') if state == State::Double => {
                    escaped_from = State::Double;
                    state = State::Escape;
                }
                Some(ch) => token.push(ch),
            },
            State::Escape => match next {
                None => return None,
                Some(ch) => {
                    if escaped_from == State::Double && ch != '\\' && ch != '"' {
                        token.push('\\');
                    }
                    token.push(ch);
                    state = escaped_from;
                }
            },
            State::Word => match next {
                None => {
                    tokens.push(std::mem::take(&mut token));
                    break;
                }
                Some(ch) if is_shell_whitespace(ch) => {
                    state = State::Space;
                    if !token.is_empty() || quoted {
                        tokens.push(std::mem::take(&mut token));
                        quoted = false;
                    }
                }
                Some('\'') => {
                    quoted = true;
                    state = State::Single;
                }
                Some('"') => {
                    quoted = true;
                    state = State::Double;
                }
                Some('\\') => {
                    escaped_from = State::Word;
                    state = State::Escape;
                }
                Some(ch) => token.push(ch),
            },
        }
    }
    Some(tokens)
}

fn is_shell_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\r' | '\n')
}

/// Path-candidate tokens from a command grader's `run` string.
///
/// Tokenized as a shell command (falling back to whitespace split when the
/// string does not parse -- an unparseable `run` is a separate defect the
/// grader check already surfaces). Each token has any `pytest` `::node-id`
/// suffix reduced to the leading file path; the URL and normalization
/// filters are applied downstream by [`task_surface`].
fn run_path_tokens(run: &str) -> Vec<String> {
    let tokens =
        shlex_split(run).unwrap_or_else(|| run.split_whitespace().map(str::to_owned).collect());
    tokens
        .into_iter()
        .map(|token| match token.find("::") {
            Some(idx) => token[..idx].to_owned(),
            None => token,
        })
        .collect()
}

/// Add `candidate` to `surface` when it normalizes to a real path.
///
/// URLs (anything containing `://`) contribute nothing; everything else is
/// run through [`conflict_key_for_path`], which drops the coarse and
/// unusable forms.
fn add_candidate(surface: &mut BTreeSet<String>, candidate: &str) {
    if candidate.contains("://") {
        return;
    }
    if let Some(key) = conflict_key_for_path(candidate) {
        surface.insert(key);
    }
}

/// Derive `task`'s normalized file surface (empty when it declares none).
///
/// The union of `context.relevant` entries (annotation-stripped) and every
/// `command` grader `run`'s path tokens, each passed through the URL filter
/// and [`conflict_key_for_path`]. Never touches the filesystem -- an output
/// path a task will create is still a surface.
pub fn task_surface(task: &Task) -> BTreeSet<String> {
    let mut surface = BTreeSet::new();
    for entry in &task.context.relevant {
        add_candidate(&mut surface, strip_relevant_annotation(entry));
    }
    for grader in &task.graders {
        if let Grader::Command(command) = grader {
            for token in run_path_tokens(&command.run) {
                add_candidate(&mut surface, &token);
            }
        }
    }
    surface
}

/// Assemble a [`TaskSurface`] from a loaded task and its source file.
///
/// The surface comes from the compiled [`Task`]; the three coordination
/// signals (`conflict_keys`, `overlap_ok`, `prerequisites`) are top-level
/// orchestration keys core flywheel ignores, so they are read straight from
/// the task file with the tolerant `sources` readers (an absent or malformed
/// key yields its default).
pub fn build_surface(path: impl AsRef<Path>, task: &Task) -> TaskSurface {
    let path = path.as_ref();
    TaskSurface {
        task_id: task.id.clone(),
        paths: task_surface(task),
        conflict_keys: read_scheduling_metadata(path).conflict_keys,
        overlap_ok: read_overlap_ok(path).into_iter().collect(),
        prerequisites: read_prerequisites(path),
    }
}

/// Transitive prerequisite reachability, over active-listing edges only.
///
/// For each task, the set of task ids reachable by following
/// `prerequisites` edges. An edge whose target is not in the active listing
/// is dropped, so a path through an archived (absent) task does not connect
/// the two ends -- the exemption is intentionally not inherited across a
/// gap.
fn reachability(surfaces: &[TaskSurface]) -> BTreeMap<&str, BTreeSet<&str>> {
    let present: BTreeSet<&str> = surfaces.iter().map(|s| s.task_id.as_str()).collect();
    let adjacency: BTreeMap<&str, Vec<&str>> = surfaces
        .iter()
        .map(|s| {
            let edges = s
                .prerequisites
                .iter()
                .map(String::as_str)
                .filter(|p| present.contains(p))
                .collect();
            (s.task_id.as_str(), edges)
        })
        .collect();
    let mut closure = BTreeMap::new();
    for (start, edges) in &adjacency {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<&str> = edges.clone();
        while let Some(node) = stack.pop() {
            if !seen.insert(node) {
                continue;
            }
            if let Some(next) = adjacency.get(node) {
                stack.extend(next.iter().copied());
            }
        }
        closure.insert(*start, seen);
    }
    closure
}

/// Python's `repr()` of a string: single-quoted unless the text contains a
/// single quote and no double quote, with backslashes, the chosen quote,
/// and control characters escaped.
fn py_repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// The defect detail: both task ids, every overlapping path, remediations.
fn overlap_detail(task_a: &str, task_b: &str, paths: &[&str]) -> String {
    format!(
        "surface overlap between tasks {} and {} on: {}. Resolve by declaring a shared \
         conflict_keys entry, listing each path in one task's overlap_ok, or chaining the \
         tasks via prerequisites.",
        py_repr_str(task_a),
        py_repr_str(task_b),
        paths.join(", ")
    )
}

/// Pairwise surface-overlap defects across the active listing.
///
/// Returns one [`TaskDefect`] per flagged pair (attached to the first task
/// of the pair in listing order). A pair is flagged when their surfaces
/// intersect, their `conflict_keys` are disjoint, at least one overlapping
/// path is in neither task's `overlap_ok`, and neither is reachable from
/// the other over prerequisites. Empty when nothing overlaps.
pub fn surface_overlap_defects(surfaces: &[TaskSurface]) -> Vec<TaskDefect> {
    let reachable = reachability(surfaces);
    let mut defects = Vec::new();
    for (i, a) in surfaces.iter().enumerate() {
        for b in &surfaces[i + 1..] {
            let overlap: Vec<&str> = a.paths.intersection(&b.paths).map(String::as_str).collect();
            if overlap.is_empty() {
                continue;
            }
            if a.conflict_keys
                .intersection(&b.conflict_keys)
                .next()
                .is_some()
            {
                // A shared key already serializes them at claim time.
                continue;
            }
            // BTreeSet intersection is already sorted, matching Python's
            // `sorted(...)` over the flagged subset.
            let flagged: Vec<&str> = overlap
                .into_iter()
                .filter(|p| !a.overlap_ok.contains(*p) && !b.overlap_ok.contains(*p))
                .collect();
            if flagged.is_empty() {
                // Every overlapping path is individually allow-listed.
                continue;
            }
            let a_reaches_b = reachable
                .get(a.task_id.as_str())
                .is_some_and(|set| set.contains(b.task_id.as_str()));
            let b_reaches_a = reachable
                .get(b.task_id.as_str())
                .is_some_and(|set| set.contains(a.task_id.as_str()));
            if a_reaches_b || b_reaches_a {
                // A prerequisite chain already runs them in sequence.
                continue;
            }
            defects.push(TaskDefect {
                task_id: a.task_id.clone(),
                detail: overlap_detail(&a.task_id, &b.task_id, &flagged),
            });
        }
    }
    defects
}

#[cfg(test)]
mod tests {
    use super::*;
    use flywheel_core::loaders::load_task_file;
    use flywheel_core::task::{CommandGrader, Context};
    use serde_json::json;
    use tempfile::TempDir;

    fn task(task_id: &str, run: &str, relevant: &[&str]) -> Task {
        let mut t = Task::new(
            format!("Goal for {task_id}."),
            vec![Grader::Command(CommandGrader::new(run).unwrap())],
        );
        t.id = task_id.to_owned();
        t.context = Context {
            relevant: relevant.iter().map(|s| (*s).to_owned()).collect(),
            ..Context::default()
        };
        t
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn surface(
        task_id: &str,
        paths: &[&str],
        conflict_keys: &[&str],
        overlap_ok: &[&str],
        prerequisites: &[&str],
    ) -> TaskSurface {
        TaskSurface {
            task_id: task_id.to_owned(),
            paths: set(paths),
            conflict_keys: set(conflict_keys),
            overlap_ok: set(overlap_ok),
            prerequisites: prerequisites.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    // --- shlex port ---------------------------------------------------------

    #[test]
    fn shlex_split_matches_python_posix_semantics() {
        assert_eq!(
            shlex_split("uv run pytest  tests/a.py").unwrap(),
            vec!["uv", "run", "pytest", "tests/a.py"]
        );
        assert_eq!(
            shlex_split(r#"echo "a b" 'c d' e\ f"#).unwrap(),
            vec!["echo", "a b", "c d", "e f"]
        );
        // Inside double quotes a backslash escapes only `"` and `\`.
        assert_eq!(
            shlex_split(r#"x "a\"b" "c\nd" "e\\f""#).unwrap(),
            vec!["x", "a\"b", "c\\nd", "e\\f"]
        );
        // An empty quoted token survives; adjacent quoted pieces join.
        assert_eq!(shlex_split(r#"a "" 'b'"c""#).unwrap(), vec!["a", "", "bc"]);
        assert_eq!(shlex_split("  \t\n ").unwrap(), Vec::<String>::new());
        assert!(shlex_split("unterminated \"quote").is_none());
        assert!(shlex_split("trailing \\").is_none());
    }

    // --- normalization ------------------------------------------------------

    #[test]
    fn conflict_key_normalizes_and_rejects_coarse_shapes() {
        assert_eq!(
            conflict_key_for_path(" ./crates//x/./y.rs "),
            Some("crates/x/y.rs".to_owned())
        );
        assert_eq!(conflict_key_for_path(""), None);
        assert_eq!(conflict_key_for_path("/abs/x.rs"), None);
        assert_eq!(conflict_key_for_path("a/../b.rs"), None);
        assert_eq!(conflict_key_for_path("docs/"), None);
        assert_eq!(conflict_key_for_path("packages/foo"), None);
        assert_eq!(conflict_key_for_path(".bashrc"), None);
        assert_eq!(conflict_key_for_path("name."), None);
        assert_eq!(conflict_key_for_path("pyproject.toml"), None);
        assert_eq!(conflict_key_for_path("sub/Cargo.lock"), None);
    }

    // --- surface derivation -------------------------------------------------

    #[test]
    fn surface_includes_command_grader_run_paths() {
        let t = task("t", "uv run pytest tests/shared/golden_test.py", &[]);
        assert_eq!(task_surface(&t), set(&["tests/shared/golden_test.py"]));
    }

    #[test]
    fn surface_includes_relevant_with_annotation_stripped() {
        let t = task(
            "t",
            "true",
            &["tests/shared/golden_test.py -- writes the shared golden"],
        );
        assert_eq!(task_surface(&t), set(&["tests/shared/golden_test.py"]));
    }

    #[test]
    fn surface_strips_pytest_node_ids_to_the_file_path() {
        let t = task("t", "pytest tests/a.py::TestX::test_y", &[]);
        assert_eq!(task_surface(&t), set(&["tests/a.py"]));
    }

    #[test]
    fn surface_drops_urls_directories_and_manifests() {
        let t = task(
            "t",
            "uv run pytest tests/real.py",
            &[
                "https://example.com/docs",
                "docs/",
                "packages/foo",
                "pyproject.toml",
            ],
        );
        assert_eq!(task_surface(&t), set(&["tests/real.py"]));
    }

    #[test]
    fn surface_of_bare_command_is_empty() {
        assert!(task_surface(&task("t", "true", &[])).is_empty());
    }

    #[test]
    fn unparseable_run_falls_back_to_whitespace_split() {
        let t = task("t", "pytest tests/a.py \"unterminated", &[]);
        assert_eq!(task_surface(&t), set(&["tests/a.py"]));
    }

    // --- pairwise flagging --------------------------------------------------

    #[test]
    fn disjoint_nonempty_conflict_keys_still_flag_with_identical_detail() {
        let a = surface(
            "wsteth",
            &["tests/golden.py"],
            &["internal-engine"],
            &[],
            &[],
        );
        let b = surface(
            "balancer",
            &["tests/golden.py"],
            &["harness-base"],
            &[],
            &[],
        );
        let defects = surface_overlap_defects(&[a, b]);
        assert_eq!(defects.len(), 1);
        assert_eq!(defects[0].task_id, "wsteth");
        assert_eq!(
            defects[0].detail,
            "surface overlap between tasks 'wsteth' and 'balancer' on: tests/golden.py. \
             Resolve by declaring a shared conflict_keys entry, listing each path in one \
             task's overlap_ok, or chaining the tasks via prerequisites."
        );
    }

    #[test]
    fn both_empty_conflict_keys_flag() {
        let a = surface("a", &["tests/golden.py"], &[], &[], &[]);
        let b = surface("b", &["tests/golden.py"], &[], &[], &[]);
        assert_eq!(surface_overlap_defects(&[a, b]).len(), 1);
    }

    #[test]
    fn shared_conflict_key_exempts_the_pair() {
        let a = surface("a", &["tests/golden.py"], &["harness", "x"], &[], &[]);
        let b = surface("b", &["tests/golden.py"], &["harness", "y"], &[], &[]);
        assert!(surface_overlap_defects(&[a, b]).is_empty());
    }

    #[test]
    fn no_overlap_no_defect() {
        let a = surface("a", &["tests/a.py"], &[], &[], &[]);
        let b = surface("b", &["tests/b.py"], &[], &[], &[]);
        assert!(surface_overlap_defects(&[a, b]).is_empty());
    }

    #[test]
    fn overlap_ok_exempts_only_the_listed_path() {
        let a = surface(
            "a",
            &["tests/one.py", "tests/two.py"],
            &[],
            &["tests/one.py"],
            &[],
        );
        let b = surface("b", &["tests/one.py", "tests/two.py"], &[], &[], &[]);
        let defects = surface_overlap_defects(&[a, b]);
        assert_eq!(defects.len(), 1);
        assert!(defects[0].detail.contains("tests/two.py"));
        assert!(!defects[0].detail.contains("tests/one.py"));
    }

    #[test]
    fn overlap_ok_on_either_task_exempts_the_whole_overlap() {
        let a = surface("a", &["tests/one.py"], &[], &[], &[]);
        let b = surface("b", &["tests/one.py"], &[], &["tests/one.py"], &[]);
        assert!(surface_overlap_defects(&[a, b]).is_empty());
    }

    #[test]
    fn direct_prerequisite_exempts_the_pair() {
        let a = surface("a", &["tests/g.py"], &[], &[], &["b"]);
        let b = surface("b", &["tests/g.py"], &[], &[], &[]);
        assert!(surface_overlap_defects(&[a, b]).is_empty());
    }

    #[test]
    fn transitive_prerequisite_exempts_the_pair() {
        let a = surface("a", &["tests/g.py"], &[], &[], &["mid"]);
        let mid = surface("mid", &[], &[], &[], &["b"]);
        let b = surface("b", &["tests/g.py"], &[], &[], &[]);
        assert!(surface_overlap_defects(&[a, mid, b]).is_empty());
    }

    #[test]
    fn prerequisite_through_absent_task_does_not_exempt() {
        let a = surface("a", &["tests/g.py"], &[], &[], &["gone"]);
        let b = surface("b", &["tests/g.py"], &[], &[], &[]);
        assert_eq!(surface_overlap_defects(&[a, b]).len(), 1);
    }

    #[test]
    fn flagged_paths_are_sorted_and_defect_attaches_to_first_task() {
        let a = surface("a", &["z/b.py", "a/a.py"], &[], &[], &[]);
        let b = surface("b", &["z/b.py", "a/a.py"], &[], &[], &[]);
        let c = surface("c", &["a/a.py"], &[], &[], &[]);
        let defects = surface_overlap_defects(&[a, b, c]);
        assert_eq!(defects.len(), 3);
        assert!(defects[0].detail.contains("on: a/a.py, z/b.py."));
        assert_eq!(
            defects
                .iter()
                .map(|d| d.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "a", "b"]
        );
    }

    // --- build_surface reads the top-level orchestration keys ----------------

    #[test]
    fn build_surface_reads_conflict_keys_overlap_ok_prerequisites() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ov.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "id": "ov",
                "goal": "G.",
                "graders": [{"type": "command", "run": "pytest tests/golden.py"}],
                "context": {"relevant": ["tests/other.py -- note"]},
                "conflict_keys": ["k1"],
                "overlap_ok": ["tests/golden.py"],
                "prerequisites": ["dep"],
            }))
            .unwrap(),
        )
        .unwrap();
        let task = load_task_file(&path).unwrap();
        let surface = build_surface(&path, &task);
        assert_eq!(surface.task_id, "ov");
        assert_eq!(surface.paths, set(&["tests/golden.py", "tests/other.py"]));
        assert_eq!(surface.conflict_keys, set(&["k1"]));
        assert_eq!(surface.overlap_ok, set(&["tests/golden.py"]));
        assert_eq!(surface.prerequisites, vec!["dep".to_owned()]);
    }

    #[test]
    fn build_surface_tolerates_absent_and_malformed_keys() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("b.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "id": "b", "goal": "G.", "graders": [], "overlap_ok": "nope",
                "conflict_keys": 7, "prerequisites": {"x": 1},
            }))
            .unwrap(),
        )
        .unwrap();
        let task = load_task_file(&path).unwrap();
        let surface = build_surface(&path, &task);
        assert!(surface.overlap_ok.is_empty());
        assert!(surface.conflict_keys.is_empty());
        assert!(surface.prerequisites.is_empty());
        assert!(surface.paths.is_empty());
    }

    #[test]
    fn py_repr_matches_python_quoting_rules() {
        assert_eq!(py_repr_str("plain"), "'plain'");
        assert_eq!(py_repr_str("it's"), "\"it's\"");
        assert_eq!(py_repr_str("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr_str("x\\y\n"), "'x\\\\y\\n'");
    }
}
