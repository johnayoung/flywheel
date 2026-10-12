//! `fw`: the short alias for the `flywheel` console script. One
//! implementation, two entry points.

fn main() {
    std::process::exit(flywheel::main_entry());
}
