//! A build jobserver the worker shares with its containers.
//!
//! A worker started inside a budgeted hub session (`fw hub`'s build
//! budget) carries `MAKEFLAGS` / `CARGO_MAKEFLAGS` naming a FIFO jobserver
//! (`--jobserver-auth=fifo:PATH`). The container backend bind-mounts that
//! FIFO at the same path and passes the variables into every `docker
//! exec`, so builds in the container draw from the same pool as the host's.
//! The contract is the environment variable alone: nothing here knows the
//! hub. A worker without one runs exactly as before.

use std::path::{Path, PathBuf};

/// The variables that carry a jobserver, in the order they are passed.
pub const JOBSERVER_VARS: [&str; 2] = ["CARGO_MAKEFLAGS", "MAKEFLAGS"];

/// A FIFO jobserver found in an environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jobserver {
    /// The FIFO, mounted into the container at this same path.
    pub fifo: PathBuf,
    /// The jobserver variables as set (passed into each exec unchanged).
    pub env: Vec<(String, String)>,
}

/// The FIFO a make-flags value names (`--jobserver-auth=fifo:PATH`).
pub fn fifo_of(flags: &str) -> Option<PathBuf> {
    flags.split_whitespace().find_map(|word| {
        word.strip_prefix("--jobserver-auth=fifo:")
            .filter(|path| Path::new(path).is_absolute())
            .map(PathBuf::from)
    })
}

impl Jobserver {
    /// The jobserver `var` reads (a variable's value, `None` when unset),
    /// if one of [`JOBSERVER_VARS`] names a FIFO. Cargo reads
    /// `CARGO_MAKEFLAGS` first, so its FIFO wins.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let env: Vec<(String, String)> = JOBSERVER_VARS
            .iter()
            .filter_map(|key| var(key).map(|value| ((*key).to_owned(), value)))
            .collect();
        let fifo = env.iter().find_map(|(_, value)| fifo_of(value))?;
        Some(Self { fifo, env })
    }

    /// The jobserver this process's environment carries.
    pub fn from_env() -> Option<Self> {
        Self::from_vars(|key| std::env::var(key).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(list: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = list
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn a_fifo_jobserver_is_found_in_either_variable() {
        let flags = "-j8 --jobserver-auth=fifo:/s/build-jobserver.fifo";
        let both = Jobserver::from_vars(vars(&[("MAKEFLAGS", flags), ("CARGO_MAKEFLAGS", flags)]))
            .unwrap();
        assert_eq!(both.fifo, PathBuf::from("/s/build-jobserver.fifo"));
        assert_eq!(
            both.env,
            vec![
                ("CARGO_MAKEFLAGS".to_owned(), flags.to_owned()),
                ("MAKEFLAGS".to_owned(), flags.to_owned())
            ]
        );
        let make_only = Jobserver::from_vars(vars(&[("MAKEFLAGS", flags)])).unwrap();
        assert_eq!(make_only.env.len(), 1);
    }

    #[test]
    fn no_fifo_no_jobserver() {
        assert_eq!(Jobserver::from_vars(vars(&[])), None);
        // A pipe jobserver's descriptors mean nothing in a container.
        assert_eq!(
            Jobserver::from_vars(vars(&[("MAKEFLAGS", "-j4 --jobserver-auth=3,4")])),
            None
        );
        assert_eq!(Jobserver::from_vars(vars(&[("MAKEFLAGS", "-j4")])), None);
        assert_eq!(fifo_of("--jobserver-auth=fifo:relative/p"), None);
    }
}
