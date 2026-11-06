//! `fw spec`: the cwd project's authored specs.
//!
//! Specs live centrally under `$FLYWHEEL_HOME/projects/<id>/specs/` (print
//! it with `fw project path --specs`); nothing is read from or written into
//! the repo tree unless the operator asks:
//!
//! * `fw spec ls [--json]` -- every spec file, relative to the specs dir.
//! * `fw spec export DIR [--force]` -- copy the specs tree (including
//!   `archive/`) into DIR byte-for-byte, creating DIR. A differing existing
//!   file is refused (nothing is written) unless `--force`; identical files
//!   are left as they are.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::project::cwd_bound_project;

/// The verb name.
pub const SPEC_VERB: &str = "spec";

/// `fw spec --help`.
pub const SPEC_USAGE: &str = "usage: fw spec <ls [--json] | export DIR [--force]>

Specs live under $FLYWHEEL_HOME/projects/<id>/specs/ (see
`fw project path --specs`), never in the repo tree.

  ls [--json]         list the current repo's spec files, relative to the
                      specs dir (JSON: [{\"path\", \"archived\"}])
  export DIR          copy the specs tree (including archive/) into DIR
                      byte-for-byte, creating DIR; prints each file written
  export DIR --force  also overwrite files in DIR that differ (without it,
                      any differing file aborts the export before a write)
";

/// Failures of `fw spec`.
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error(transparent)]
    Config(#[from] flywheel_orchestrator::config::ConfigError),
    #[error("{}: {message}", path.display())]
    Io { path: PathBuf, message: String },
    #[error("{0}")]
    Invalid(String),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> SpecError + '_ {
    move |err| SpecError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

/// Every regular file under `root`, as sorted paths relative to it
/// (empty when `root` does not exist). Symlinks are not followed.
pub fn spec_files(root: &Path) -> Result<Vec<PathBuf>, SpecError> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = root.join(&rel);
        for entry in fs::read_dir(&dir).map_err(io_err(&dir))? {
            let entry = entry.map_err(io_err(&dir))?;
            let kind = entry.file_type().map_err(io_err(&entry.path()))?;
            let child = rel.join(entry.file_name());
            if kind.is_dir() {
                stack.push(child);
            } else if kind.is_file() {
                out.push(child);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// What one [`export_specs`] wrote.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExportReport {
    /// Files created or overwritten, as paths under the destination.
    pub written: Vec<PathBuf>,
    /// Files already byte-identical at the destination.
    pub unchanged: Vec<PathBuf>,
}

/// Copy every file under `src` into `dest` (same relative paths, same
/// bytes), creating `dest`.
///
/// Every target is checked before anything is written: a differing file
/// (or a directory where a file belongs) aborts the export unless `force`,
/// and a directory is never overwritten. `dest` inside `src` is refused.
pub fn export_specs(src: &Path, dest: &Path, force: bool) -> Result<ExportReport, SpecError> {
    if let (Ok(src_real), Some(dest_real)) = (src.canonicalize(), canonical_prefix(dest))
        && dest_real.starts_with(&src_real)
    {
        return Err(SpecError::Invalid(format!(
            "{} is inside the specs dir {}; export to a directory outside it",
            dest.display(),
            src.display()
        )));
    }
    let files = spec_files(src)?;
    let mut plan = Vec::new();
    let mut conflicts = Vec::new();
    let mut report = ExportReport::default();
    for rel in files {
        let from = src.join(&rel);
        let to = dest.join(&rel);
        let bytes = fs::read(&from).map_err(io_err(&from))?;
        match fs::symlink_metadata(&to) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => plan.push((to, bytes)),
            Err(err) => return Err(io_err(&to)(err)),
            Ok(meta) if meta.is_dir() => {
                return Err(SpecError::Invalid(format!(
                    "{} is a directory; cannot export {} over it",
                    to.display(),
                    rel.display()
                )));
            }
            Ok(_) => {
                let existing = fs::read(&to).map_err(io_err(&to))?;
                if existing == bytes {
                    report.unchanged.push(to);
                } else if force {
                    plan.push((to, bytes));
                } else {
                    conflicts.push(to);
                }
            }
        }
    }
    if !conflicts.is_empty() {
        let list: Vec<String> = conflicts
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect();
        return Err(SpecError::Invalid(format!(
            "refusing to overwrite {} differing file(s) (pass --force to overwrite; nothing \
             was written):\n{}",
            conflicts.len(),
            list.join("\n")
        )));
    }
    fs::create_dir_all(dest).map_err(io_err(dest))?;
    for (to, bytes) in plan {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(io_err(parent))?;
        }
        fs::write(&to, &bytes).map_err(io_err(&to))?;
        report.written.push(to);
    }
    Ok(report)
}

/// `path` canonicalized through its deepest existing ancestor (the
/// not-yet-created tail re-appended), so containment checks work before
/// the destination exists.
fn canonical_prefix(path: &Path) -> Option<PathBuf> {
    let mut tail = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(real) = cursor.canonicalize() {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return Some(out);
        }
        tail.push(cursor.file_name()?.to_owned());
        cursor = cursor.parent()?;
        if cursor.as_os_str().is_empty() {
            cursor = Path::new(".");
        }
    }
}

/// One `fw spec ls --json` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SpecRow {
    path: PathBuf,
    archived: bool,
}

