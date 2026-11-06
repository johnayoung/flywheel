//! Where the hub keeps its state: `$FLYWHEEL_HOME/hub`, the home falling
//! back to `~/.flywheel` (`$FLYWHEEL_HOME/hub` = `~/.flywheel/hub`). The
//! pre-home dir (`$XDG_STATE_HOME/flywheel/hub`) is never read; a start that
//! finds it prints the `mv` that moves it ([`legacy_state_notice`]).
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

/// The state dir hubs used before the flywheel home:
/// `$XDG_STATE_HOME/flywheel/hub`, else `<HOME>/.local/state/flywheel/hub`.
/// An `XDG_STATE_HOME` that is unset, empty, or relative is ignored (the XDG
/// spec requires absolute paths), exactly as that resolution did.
pub fn legacy_state_dir_from(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_state_home.filter(|v| Path::new(v).is_absolute()) {
        return Some(Path::new(xdg).join("flywheel").join("hub"));
    }
    let home = home.filter(|v| !v.is_empty())?;
    Some(Path::new(home).join(".local/state/flywheel/hub"))
}

/// [`legacy_state_dir_from`] against the process environment.
pub fn legacy_state_dir() -> Option<PathBuf> {
    let xdg = std::env::var("XDG_STATE_HOME").ok();
    let home = std::env::var("HOME").ok();
    legacy_state_dir_from(xdg.as_deref(), home.as_deref())
}

/// The one-time notice for a pre-home hub state dir: `Some` when `legacy`
/// is a directory and `state_dir` does not exist yet (the first start on
/// the new layout; the hub then creates `state_dir`, so the notice never
/// repeats). The command is printed, never run: nothing moves on its own.
pub fn legacy_state_notice(legacy: &Path, state_dir: &Path) -> Option<String> {
    if !legacy.is_dir() || state_dir.exists() || legacy == state_dir {
        return None;
    }
    let (old, new) = (shell_quote(legacy), shell_quote(state_dir));
    Some(format!(
        "fw hub: notice: {old} (hub state from an older flywheel) is not read; this hub \
         starts fresh at {new}. To keep the old state instead: fw hub stop && mv {new} \
         {new}.fresh && mv {old} {new}"
    ))
}

/// `path` as one POSIX shell word: bare when it holds only safe bytes, else
/// single-quoted.
fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    let safe = |c: char| c.is_ascii_alphanumeric() || "/._-+,:@%=".contains(c);
    if !text.is_empty() && text.chars().all(safe) {
        return text.into_owned();
    }
    format!("'{}'", text.replace('\'', r"'\''"))
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
    fn legacy_dir_follows_the_old_xdg_rule() {
        assert_eq!(
            legacy_state_dir_from(Some("/x/state"), Some("/home/u")),
            Some(PathBuf::from("/x/state/flywheel/hub"))
        );
        let fallback = Some(PathBuf::from("/home/u/.local/state/flywheel/hub"));
        assert_eq!(legacy_state_dir_from(None, Some("/home/u")), fallback);
        assert_eq!(legacy_state_dir_from(Some(""), Some("/home/u")), fallback);
        assert_eq!(
            legacy_state_dir_from(Some("rel"), Some("/home/u")),
            fallback
        );
        assert_eq!(legacy_state_dir_from(None, None), None);
    }

    #[test]
    fn legacy_notice_names_the_move_only_before_the_new_dir_exists() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("xdg/flywheel/hub");
        let new = root.path().join("fw home/hub");
        // No legacy dir: nothing to say.
        assert_eq!(legacy_state_notice(&legacy, &new), None);

        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("token"), "t").unwrap();
        let notice = legacy_state_notice(&legacy, &new).unwrap();
        let quoted_new = format!("'{}'", new.display());
        assert!(
            notice.contains(&format!("mv {} {quoted_new}", legacy.display())),
            "{notice}"
        );
        assert!(notice.contains("is not read"), "{notice}");
        // Printing it moved nothing.
        assert!(legacy.join("token").is_file());
        assert!(!new.exists());

        // Once the new dir exists (the hub created it), it never repeats.
        fs::create_dir_all(&new).unwrap();
        assert_eq!(legacy_state_notice(&legacy, &new), None);
    }

    #[test]
    fn shell_quote_keeps_safe_paths_bare() {
        assert_eq!(shell_quote(Path::new("/a/b-c.d")), "/a/b-c.d");
        assert_eq!(shell_quote(Path::new("/a b")), "'/a b'");
        assert_eq!(shell_quote(Path::new("/it's")), r"'/it'\''s'");
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
