//! Shared installation-probe helpers: executable lookup on `PATH`, the
//! `--version` read with a timeout, and an explicit environment snapshot so
//! probes are testable without touching the real process environment.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// The slice of the environment a probe inspects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeEnvironment {
    pub path: Option<String>,
    pub home: Option<PathBuf>,
    pub vars: BTreeMap<String, String>,
}

impl ProbeEnvironment {
    /// Snapshot the current process environment.
    pub fn from_process() -> Self {
        Self {
            path: std::env::var("PATH").ok(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            vars: std::env::vars().collect(),
        }
    }

    /// A variable's value when set to something non-empty.
    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// First `PATH` entry holding an executable named `name`.
pub fn find_executable(name: &str, path: Option<&str>) -> Option<PathBuf> {
    let path = path?;
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// `<executable> --version`, trimmed stdout; `None` on spawn failure,
/// timeout, non-zero exit, or empty output.
pub async fn read_version(executable: &Path) -> Option<String> {
    let child = Command::new(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let output = tokio::time::timeout(VERSION_TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_executable_scans_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("fakebin");
        std::fs::write(&exe, "#!/bin/sh\necho hi\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!("/nonexistent-dir:{}", dir.path().display());
        assert_eq!(find_executable("fakebin", Some(&path)), Some(exe));
        assert_eq!(find_executable("missing", Some(&path)), None);
        assert_eq!(find_executable("fakebin", None), None);
    }

    #[tokio::test]
    async fn read_version_returns_none_for_missing_binary() {
        assert_eq!(
            read_version(Path::new("/nonexistent/definitely-not-here")).await,
            None
        );
    }

    #[test]
    fn var_filters_empty_values() {
        let mut env = ProbeEnvironment::default();
        env.vars.insert("A".into(), "".into());
        env.vars.insert("B".into(), "x".into());
        assert_eq!(env.var("A"), None);
        assert_eq!(env.var("B"), Some("x"));
    }
}
