//! Where the hub keeps its state: `$FLYWHEEL_HOME/hub`, the home falling
//! back to `~/.flywheel` (`$FLYWHEEL_HOME/hub` = `~/.flywheel/hub`).
//!
//! The hub depends on no other flywheel crate, so it mirrors the home rule
//! of `flywheel_orchestrator::home::home_from` here; a test in the `flywheel`
//! crate pins the two together.

use std::fs;
use std::path::{Path, PathBuf};

/// The environment variable naming the flywheel home.
pub const HOME_ENV: &str = "FLYWHEEL_HOME";

/// Resolve the default state dir from the given environment lookups:
/// `<FLYWHEEL_HOME>/hub` when it is non-empty (a relative value is anchored
/// at `cwd`), else `<HOME>/.flywheel/hub`. Returns `None` when neither is
/// usable.
pub fn default_state_dir_from(
    flywheel_home: Option<&str>,
    home: Option<&str>,
    cwd: &Path,
) -> Option<PathBuf> {
    let root = if let Some(value) = flywheel_home.filter(|v| !v.is_empty()) {
        let path = Path::new(value);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    } else {
        Path::new(home.filter(|v| !v.is_empty())?).join(".flywheel")
    };
    Some(root.join("hub"))
}

/// [`default_state_dir_from`] against the process environment.
pub fn default_state_dir() -> Option<PathBuf> {
    let flywheel_home = std::env::var(HOME_ENV).ok();
    let home = std::env::var("HOME").ok();
    let cwd = std::env::current_dir().unwrap_or_default();
    default_state_dir_from(flywheel_home.as_deref(), home.as_deref(), &cwd)
}

/// Create `dir` (and parents) and restrict it to the owner (0700).
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_flywheel_home() {
        let cwd = Path::new("/work");
        assert_eq!(
            default_state_dir_from(Some("/x/fw"), Some("/home/u"), cwd),
            Some(PathBuf::from("/x/fw/hub"))
        );
        assert_eq!(
            default_state_dir_from(Some("rel"), Some("/home/u"), cwd),
            Some(PathBuf::from("/work/rel/hub"))
        );
    }

    #[test]
    fn falls_back_to_home_dot_flywheel() {
        let cwd = Path::new("/work");
        let expected = Some(PathBuf::from("/home/u/.flywheel/hub"));
        assert_eq!(default_state_dir_from(None, Some("/home/u"), cwd), expected);
        assert_eq!(
            default_state_dir_from(Some(""), Some("/home/u"), cwd),
            expected
        );
        assert_eq!(default_state_dir_from(None, None, cwd), None);
        assert_eq!(default_state_dir_from(None, Some(""), cwd), None);
    }

    #[test]
    fn private_dir_is_0700() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("a/b");
        ensure_private_dir(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }
}
