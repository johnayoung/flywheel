//! Port of the Python `flywheel._cli` module: the verb router.

/// Route one invocation; returns the exit code.
pub fn run(argv: Vec<String>) -> i32 {
    if argv.len() > 1 {
        eprintln!("flywheel: verb routing is not wired yet ({})", argv[1]);
        return 2;
    }
    eprintln!("usage: flywheel <verb> [args]");
    2
}
