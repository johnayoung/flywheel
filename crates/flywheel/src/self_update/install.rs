//! The install dir rules, the lock, the recorded state, and the atomic
//! install of the `fw` / `flywheel` pair.

use flywheel_orchestrator::home::Home;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The two binaries, in install order.
pub const BINARIES: [&str; 2] = ["fw", "flywheel"];

/// The sha256 (hex) of a file's bytes.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

// --- install dir ---------------------------------------------------------------

/// Whether `dir` lies inside a cargo target dir: an ancestor (or `dir`
/// itself) carries cargo's `CACHEDIR.TAG` / `.rustc_info.json` markers, or
/// is named `target` beside a `Cargo.toml`.
pub fn inside_cargo_target(dir: &Path) -> bool {
    dir.ancestors().any(|ancestor| {
        let tagged = std::fs::read_to_string(ancestor.join("CACHEDIR.TAG"))
            .map(|tag| tag.contains("cargo"))
            .unwrap_or(false);
        let named = ancestor.file_name().is_some_and(|n| n == "target")
            && ancestor
                .parent()
                .is_some_and(|p| p.join("Cargo.toml").is_file());
        tagged || named || ancestor.join(".rustc_info.json").is_file()
    })
}

fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated string for the call's
    // duration; access(2) only reads it.
    unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

/// Resolve and validate the install dir: `--install-dir`, else the running
/// executable's dir. Refused when it lacks either binary, is inside a cargo
/// target dir, or is not writable.
pub fn resolve_install_dir(flag: Option<&Path>, current_exe: &Path) -> Result<PathBuf, String> {
    let raw = match flag {
        Some(dir) => dir.to_path_buf(),
        None => current_exe
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "cannot locate the running executable's directory".to_owned())?,
    };
    let dir = raw
        .canonicalize()
        .map_err(|e| format!("install dir {}: {e}", raw.display()))?;
    if !dir.is_dir() {
        return Err(format!("install dir {} is not a directory", dir.display()));
    }
    let missing: Vec<&str> = BINARIES
        .iter()
        .copied()
        .filter(|b| !dir.join(b).is_file())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "install dir {} lacks {} (install flywheel there first, or pass --install-dir)",
            dir.display(),
            missing.join(" and ")
        ));
    }
    if inside_cargo_target(&dir) {
        return Err(format!(
            "install dir {} is inside a cargo target dir; pass --install-dir",
            dir.display()
        ));
    }
    if !writable(&dir) {
        return Err(format!("install dir {} is not writable", dir.display()));
    }
    Ok(dir)
}

// --- lock ----------------------------------------------------------------------

/// The held `self-update/lock` flock; released on drop (or exit).
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

/// Take the exclusive non-blocking lock, writing this pid into the file.
/// A held lock refuses at once, naming the holder's pid.
pub fn take_lock(home: &Home) -> Result<Lock, String> {
    let path = home.self_update_lock();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    // SAFETY: flock(2) on a descriptor this function owns.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            let mut holder = String::new();
            let _ = file.read_to_string(&mut holder);
            let holder = holder.trim();
            let who = if holder.is_empty() {
                "an unknown pid".to_owned()
            } else {
                format!("pid {holder}")
            };
            return Err(format!(
                "another fw self-update holds {} ({who})",
                path.display()
            ));
        }
        return Err(format!("cannot lock {}: {error}", path.display()));
    }
    let pid = std::process::id();
    let written = file
        .set_len(0)
        .and_then(|()| file.seek(SeekFrom::Start(0)))
        .and_then(|_| writeln!(file, "{pid}"))
        .and_then(|()| file.flush());
    written.map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(Lock { _file: file })
}

// --- recorded state ------------------------------------------------------------

/// What the last successful update installed (`self-update/state.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub commit: String,
    pub checkout: PathBuf,
    pub install_dir: PathBuf,
    pub version: String,
    pub fw_sha256: String,
    pub flywheel_sha256: String,
    pub installed_at: String,
}

/// Read the record; a missing or unreadable record is `None` (the next run
/// is then never "already current").
pub fn read_record(home: &Home) -> Option<Record> {
    let body = std::fs::read_to_string(home.self_update_state()).ok()?;
    serde_json::from_str(&body).ok()
}

/// Write the record atomically (temp file + rename).
pub fn write_record(home: &Home, record: &Record) -> Result<(), String> {
    let path = home.self_update_state();
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let body = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, format!("{body}\n"))
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot write {}: {e}", path.display())
        })
}

