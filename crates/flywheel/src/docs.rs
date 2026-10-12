//! The `flywheel docs` / `fw docs` verb: serve the curated operator docs.
//!
//! The curated topics are embedded at build time, byte-identical copies of
//! the canonical `docs/*.md` (`include_str!`), so a topic prints
//! identically from any working directory and never resolves a path outside
//! the binary.
//!
//! [`TOPICS`] is the single source of truth for the curated set. The bare
//! listing, the per-topic serving path, and the parity test all consume this
//! one constant -- there is no second copy of the topic names anywhere.
//! Adding or renaming a curated doc is a one-line edit here (plus the
//! matching `include_str!` arm in [`render_topic`]).
//!
//! Docs are knowledge, not policy: this verb loads no `flywheel.toml` and
//! touches no store, so it works in a bare directory with nothing
//! scaffolded.

use std::io::Write;

const PROG: &str = "fw";

/// The curated topic set, in listing order: `(topic, description)`.
///
/// Topic names are the canonical `docs/<topic>.md` basenames; descriptions
/// are the one-line summaries shown in the bare listing, condensed from
/// `docs/README.md`.
pub const TOPICS: [(&str, &str); 10] = [
    (
        "cli",
        "every verb, the interactive operator console, slash commands, and init.",
    ),
    ("configuration", "the complete flywheel.toml reference."),
    (
        "task-schema",
        "the Task / Grader / Context shape and validation rules.",
    ),
    (
        "loop",
        "the single-task control plane: the iteration envelope, state detection, harness behavior.",
    ),
    (
        "workflow",
        "how flywheel develops itself: the spec-driven authoring pipeline and the runtime loop.",
    ),
    (
        "strategy",
        "the SubmitStrategy / SandboxHandle landing seam and the shipped strategies.",
    ),
    (
        "sandbox",
        "the sandbox-as-deploy model and the complete [sandbox.*] reference.",
    ),
    (
        "held-out-gate",
        "the execute-time held-out landing gate that defends against reward-hacking.",
    ),
    (
        "autopilot",
        "the autopilot intake daemon: tier model, scoring, CLI, and console activation.",
    ),
    (
        "team-mode",
        "the multi-operator runbook: GitHub App identity, rulesets, and the merge queue as the distributed merge lock.",
    ),
];

/// The topic names alone, in listing order.
pub fn topic_names() -> impl Iterator<Item = &'static str> {
    TOPICS.iter().map(|(topic, _)| *topic)
}

/// Whether `topic` is one of the curated set.
pub fn is_topic(topic: &str) -> bool {
    topic_names().any(|name| name == topic)
}

/// The byte-for-byte content of one curated doc, or `None` for an unknown
/// topic. Every arm embeds the canonical `docs/<topic>.md` at build time.
pub fn render_topic(topic: &str) -> Option<&'static str> {
    let page = match topic {
        "cli" => include_str!("../../../docs/cli.md"),
        "configuration" => include_str!("../../../docs/configuration.md"),
        "task-schema" => include_str!("../../../docs/task-schema.md"),
        "loop" => include_str!("../../../docs/loop.md"),
        "workflow" => include_str!("../../../docs/workflow.md"),
        "strategy" => include_str!("../../../docs/strategy.md"),
        "sandbox" => include_str!("../../../docs/sandbox.md"),
        "held-out-gate" => include_str!("../../../docs/held-out-gate.md"),
        "autopilot" => include_str!("../../../docs/autopilot.md"),
        "team-mode" => include_str!("../../../docs/team-mode.md"),
        _ => return None,
    };
    Some(page)
}