/// Run `fw spec ...` (`args` excludes the verb); returns the exit code.
pub fn run_with(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let Some(sub) = args.first() else {
        let _ = err.write_all(SPEC_USAGE.as_bytes());
        return 2;
    };
    let rest = &args[1..];
    if sub == "-h" || sub == "--help" || rest.iter().any(|a| a == "-h" || a == "--help") {
        let _ = out.write_all(SPEC_USAGE.as_bytes());
        return 0;
    }
    let result = match sub.as_str() {
        "ls" => match rest {
            [] => ls(false, out),
            [flag] if flag == "--json" => ls(true, out),
            _ => return usage_error(err, &format!("unexpected arguments: {}", rest.join(" "))),
        },
        "export" => {
            let force = rest.iter().any(|a| a == "--force");
            let positional: Vec<&String> = rest.iter().filter(|a| *a != "--force").collect();
            match positional.as_slice() {
                [dir] if !dir.starts_with('-') => export(dir, force, out),
                [] => return usage_error(err, "export needs a destination DIR"),
                _ => {
                    return usage_error(
                        err,
                        &format!(
                            "unexpected arguments: {}",
                            positional
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(" ")
                        ),
                    );
                }
            }
        }
        other => return usage_error(err, &format!("unknown subcommand: {other}")),
    };
    match result {
        Ok(()) => 0,
        Err(failure) => {
            let _ = writeln!(err, "fw spec: error: {failure}");
            1
        }
    }
}

fn usage_error(err: &mut dyn Write, message: &str) -> i32 {
    let _ = err.write_all(SPEC_USAGE.as_bytes());
    let _ = writeln!(err, "fw spec: error: {message}");
    2
}

fn write_out(out: &mut dyn Write, line: std::fmt::Arguments<'_>) -> Result<(), SpecError> {
    out.write_fmt(line)
        .and_then(|()| out.write_all(b"\n"))
        .map_err(|e| SpecError::Invalid(e.to_string()))
}

fn ls(json: bool, out: &mut dyn Write) -> Result<(), SpecError> {
    let project = cwd_bound_project()?;
    let dir = project.paths.specs();
    let files = spec_files(&dir)?;
    if json {
        let rows: Vec<SpecRow> = files
            .into_iter()
            .map(|path| SpecRow {
                archived: path
                    .components()
                    .next()
                    .is_some_and(|c| c.as_os_str() == "archive" && path.components().count() > 1),
                path,
            })
            .collect();
        let text = serde_json::to_string(&rows).map_err(|e| SpecError::Invalid(e.to_string()))?;
        return write_out(out, format_args!("{text}"));
    }
    if files.is_empty() {
        return write_out(out, format_args!("(no specs in {})", dir.display()));
    }
    for file in files {
        write_out(out, format_args!("{}", file.display()))?;
    }
    Ok(())
}

