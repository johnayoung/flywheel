//! Session kinds -- a plain shell, or one of the agent CLIs the hub
//! launches (Claude Code, Codex) -- and resolving their programs on `PATH`.

use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// What a session runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// The hub's shell, or an explicit argv.
    #[default]
    Shell,
    /// The `claude` CLI (Claude Code).
    Claude,
    /// The `codex` CLI.
    Codex,
}

impl SessionKind {
    pub const ALL: [Self; 3] = [Self::Shell, Self::Claude, Self::Codex];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn is_agent(self) -> bool {
        !matches!(self, Self::Shell)
    }
}

/// The agent CLIs the hub launches, by program name or path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPrograms {
    pub claude: String,
    pub codex: String,
}

impl Default for AgentPrograms {
    fn default() -> Self {
        Self {
            claude: "claude".into(),
            codex: "codex".into(),
        }
    }
}

impl AgentPrograms {
    /// The configured program for an agent kind (`None` for shells).
    pub fn program(&self, kind: SessionKind) -> Option<&str> {
        match kind {
            SessionKind::Shell => None,
            SessionKind::Claude => Some(&self.claude),
            SessionKind::Codex => Some(&self.codex),
        }
    }
}

/// Resolve `program` the way a shell would: a name containing `/` is used
/// as is (when it is an executable file), a bare name is searched for in
/// `path` (a `PATH`-style list).
pub fn resolve_program(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let candidate = PathBuf::from(program);
        return is_executable(&candidate).then_some(candidate);
    }
    std::env::split_paths(path?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

/// Resolve against this process's `PATH`.
pub fn which(program: &str) -> Option<PathBuf> {
    resolve_program(program, std::env::var_os("PATH").as_deref())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(dir: &Path, name: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn resolves_bare_names_on_path_in_order_and_skips_non_executables() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        script(&first, "claude", 0o644);
        let found = script(&second, "claude", 0o755);
        script(&first, "codex", 0o755);
        std::fs::create_dir(second.join("codex")).unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();

        assert_eq!(resolve_program("claude", Some(&path)), Some(found));
        assert_eq!(
            resolve_program("codex", Some(&path)),
            Some(first.join("codex"))
        );
        assert_eq!(resolve_program("missing", Some(&path)), None);
        assert_eq!(resolve_program("claude", None), None);
        assert_eq!(resolve_program("", Some(&path)), None);
    }

    #[test]
    fn paths_with_a_slash_are_used_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let exe = script(dir.path(), "agent", 0o755);
        let plain = script(dir.path(), "notes", 0o600);
        let exe_str = exe.to_str().unwrap();
        assert_eq!(resolve_program(exe_str, None), Some(exe.clone()));
        assert_eq!(resolve_program(plain.to_str().unwrap(), None), None);
        // A slash means "do not search", even when PATH has a match.
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(resolve_program("./agent-missing", Some(&path)), None);
    }

    #[test]
    fn kinds_serialize_as_lowercase_names() {
        for kind in SessionKind::ALL {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
        }
        assert_eq!(SessionKind::default(), SessionKind::Shell);
        assert!(SessionKind::Claude.is_agent() && !SessionKind::Shell.is_agent());
    }
}