/// The bare-listing text: one `<topic>  <description>` line each.
///
/// Both the names and the descriptions come from [`TOPICS`], so the listing
/// can never name a topic the serving path cannot serve.
pub fn render_listing() -> String {
    let width = TOPICS
        .iter()
        .map(|(topic, _)| topic.len())
        .max()
        .unwrap_or(0)
        + 2;
    let mut lines = vec![
        "Curated operator docs. Read one with: flywheel docs <topic>".to_owned(),
        String::new(),
    ];
    lines.extend(
        TOPICS
            .iter()
            .map(|(topic, description)| format!("{topic:<width$}{description}")),
    );
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Serve the curated operator docs, writing to the supplied streams.
///
/// * no args (or `-h` / `--help`): the listing on `stdout`, exit 0;
/// * `<topic>`: that doc verbatim on `stdout`, exit 0;
/// * an unknown topic: an error naming it on `stderr` (nothing on
///   `stdout`), exit 2.
pub fn run_with(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let Some(topic) = args.first() else {
        let _ = stdout.write_all(render_listing().as_bytes());
        return 0;
    };
    if topic == "-h" || topic == "--help" {
        let _ = stdout.write_all(render_listing().as_bytes());
        return 0;
    }
    match render_topic(topic) {
        Some(page) => {
            let _ = stdout.write_all(page.as_bytes());
            0
        }
        None => {
            let _ = writeln!(stderr, "{PROG}: unknown docs topic: {topic}");
            let _ = writeln!(stderr, "  see '{PROG} docs' for the topic list");
            2
        }
    }
}

/// [`run_with`] against the process's own stdout / stderr: the verb entry.
pub fn run(args: &[String]) -> i32 {
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let rc = run_with(args, &mut out, &mut err);
    let _ = out.flush();
    let _ = err.flush();
    rc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// The repo root: this crate lives at `<root>/crates/flywheel`.
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .expect("crate sits two levels under the repo root")
    }

    #[test]
    fn served_doc_matches_canonical_source_for_every_topic() {
        for topic in topic_names() {
            let canonical = repo_root().join("docs").join(format!("{topic}.md"));
            let source = std::fs::read(&canonical)
                .unwrap_or_else(|err| panic!("{}: {err}", canonical.display()));
            let served = render_topic(topic).unwrap_or_else(|| panic!("unserved topic {topic}"));
            assert_eq!(
                served.as_bytes(),
                source.as_slice(),
                "served {topic}.md drifted from canonical {}",
                canonical.display()
            );
        }
    }

    #[test]
    fn topic_set_and_served_pages_are_in_lockstep() {
        for topic in topic_names() {
            assert!(render_topic(topic).is_some(), "{topic} listed but unserved");
            assert!(is_topic(topic));
        }
        assert!(render_topic("no-such-topic").is_none());
        assert!(!is_topic("no-such-topic"));
        let mut names: Vec<_> = topic_names().collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TOPICS.len(), "duplicate topic names");
    }

    #[test]
    fn listing_formats_one_padded_line_per_topic() {
        let listing = render_listing();
        let lines: Vec<&str> = listing.lines().collect();
        assert_eq!(
            lines[0],
            "Curated operator docs. Read one with: flywheel docs <topic>"
        );
        assert_eq!(lines[1], "");
        assert_eq!(lines.len(), 2 + TOPICS.len());
        assert!(listing.ends_with('\n'));
        let width = TOPICS.iter().map(|(t, _)| t.len()).max().unwrap() + 2;
        for (line, (topic, description)) in lines[2..].iter().zip(TOPICS.iter()) {
            assert_eq!(*line, format!("{topic:<width$}{description}"));
        }
        assert!(listing.contains("cli            every verb"));
    }

    #[test]
    fn run_serves_listing_topic_and_unknown_error() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run_with(&[], &mut out, &mut err), 0);
        assert_eq!(String::from_utf8(out).unwrap(), render_listing());
        assert!(err.is_empty());

        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run_with(&["--help".to_owned()], &mut out, &mut err), 0);
        assert_eq!(String::from_utf8(out).unwrap(), render_listing());

        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run_with(&["cli".to_owned()], &mut out, &mut err), 0);
        assert_eq!(out.as_slice(), render_topic("cli").unwrap().as_bytes());
        assert!(err.is_empty());

        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run_with(&["bogus".to_owned()], &mut out, &mut err), 2);
        assert!(out.is_empty());
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "fw: unknown docs topic: bogus\n  see 'fw docs' for the topic list\n"
        );
    }
}