fn export(dir: &str, force: bool, out: &mut dyn Write) -> Result<(), SpecError> {
    let cwd = std::env::current_dir().map_err(io_err(Path::new(".")))?;
    let joined = cwd.join(dir);
    // Printed paths are canonical (no `..`), through the deepest existing
    // ancestor.
    let dest = canonical_prefix(&joined).unwrap_or(joined);
    let project = cwd_bound_project()?;
    let src = project.paths.specs();
    let report = export_specs(&src, &dest, force)?;
    for path in &report.written {
        write_out(out, format_args!("wrote {}", path.display()))?;
    }
    write_out(
        out,
        format_args!(
            "exported {} file(s) from {} to {} ({} written, {} unchanged)",
            report.written.len() + report.unchanged.len(),
            src.display(),
            dest.display(),
            report.written.len(),
            report.unchanged.len()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn run(args: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_with(&args, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn put(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn help_and_usage_errors() {
        let (code, out, _) = run(&["--help"]);
        assert_eq!((code, out.as_str()), (0, SPEC_USAGE));
        assert_eq!(run(&["export", "--help"]).0, 0);
        assert_eq!(run(&[]).0, 2);
        let (code, _, err) = run(&["frob"]);
        assert_eq!(code, 2);
        assert!(err.contains("unknown subcommand: frob"), "{err}");
        assert_eq!(run(&["ls", "--yaml"]).0, 2);
        let (code, _, err) = run(&["export"]);
        assert_eq!(code, 2);
        assert!(err.contains("export needs a destination DIR"), "{err}");
        assert_eq!(run(&["export", "a", "b"]).0, 2);
        assert_eq!(run(&["export", "--frob"]).0, 2);
    }

    #[test]
    fn export_copies_the_tree_byte_for_byte_and_creates_dest() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("specs");
        let body: &[u8] = b"# Feature: probe\n\r\nbinary \x00\xff tail";
        put(&src, "00001-FEATURE-probe.md", body);
        put(&src, "archive/00000-FEATURE-old.md", b"old\n");
        let dest = tmp.path().join("out").join("nested");
        let report = export_specs(&src, &dest, false).unwrap();
        assert_eq!(
            report.written,
            vec![
                dest.join("00001-FEATURE-probe.md"),
                dest.join("archive/00000-FEATURE-old.md"),
            ]
        );
        assert_eq!(fs::read(dest.join("00001-FEATURE-probe.md")).unwrap(), body);
        assert_eq!(
            fs::read(dest.join("archive/00000-FEATURE-old.md")).unwrap(),
            b"old\n"
        );

        // A re-run is a no-op: identical files are reported unchanged.
        let again = export_specs(&src, &dest, false).unwrap();
        assert!(again.written.is_empty());
        assert_eq!(again.unchanged.len(), 2);
    }

    #[test]
    fn export_refuses_a_differing_file_unless_forced() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("specs");
        put(&src, "00001-FEATURE-a.md", b"new\n");
        put(&src, "00002-FEATURE-b.md", b"b\n");
        let dest = tmp.path().join("out");
        put(&dest, "00001-FEATURE-a.md", b"local edit\n");

        let err = export_specs(&src, &dest, false).unwrap_err().to_string();
        assert!(
            err.contains("refusing to overwrite 1 differing file"),
            "{err}"
        );
        assert!(err.contains("00001-FEATURE-a.md"), "{err}");
        // Nothing was written, not even the non-conflicting file.
        assert!(!dest.join("00002-FEATURE-b.md").exists());
        assert_eq!(
            fs::read(dest.join("00001-FEATURE-a.md")).unwrap(),
            b"local edit\n"
        );

        let report = export_specs(&src, &dest, true).unwrap();
        assert_eq!(report.written.len(), 2);
        assert_eq!(fs::read(dest.join("00001-FEATURE-a.md")).unwrap(), b"new\n");
    }

    #[test]
    fn export_refuses_a_directory_in_a_files_place_and_a_dest_inside_src() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("specs");
        put(&src, "00001-FEATURE-a.md", b"a\n");
        let dest = tmp.path().join("out");
        fs::create_dir_all(dest.join("00001-FEATURE-a.md")).unwrap();
        let err = export_specs(&src, &dest, true).unwrap_err().to_string();
        assert!(err.contains("is a directory"), "{err}");

        let err = export_specs(&src, &src.join("copy"), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("inside the specs dir"), "{err}");
        assert!(!src.join("copy").exists());
    }

    #[test]
    fn export_of_a_missing_specs_dir_creates_an_empty_dest() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("out");
        let report = export_specs(&tmp.path().join("absent"), &dest, false).unwrap();
        assert_eq!(report, ExportReport::default());
        assert!(dest.is_dir());
    }

    #[test]
    fn spec_files_lists_sorted_relative_paths() {
        let tmp = TempDir::new().unwrap();
        put(tmp.path(), "b.md", b"");
        put(tmp.path(), "archive/a.md", b"");
        put(tmp.path(), "a.md", b"");
        assert_eq!(
            spec_files(tmp.path()).unwrap(),
            vec![
                PathBuf::from("a.md"),
                PathBuf::from("archive/a.md"),
                PathBuf::from("b.md"),
            ]
        );
        assert!(spec_files(&tmp.path().join("nope")).unwrap().is_empty());
    }
}
