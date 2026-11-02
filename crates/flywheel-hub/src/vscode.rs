//! VS Code multi-root workspace files (`*.code-workspace`): parsing them
//! into hub workspace folders, and finding them on disk for the UI's
//! import picker.
//!
//! The format is JSONC (comments, trailing commas). Each entry of
//! `folders` has a `path` (absolute, or relative to the file's directory)
//! or a `uri`, plus an optional display `name`. Entries the hub cannot use
//! (remote URIs, Windows paths) are reported as skipped, never dropped
//! silently.

use serde::Serialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// The file extension VS Code gives workspace files.
pub const EXTENSION: &str = ".code-workspace";
/// Largest workspace file read (they are a few KiB in practice).
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// How deep [`discover`] looks below its root by default.
pub const DEFAULT_SEARCH_DEPTH: usize = 3;
/// Directories [`discover`] visits at most (bounds a huge tree).
pub const MAX_SEARCH_DIRS: usize = 20_000;
/// Files [`discover`] returns at most.
pub const MAX_SEARCH_FILES: usize = 500;

/// Directory names [`discover`] never descends into: build output and
/// dependency trees, which are large and hold no workspace files of yours.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "venv",
    "__pycache__",
    "dist",
    "build",
];

/// A parsed workspace file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodeWorkspace {
    /// The file name without `.code-workspace`.
    pub name: String,
    /// Usable folder entries, in file order (paths resolved, not yet
    /// checked against the disk).
    pub folders: Vec<CodeFolder>,
    /// Entries that name no local directory, with why.
    pub skipped: Vec<Skipped>,
}

/// One folder entry, resolved to an absolute path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodeFolder {
    /// Its place in the file's `folders` (0-based).
    #[serde(skip)]
    pub index: usize,
    /// The entry as written (`path` or `uri`), for reports.
    pub entry: String,
    /// Absolute, lexically normalized (`.`/`..` folded) path.
    pub path: PathBuf,
    /// The entry's `name` label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// An entry that was not imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    /// Its place in the file's `folders` (0-based), for reporting in file
    /// order.
    #[serde(skip)]
    pub index: usize,
    /// The entry as written (`path` or `uri`; `#N` for an entry with
    /// neither).
    pub entry: String,
    /// The resolved path, when there is one (a folder that does not exist).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub reason: String,
}

/// What resolving paths needs from the environment.
#[derive(Debug, Clone, Default)]
pub struct ResolveEnv {
    /// `$HOME`, for `~/...` paths.
    pub home: Option<PathBuf>,
    /// The WSL distro this runs in (`$WSL_DISTRO_NAME`): a Windows VS Code
    /// file's `vscode-remote://wsl+<distro>/...` folders are local here.
    pub wsl_distro: Option<String>,
}

impl ResolveEnv {
    /// This process's environment.
    pub fn current() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Self {
            home: var("HOME").map(PathBuf::from),
            wsl_distro: var("WSL_DISTRO_NAME"),
        }
    }
}

/// The workspace name a file implies: its name without `.code-workspace`
/// (or its stem, for a file named otherwise).
pub fn default_name(file: &Path) -> Option<String> {
    let name = file.file_name()?.to_str()?;
    let stem = match name.strip_suffix(EXTENSION) {
        Some(stem) => stem,
        None => file.file_stem()?.to_str()?,
    };
    (!stem.trim().is_empty()).then(|| stem.to_owned())
}

/// Read and parse the workspace file at `file` (absolute).
pub fn read(file: &Path, env: &ResolveEnv) -> Result<CodeWorkspace, String> {
    let shown = file.display();
    let opened = fs::File::open(file).map_err(|e| format!("{shown}: {e}"))?;
    let meta = opened.metadata().map_err(|e| format!("{shown}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{shown} is not a file"));
    }
    let mut bytes = Vec::new();
    opened
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{shown}: {e}"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "{shown} is larger than {} KiB; not a workspace file",
            MAX_FILE_BYTES / 1024
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{shown} is not UTF-8 text"))?;
    parse(&text, file, env).map_err(|e| format!("{shown}: {e}"))
}

