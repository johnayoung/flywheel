//! `task` and `lifecycle` must stay pure: no filesystem, IO, or environment
//! access. Mirrors the Python `test_task_module_purity.py` /
//! `test_lifecycle_module_purity.py` invariants.

const TASK_SRC: &str = include_str!("../src/task.rs");
const LIFECYCLE_SRC: &str = include_str!("../src/lifecycle.rs");
const ENVELOPE_SRC: &str = include_str!("../src/envelope.rs");

const FORBIDDEN: &[&str] = &[
    "std::fs",
    "std::io",
    "std::env",
    "std::process",
    "std::net",
    "tokio::",
    "File::",
    "read_to_string",
];

fn assert_pure(name: &str, src: &str) {
    for needle in FORBIDDEN {
        assert!(
            !src.contains(needle),
            "{name} must stay pure: found {needle:?}"
        );
    }
}

#[test]
fn task_module_is_pure() {
    assert_pure("task.rs", TASK_SRC);
}

#[test]
fn lifecycle_module_is_pure() {
    assert_pure("lifecycle.rs", LIFECYCLE_SRC);
}

#[test]
fn envelope_module_is_pure() {
    assert_pure("envelope.rs", ENVELOPE_SRC);
}
