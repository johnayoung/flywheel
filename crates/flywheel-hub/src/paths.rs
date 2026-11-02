//! Where the hub keeps its state: `$XDG_STATE_HOME/flywheel/hub`, falling
//! back to `~/.local/state/flywheel/hub`.

use std::fs;
use std::path::{Path, PathBuf};

/// Resolve the default state dir from the given environment lookups.
///
/// An `XDG_STATE_HOME` that is unset, empty, or relative is ignored (the XDG
/// spec requires absolute paths). Returns `None` when neither it nor `HOME`
/// is usable.
pub fn default_state_dir_from(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_state_home.filter(|v| Path::new(v).is_absolute()) {
        return Some(Path::new(xdg).join("flywheel").join("hub"));
    }
    let home = home.filter(|v| !v.is_empty())?;
    Some(
        Path::new(home)
            .join(".local")
            .join("state")
            .join("flywheel")
            .join("hub"),
    )
}

/// [`default_state_dir_from`] against the process environment.
pub fn default_state_dir() -> Option<PathBuf> {
    let xdg = std::env::var("XDG_STATE_HOME").ok();
    let home = std::env::var("HOME").ok();
    default_state_dir_from(xdg.as_deref(), home.as_deref())
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
    fn prefers_absolute_xdg_state_home() {
        assert_eq!(
            default_state_dir_from(Some("/x/state"), Some("/home/u")),
            Some(PathBuf::from("/x/state/flywheel/hub"))
        );
    }

    #[test]
    fn falls_back_to_home_for_unset_or_relative_xdg() {
        let expected = Some(PathBuf::from("/home/u/.local/state/flywheel/hub"));
        assert_eq!(default_state_dir_from(None, Some("/home/u")), expected);
        assert_eq!(default_state_dir_from(Some(""), Some("/home/u")), expected);
        assert_eq!(
            default_state_dir_from(Some("rel"), Some("/home/u")),
            expected
        );
        assert_eq!(default_state_dir_from(None, None), None);
        assert_eq!(default_state_dir_from(None, Some("")), None);
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