/// Parse workspace file text; `file` (absolute) anchors relative paths and
/// names the workspace.
pub fn parse(text: &str, file: &Path, env: &ResolveEnv) -> Result<CodeWorkspace, String> {
    let name = default_name(file).ok_or_else(|| "cannot name a workspace from it".to_owned())?;
    let base = file
        .parent()
        .filter(|p| p.is_absolute())
        .ok_or_else(|| "its path must be absolute".to_owned())?;
    let json = strip_jsonc(text.strip_prefix('\u{feff}').unwrap_or(text));
    let doc: Value = serde_json::from_str(&json).map_err(|e| format!("invalid JSON: {e}"))?;
    let entries = doc
        .get("folders")
        .and_then(Value::as_array)
        .ok_or_else(|| "no \"folders\" list (not a VS Code workspace file)".to_owned())?;
    let mut folders = Vec::new();
    let mut skipped = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let label = entry
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned);
        let resolved = if let Some(path) = entry.get("path").and_then(Value::as_str) {
            (path.to_owned(), resolve_path(path, base, env))
        } else if let Some(uri) = entry.get("uri").and_then(Value::as_str) {
            (uri.to_owned(), resolve_uri(uri, env))
        } else {
            (
                format!("#{}", i + 1),
                Err("entry has neither a \"path\" nor a \"uri\"".to_owned()),
            )
        };
        match resolved {
            (entry, Ok(path)) => folders.push(CodeFolder {
                index: i,
                entry,
                path,
                name: label,
            }),
            (entry, Err(reason)) => skipped.push(Skipped {
                index: i,
                entry,
                path: None,
                reason,
            }),
        }
    }
    Ok(CodeWorkspace {
        name,
        folders,
        skipped,
    })
}

/// A Windows drive (`C:\...`, `C:/...`) or UNC (`\\host\...`) path.
fn is_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/'))
        || path.starts_with("\\\\")
}

fn resolve_path(raw: &str, base: &Path, env: &ResolveEnv) -> Result<PathBuf, String> {
    if raw.trim().is_empty() {
        return Err("empty path".into());
    }
    if is_windows_path(raw) {
        return Err("a Windows path; not reachable from this machine".into());
    }
    let expanded = crate::state::expand_home(Path::new(raw), env.home.as_deref());
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    Ok(normalize(&absolute))
}

fn resolve_uri(uri: &str, env: &ResolveEnv) -> Result<PathBuf, String> {
    let Some((scheme, rest)) = uri.split_once("://") else {
        return Err("not a URI".into());
    };
    let (authority, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, "/"),
    };
    let authority = percent_decode(authority).unwrap_or_else(|| authority.to_owned());
    let local = match scheme.to_ascii_lowercase().as_str() {
        "file" => authority.is_empty() || authority.eq_ignore_ascii_case("localhost"),
        "vscode-remote" => match (authority.strip_prefix("wsl+"), env.wsl_distro.as_deref()) {
            (Some(distro), Some(ours)) => distro.eq_ignore_ascii_case(ours),
            _ => false,
        },
        _ => false,
    };
    if !local {
        return Err(format!(
            "a {scheme} folder on {}; only local folders can be imported",
            if authority.is_empty() {
                "another machine"
            } else {
                &authority
            }
        ));
    }
    let decoded = percent_decode(path).ok_or_else(|| "badly escaped URI".to_owned())?;
    if is_windows_path(decoded.trim_start_matches('/')) {
        return Err("a Windows path; not reachable from this machine".into());
    }
    Ok(normalize(Path::new(&decoded)))
}