/// Already current: HEAD is the recorded commit, the install dir is the
/// recorded one, and both installed binaries are the recorded bytes.
pub fn is_current(record: &Record, head: &str, install_dir: &Path) -> bool {
    if record.commit != head || record.install_dir != install_dir {
        return false;
    }
    let fw = sha256_file(&install_dir.join("fw"));
    let flywheel = sha256_file(&install_dir.join("flywheel"));
    fw.as_deref() == Ok(record.fw_sha256.as_str())
        && flywheel.as_deref() == Ok(record.flywheel_sha256.as_str())
}

// --- install -------------------------------------------------------------------

/// Copy `from` to `to` through a sibling temp file and an atomic rename,
/// mode 0755, synced before the rename.
fn atomic_copy(from: &Path, to: &Path) -> Result<(), String> {
    let name = to
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = to.with_file_name(format!(".{name}.self-update-{}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut src = File::open(from)?;
        let mut dst = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        std::io::copy(&mut src, &mut dst)?;
        dst.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        dst.sync_all()?;
        drop(dst);
        std::fs::rename(&tmp, to)
    })();
    result.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot install {} -> {}: {e}", from.display(), to.display())
    })
}

/// Install both artifacts into `install_dir`: copy the current pair to
/// `previous/` first, then replace each by atomic rename and verify the
/// installed bytes. On failure every binary already replaced is restored
/// from `previous/`; the error says whether that revert succeeded.
pub fn install(
    home: &Home,
    install_dir: &Path,
    artifacts: [(&str, &Path, &str); 2],
) -> Result<(), InstallError> {
    let previous = home.self_update_previous_dir();
    std::fs::create_dir_all(&previous).map_err(|e| {
        InstallError::NothingChanged(format!("cannot create {}: {e}", previous.display()))
    })?;
    for name in BINARIES {
        atomic_copy(&install_dir.join(name), &previous.join(name))
            .map_err(InstallError::NothingChanged)?;
    }
    let mut replaced: Vec<&str> = Vec::new();
    for (name, artifact, sha256) in artifacts {
        let target = install_dir.join(name);
        let outcome = atomic_copy(artifact, &target).and_then(|()| {
            replaced.push(name);
            let installed = sha256_file(&target)?;
            if installed == sha256 {
                Ok(())
            } else {
                Err(format!(
                    "{} has sha256 {installed} after install, expected {sha256}",
                    target.display()
                ))
            }
        });
        if let Err(error) = outcome {
            let mut revert_errors = Vec::new();
            for done in &replaced {
                if let Err(e) = atomic_copy(&previous.join(done), &install_dir.join(done)) {
                    revert_errors.push(e);
                }
            }
            return Err(if revert_errors.is_empty() {
                InstallError::Reverted(error)
            } else {
                InstallError::RevertFailed(format!("{error}; revert: {}", revert_errors.join("; ")))
            });
        }
    }
    Ok(())
}

