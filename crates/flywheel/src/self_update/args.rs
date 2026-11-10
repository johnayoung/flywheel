//! `fw self-update` argv parsing.

use std::path::PathBuf;

/// `--drain-timeout`'s default, in seconds.
pub const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 120;

/// The parsed flags of one `fw self-update` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// `--from PATH`: the flywheel checkout to build.
    pub from: Option<PathBuf>,
    /// `--install-dir DIR`: where `fw` / `flywheel` are replaced.
    pub install_dir: Option<PathBuf>,
    /// `--allow-unlanded`: skip the HEAD-on-the-default-branch check.
    pub allow_unlanded: bool,
    /// `--force`: skip the already-current check.
    pub force: bool,
    /// `--drain-timeout SECS`: the per-process wait after SIGTERM.
    pub drain_timeout_secs: u64,
    /// `--upgrade-postgres`: allow upgrading a behind Postgres store.
    pub upgrade_postgres: bool,
    /// `--dry-run`: build, then plan every later step.
    pub dry_run: bool,
    /// `--json`: JSON Lines on stdout.
    pub json: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            from: None,
            install_dir: None,
            allow_unlanded: false,
            force: false,
            drain_timeout_secs: DEFAULT_DRAIN_TIMEOUT_SECS,
            upgrade_postgres: false,
            dry_run: false,
            json: false,
        }
    }
}

/// What the argv asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Help,
    Run(Args),
}

/// Parse `args` (the argv after `self-update`). Errors are usage errors.
pub fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut parsed = Args::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "-h" || arg == "--help" {
            return Ok(Parsed::Help);
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        let switch = match flag {
            "--allow-unlanded" => Some(&mut parsed.allow_unlanded),
            "--force" => Some(&mut parsed.force),
            "--upgrade-postgres" => Some(&mut parsed.upgrade_postgres),
            "--dry-run" => Some(&mut parsed.dry_run),
            "--json" => Some(&mut parsed.json),
            _ => None,
        };
        if let Some(slot) = switch {
            if inline.is_some() {
                return Err(format!("{flag} takes no value"));
            }
            *slot = true;
            continue;
        }
        if !matches!(flag, "--from" | "--install-dir" | "--drain-timeout") {
            return Err(format!("unrecognized argument: {arg}"));
        }
        let value = match inline {
            Some(value) => value,
            None => iter
                .next()
                .cloned()
                .ok_or_else(|| format!("{flag} expects a value"))?,
        };
        if value.is_empty() {
            return Err(format!("{flag} expects a non-empty value"));
        }
        match flag {
            "--from" => parsed.from = Some(PathBuf::from(value)),
            "--install-dir" => parsed.install_dir = Some(PathBuf::from(value)),
            _ => {
                parsed.drain_timeout_secs = value.parse::<u64>().map_err(|_| {
                    format!("--drain-timeout expects a whole number of seconds, got {value:?}")
                })?;
            }
        }
    }
    Ok(Parsed::Run(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn run(parts: &[&str]) -> Args {
        match parse(&argv(parts)).unwrap() {
            Parsed::Run(args) => args,
            Parsed::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn defaults_when_no_flags() {
        assert_eq!(run(&[]), Args::default());
        assert_eq!(Args::default().drain_timeout_secs, 120);
    }

    #[test]
    fn every_flag_parses_spaced_and_inline() {
        let args = run(&[
            "--from",
            "/src",
            "--install-dir=/bin",
            "--allow-unlanded",
            "--force",
            "--drain-timeout",
            "7",
            "--upgrade-postgres",
            "--dry-run",
            "--json",
        ]);
        assert_eq!(args.from, Some(PathBuf::from("/src")));
        assert_eq!(args.install_dir, Some(PathBuf::from("/bin")));
        assert!(args.allow_unlanded && args.force && args.upgrade_postgres);
        assert!(args.dry_run && args.json);
        assert_eq!(args.drain_timeout_secs, 7);
        assert_eq!(run(&["--drain-timeout=0"]).drain_timeout_secs, 0);
    }

    #[test]
    fn help_wins_anywhere() {
        assert_eq!(parse(&argv(&["--json", "--help"])).unwrap(), Parsed::Help);
        assert_eq!(parse(&argv(&["-h"])).unwrap(), Parsed::Help);
    }

    #[test]
    fn usage_errors() {
        for bad in [
            &["--no-such-flag"][..],
            &["positional"],
            &["--from"],
            &["--from="],
            &["--drain-timeout", "soon"],
            &["--drain-timeout", "-1"],
            &["--force=yes"],
            &["--rollback"],
        ] {
            assert!(parse(&argv(bad)).is_err(), "{bad:?} parsed");
        }
    }
}