/// `%XX` escapes decoded (UTF-8); `None` when malformed.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = raw.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Fold `.` and `..` components lexically (an absolute path's `..` at the
/// root stays at the root).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// JSONC to JSON: `//` and `/* */` comments outside strings removed
/// (newlines kept, so error positions still point at the right line) and
/// trailing commas before `}` / `]` dropped.
pub fn strip_jsonc(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut code = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            code.push(c);
            if c == '\\' && i + 1 < chars.len() {
                code.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match (c, chars.get(i + 1)) {
            ('"', _) => {
                in_string = true;
                code.push(c);
                i += 1;
            }
            ('/', Some('/')) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    if chars[i] == '\n' {
                        code.push('\n');
                    }
                    i += 1;
                }
                i += 2;
                code.push(' ');
            }
            _ => {
                code.push(c);
                i += 1;
            }
        }
    }
    // Second pass: a comma whose next significant character closes the
    // object or array is dropped.
    let chars: Vec<char> = code.chars().collect();
    let mut out = String::with_capacity(code.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|n| !n.is_whitespace());
            if !matches!(next, Some('}' | ']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// Workspace files found under a root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Sorted.
    pub files: Vec<PathBuf>,
    /// The search stopped at [`MAX_SEARCH_DIRS`] or [`MAX_SEARCH_FILES`].
    pub truncated: bool,
}

/// Find `*.code-workspace` files in `root` and directories up to
/// `max_depth` levels below it, breadth first. Hidden directories, build
/// and dependency trees ([`SKIP_DIRS`]), and symlinked directories are not
/// entered; symlinked files are listed. Unreadable directories are passed
/// over.
pub fn discover(root: &Path, max_depth: usize) -> Discovery {
    let mut found = Discovery::default();
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut visited = 0;
    while let Some((dir, depth)) = queue.pop_front() {
        visited += 1;
        if visited > MAX_SEARCH_DIRS {
            found.truncated = true;
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let path = entry.path();
            if name.ends_with(EXTENSION) && name.len() > EXTENSION.len() {
                let is_file = kind.is_file() || (kind.is_symlink() && path.is_file());
                if is_file {
                    if found.files.len() >= MAX_SEARCH_FILES {
                        found.truncated = true;
                        break;
                    }
                    found.files.push(path);
                }
            } else if kind.is_dir()
                && depth < max_depth
                && !name.starts_with('.')
                && !SKIP_DIRS.contains(&name)
            {
                queue.push_back((path, depth + 1));
            }
        }
        if found.files.len() >= MAX_SEARCH_FILES && found.truncated {
            break;
        }
    }
    found.files.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> ResolveEnv {
        ResolveEnv {
            home: Some(PathBuf::from("/home/me")),
            wsl_distro: Some("Ubuntu".into()),
        }
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_are_removed_outside_strings() {
        let text = r#"{
            // a line comment
            "folders": [ /* inline */ {"path": "a//b", "name": "x /* y */",},
                {"path": "c\"//d",}, ],
            /* multi
               line */
            "settings": {"k": [1, 2,],},
        }"#;
        let value: Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(value["folders"][0]["path"], "a//b");
        assert_eq!(value["folders"][0]["name"], "x /* y */");
        assert_eq!(value["folders"][1]["path"], "c\"//d");
        assert_eq!(value["settings"]["k"], serde_json::json!([1, 2]));
        // Comments keep line numbers for errors.
        assert_eq!(strip_jsonc("/*a\nb*/x").lines().count(), 2);
    }

    #[test]
    fn folders_resolve_relative_to_the_file_with_labels_and_home() {
        let text = r#"{
            "folders": [
                {"name": "infra", "path": "../infrared"},
                {"path": "../../pickem"},
                {"path": "./same"},
                {"path": "~/notes", "name": "  "},
                {"path": "/abs/dir/"},
            ],
            "settings": {}
        }"#;
        let parsed = parse(
            text,
            Path::new("/home/me/code/workspaces/personal.code-workspace"),
            &env(),
        )
        .unwrap();
        assert_eq!(parsed.name, "personal");
        let paths: Vec<_> = parsed.folders.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            paths,
            [
                "/home/me/code/infrared",
                "/home/me/pickem",
                "/home/me/code/workspaces/same",
                "/home/me/notes",
                "/abs/dir",
            ]
            .map(PathBuf::from)
        );
        assert_eq!(parsed.folders[0].name.as_deref(), Some("infra"));
        assert_eq!(parsed.folders[0].entry, "../infrared");
        assert_eq!(parsed.folders[3].name, None, "blank labels are dropped");
        assert!(parsed.skipped.is_empty());
    }

    #[test]
    fn unusable_entries_are_reported_as_skipped() {
        let text = r#"{"folders": [
            {"uri": "file:///home/me/My%20Repo"},
            {"uri": "vscode-remote://ssh-remote%2Bbox/srv/app"},
            {"uri": "vscode-remote://wsl%2Bubuntu/home/me/wsl-repo"},
            {"uri": "vscode-remote://wsl+Debian/home/me/other"},
            {"path": "C:\\Users\\me\\proj"},
            {"name": "nothing"},
            {"path": ""}
        ]}"#;
        let parsed = parse(text, Path::new("/w/x.code-workspace"), &env()).unwrap();
        let paths: Vec<_> = parsed.folders.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("/home/me/My Repo"),
                PathBuf::from("/home/me/wsl-repo")
            ]
        );
        let reasons: Vec<_> = parsed
            .skipped
            .iter()
            .map(|s| (s.entry.as_str(), s.reason.as_str()))
            .collect();
        assert_eq!(reasons.len(), 5, "{reasons:?}");
        assert!(reasons[0].1.contains("ssh-remote+box"), "{reasons:?}");
        assert!(reasons[1].1.contains("wsl+Debian"), "{reasons:?}");
        assert!(reasons[2].1.contains("Windows path"), "{reasons:?}");
        assert_eq!(reasons[3].0, "#6");
        assert_eq!(reasons[4].1, "empty path");
    }

    #[test]
    fn rejects_files_that_are_not_workspaces() {
        let file = Path::new("/w/x.code-workspace");
        assert!(
            parse("{]", file, &env())
                .unwrap_err()
                .contains("invalid JSON")
        );
        assert!(
            parse(r#"{"settings": {}}"#, file, &env())
                .unwrap_err()
                .contains("no \"folders\"")
        );
        assert!(parse("{\"folders\": []}", Path::new("rel.code-workspace"), &env()).is_err());
        // A byte-order mark is fine.
        assert!(parse("\u{feff}{\"folders\": []}", file, &env()).is_ok());
    }

    #[test]
    fn names_come_from_the_file() {
        assert_eq!(
            default_name(Path::new("/a/my.team.code-workspace")).as_deref(),
            Some("my.team")
        );
        assert_eq!(
            default_name(Path::new("/a/proj.json")).as_deref(),
            Some("proj")
        );
        assert_eq!(default_name(Path::new("/a/.code-workspace")), None);
    }

    #[test]
    fn read_bounds_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.code-workspace");
        fs::write(&big, vec![b' '; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert!(read(&big, &env()).unwrap_err().contains("larger than"));
        assert!(read(dir.path(), &env()).is_err());
        let ok = dir.path().join("ok.code-workspace");
        fs::write(&ok, r#"{"folders":[{"path":"."}]}"#).unwrap();
        let parsed = read(&ok, &env()).unwrap();
        assert_eq!(parsed.folders[0].path, dir.path());
    }

    #[test]
    fn discovery_honours_depth_and_skips_hidden_and_build_trees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let touch = |rel: &str| {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "{}").unwrap();
        };
        touch("top.code-workspace");
        touch("workspaces/a.code-workspace");
        touch("x/y/z/deep.code-workspace");
        touch("x/y/z/w/too-deep.code-workspace");
        touch(".hidden/h.code-workspace");
        touch("proj/node_modules/n.code-workspace");
        touch("proj/notes.txt");
        fs::create_dir_all(root.join("workspaces/dir.code-workspace")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("x"), root.join("loop")).unwrap();
        let found = discover(root, DEFAULT_SEARCH_DEPTH);
        let rel: Vec<_> = found
            .files
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            rel,
            [
                "top.code-workspace",
                "workspaces/a.code-workspace",
                "x/y/z/deep.code-workspace"
            ]
        );
        assert!(!found.truncated);
        assert!(discover(&root.join("missing"), 3).files.is_empty());
    }
}