/// Copy the `previous/` pair back into `install_dir`.
pub fn restore_previous(home: &Home, install_dir: &Path) -> Result<(), String> {
    let previous = home.self_update_previous_dir();
    let errors: Vec<String> = BINARIES
        .iter()
        .filter_map(|name| atomic_copy(&previous.join(name), &install_dir.join(name)).err())
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// How an install failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// Failed before any install-dir file changed.
    NothingChanged(String),
    /// Failed midway; the previous pair is back in place.
    Reverted(String),
    /// Failed midway and the revert failed too.
    RevertFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(dir: &Path, fw: &[u8], flywheel: &[u8]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("fw"), fw).unwrap();
        std::fs::write(dir.join("flywheel"), flywheel).unwrap();
    }

    #[test]
    fn sha256_matches_a_known_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f");
        std::fs::write(&file, b"abc").unwrap();
        assert_eq!(
            sha256_file(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn install_dir_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let bin = root.join("bin");
        seed(&bin, b"a", b"b");
        assert_eq!(
            resolve_install_dir(Some(&bin), Path::new("/x/fw")).unwrap(),
            bin
        );
        assert_eq!(resolve_install_dir(None, &bin.join("fw")).unwrap(), bin);

        let half = root.join("half");
        std::fs::create_dir_all(&half).unwrap();
        std::fs::write(half.join("fw"), b"a").unwrap();
        assert!(
            resolve_install_dir(Some(&half), Path::new("/x"))
                .unwrap_err()
                .contains("flywheel")
        );

        let target = root.join("ws/target/release");
        seed(&target, b"a", b"b");
        std::fs::write(root.join("ws/Cargo.toml"), "").unwrap();
        assert!(inside_cargo_target(&target));
        assert!(
            resolve_install_dir(Some(&target), Path::new("/x"))
                .unwrap_err()
                .contains("target")
        );

        let tagged = root.join("custom/release");
        seed(&tagged, b"a", b"b");
        std::fs::write(
            root.join("custom/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by cargo.\n",
        )
        .unwrap();
        assert!(inside_cargo_target(&tagged));
        assert!(!inside_cargo_target(&bin));

        assert!(resolve_install_dir(Some(&root.join("missing")), Path::new("/x")).is_err());
    }

    #[test]
    fn a_read_only_install_dir_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        seed(&bin, b"a", b"b");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = resolve_install_dir(Some(&bin), Path::new("/x"));
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        // root bypasses permission bits; only assert where they apply.
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            assert!(result.unwrap_err().contains("not writable"));
        }
    }

    #[test]
    fn the_lock_refuses_a_second_holder_naming_the_first() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        let held = take_lock(&home).unwrap();
        let body = std::fs::read_to_string(home.self_update_lock()).unwrap();
        assert_eq!(body.trim(), std::process::id().to_string());
        // flock is per open file description: a second open in this
        // process conflicts like another process would.
        let err = take_lock(&home).unwrap_err();
        assert!(
            err.contains(&format!("pid {}", std::process::id())),
            "{err}"
        );
        drop(held);
        take_lock(&home).unwrap();
    }

    #[test]
    fn record_round_trips_and_decides_current() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(home.self_update_dir()).unwrap();
        assert_eq!(read_record(&home), None);
        let bin = tmp.path().join("bin");
        seed(&bin, b"fw-bytes", b"flywheel-bytes");
        let record = Record {
            commit: "c".repeat(40),
            checkout: PathBuf::from("/src"),
            install_dir: bin.clone(),
            version: "0.1.0".into(),
            fw_sha256: sha256_file(&bin.join("fw")).unwrap(),
            flywheel_sha256: sha256_file(&bin.join("flywheel")).unwrap(),
            installed_at: "2026-11-10T00:00:00Z".into(),
        };
        write_record(&home, &record).unwrap();
        assert_eq!(read_record(&home).as_ref(), Some(&record));
        assert!(is_current(&record, &"c".repeat(40), &bin));
        assert!(!is_current(&record, &"d".repeat(40), &bin));
        std::fs::write(bin.join("flywheel"), b"changed").unwrap();
        assert!(!is_current(&record, &"c".repeat(40), &bin));
    }

    #[test]
    fn install_keeps_the_previous_pair_and_replaces_both() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let bin = tmp.path().join("bin");
        seed(&bin, b"old-fw", b"old-flywheel");
        let art = tmp.path().join("art");
        seed(&art, b"new-fw", b"new-flywheel");
        let fw_sha = sha256_file(&art.join("fw")).unwrap();
        let fly_sha = sha256_file(&art.join("flywheel")).unwrap();
        install(
            &home,
            &bin,
            [
                ("fw", &art.join("fw"), &fw_sha),
                ("flywheel", &art.join("flywheel"), &fly_sha),
            ],
        )
        .unwrap();
        assert_eq!(std::fs::read(bin.join("fw")).unwrap(), b"new-fw");
        assert_eq!(
            std::fs::read(bin.join("flywheel")).unwrap(),
            b"new-flywheel"
        );
        let prev = home.self_update_previous_dir();
        assert_eq!(std::fs::read(prev.join("fw")).unwrap(), b"old-fw");
        assert_eq!(
            std::fs::read(prev.join("flywheel")).unwrap(),
            b"old-flywheel"
        );
        let mode = std::fs::metadata(bin.join("fw"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        let leftovers: Vec<_> = std::fs::read_dir(&bin).unwrap().collect();
        assert_eq!(leftovers.len(), 2, "temp files left in the install dir");
    }

    #[test]
    fn a_failed_second_binary_reverts_the_first() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let bin = tmp.path().join("bin");
        seed(&bin, b"old-fw", b"old-flywheel");
        let art = tmp.path().join("art");
        seed(&art, b"new-fw", b"new-flywheel");
        let fw_sha = sha256_file(&art.join("fw")).unwrap();
        let err = install(
            &home,
            &bin,
            [
                ("fw", &art.join("fw"), &fw_sha),
                ("flywheel", &art.join("missing"), "0"),
            ],
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::Reverted(_)), "{err:?}");
        assert_eq!(std::fs::read(bin.join("fw")).unwrap(), b"old-fw");
        assert_eq!(
            std::fs::read(bin.join("flywheel")).unwrap(),
            b"old-flywheel"
        );
    }
}
