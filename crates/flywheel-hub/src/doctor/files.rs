//! The state dir (owner, modes, secrets) and the config files in it.

use super::{Check, Level};
use crate::util::shell_path;
use std::path::Path;

/// The state dir and its secrets: owned by this user, private.
pub fn state_dir_checks(dir: &Path) -> Vec<Check> {
    use std::os::unix::fs::MetadataExt;
    let id = "state dir";
    let meta = match std::fs::metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return vec![Check::new(
                id,
                Level::Info,
                format!(
                    "{} does not exist yet (the first serve creates it, mode 700)",
                    dir.display()
                ),
            )];
        }
        Err(e) => {
            return vec![
                Check::new(id, Level::Fail, format!("{}: {e}", dir.display()))
                    .fix("fix its permissions, or pass another --state-dir"),
            ];
        }
    };
    let uid = unsafe_getuid();
    let mut checks = Vec::new();
    let mode = meta.mode() & 0o777;
    if !meta.is_dir() {
        checks.push(
            Check::new(
                id,
                Level::Fail,
                format!("{} is not a directory", dir.display()),
            )
            .fix("move it away, or pass another --state-dir"),
        );
        return checks;
    }
    if meta.uid() != uid {
        checks.push(
            Check::new(
                id,
                Level::Fail,
                format!(
                    "{} is owned by uid {}, not you ({uid})",
                    dir.display(),
                    meta.uid()
                ),
            )
            .fix(format!(
                "sudo chown -R $(id -u):$(id -g) {}",
                shell_path(dir)
            )),
        );
    } else if !writable(dir) {
        checks.push(
            Check::new(
                id,
                Level::Fail,
                format!("{} is not writable", dir.display()),
            )
            .fix(format!("chmod u+rwx {}", shell_path(dir))),
        );
    } else if mode & 0o077 != 0 {
        checks.push(
            Check::new(
                id,
                Level::Warn,
                format!(
                    "{} is mode {mode:o}: other users can look inside (the hub sets 700 when \
                     it opens it)",
                    dir.display()
                ),
            )
            .fix(format!("chmod 700 {}", shell_path(dir))),
        );
    } else {
        checks.push(Check::new(
            id,
            Level::Ok,
            format!("{} (mode {mode:o}, yours)", dir.display()),
        ));
    }
    for secret in SECRET_FILES {
        let path = dir.join(secret);
        if let Ok(meta) = std::fs::metadata(&path) {
            let mode = meta.mode() & 0o777;
            if mode & 0o077 != 0 {
                checks.push(
                    Check::new(
                        "secrets",
                        Level::Fail,
                        format!(
                            "{} is mode {mode:o}: readable by other users",
                            path.display()
                        ),
                    )
                    .fix(format!(
                        "chmod 600 {}{}",
                        shell_path(&path),
                        if *secret == crate::auth::TOKEN_FILE {
                            "; then `fw hub token --rotate` (it may have been read)"
                        } else if *secret == crate::notify::CONFIG_FILE {
                            "; the hub ignores it (no push notifications) until then"
                        } else {
                            ""
                        }
                    )),
                );
            }
        }
    }
    // Directories of private files: recordings (whole terminal output)
    // and plugin logs (what plugins printed), written 0600.
    for (name, what) in PRIVATE_DIRS {
        let sub = dir.join(name);
        let Ok(entries) = std::fs::read_dir(&sub) else {
            continue;
        };
        let open: Vec<String> = entries
            .flatten()
            .filter(|e| {
                e.metadata()
                    .is_ok_and(|m| m.is_file() && m.mode() & 0o077 != 0)
            })
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        if open.is_empty() {
            continue;
        }
        let shown: Vec<&str> = open.iter().take(3).map(String::as_str).collect();
        checks.push(
            Check::new(
                "secrets",
                Level::Fail,
                format!(
                    "{} in {} {} readable by other users ({}{})",
                    crate::util::plural(open.len(), what),
                    sub.display(),
                    if open.len() == 1 { "is" } else { "are" },
                    shown.join(", "),
                    if open.len() > shown.len() {
                        ", ..."
                    } else {
                        ""
                    }
                ),
            )
            .fix(format!("chmod -R go-rwx {}", shell_path(&sub))),
        );
    }
    checks
}

/// State files holding credentials or their digests: never readable by
/// other users.
const SECRET_FILES: &[&str] = &[
    crate::auth::TOKEN_FILE,
    crate::auth::LOGINS_FILE,
    crate::notify::CONFIG_FILE,
    crate::machines::MACHINES_FILE,
    crate::peers::PEERS_FILE,
    crate::share::SHARES_FILE,
];

/// State directories whose files are private: (directory, what a file is).
const PRIVATE_DIRS: &[(&str, &str)] = &[
    (crate::record::RECORDINGS_DIR, "recording file"),
    (crate::plugins::LOG_DIR, "plugin log"),
];

fn unsafe_getuid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated string for the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

// --- config files -------------------------------------------------------------------

/// `hub.toml`, `notify.toml`, `settings.json`, and plugin manifests parse.
/// `hub.toml` is checked by the validation `fw hub config check` and the
/// hub's start run (every key, `[recordings]` and `[shares]` included).
pub fn config_checks(dir: &Path, home: Option<&Path>) -> Vec<Check> {
    let mut checks = Vec::new();
    let hub_toml = dir.join(crate::config::CONFIG_FILE);
    checks.push(match crate::config::read_file(&hub_toml) {
        Ok(None) => Check::new("hub.toml", Level::Ok, "none (every setting is its default)"),
        Ok(Some(file)) => {
            match crate::config::resolve(&file, &crate::config::Flags::default(), home) {
                Ok(_) => Check::new("hub.toml", Level::Ok, format!("{} is valid", hub_toml.display())),
                Err(errors) => errors.into_iter().fold(
                    Check::new("hub.toml", Level::Fail, format!("{} has bad values", hub_toml.display()))
                        .fix("fix them (`fw hub config check` re-checks); the hub refuses to start until then"),
                    Check::detail,
                ),
            }
        }
        Err(message) => Check::new("hub.toml", Level::Fail, message).fix(
            "correct it where the error points (`fw hub config check` re-checks, `fw docs hub-reference` \
             lists the keys); the hub refuses to start until then",
        ),
    });
    // Its content only: a mode that is not private is the secrets check's
    // finding (and does not hide a parse error here).
    let notify_path = dir.join(crate::notify::CONFIG_FILE);
    match std::fs::read_to_string(&notify_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => checks.push(Check::new(
            "notify.toml",
            Level::Ok,
            "none (no push notifications)",
        )),
        Err(e) => checks.push(
            Check::new(
                "notify.toml",
                Level::Fail,
                format!("{}: {e}", notify_path.display()),
            )
            .fix(format!("chmod 600 {}", shell_path(&notify_path))),
        ),
        Ok(text) => checks.push(
            match crate::notify::NotifyConfig::from_parts(Some(&text), None) {
                Ok(Some(config)) => Check::new(
                    "notify.toml",
                    Level::Ok,
                    format!("pushes to {}", config.host()),
                ),
                Ok(None) => Check::new("notify.toml", Level::Ok, "no push notifications"),
                Err(message) => Check::new(
                    "notify.toml",
                    Level::Fail,
                    format!("{}: {message}", notify_path.display()),
                )
                .fix(
                    "correct it: url = \"https://ntfy.sh/<topic>\" (required), token = \"...\", \
                 events = [\"blocked\", \"done\"]; until then the hub starts without push \
                 notifications",
                ),
            },
        ),
    }
    match crate::settings::read(&dir.join(crate::settings::SETTINGS_FILE)) {
        Ok(_) => {}
        Err(message) => checks.push(
            Check::new("settings.json", Level::Fail, message)
                .fix("fix or remove it (the UI's settings are not saved over it until then)"),
        ),
    }
    let broken: Vec<String> = crate::plugins::scan(dir)
        .into_iter()
        .filter_map(|p| p.manifest.err().map(|e| format!("{}: {e}", p.name)))
        .collect();
    if !broken.is_empty() {
        checks.push(
            broken.into_iter().fold(
                Check::new("plugins", Level::Warn, "some plugin manifests do not load")
                    .fix("fix their plugin.toml (`fw hub plugin ls` lists them)"),
                Check::detail,
            ),
        );
    }
    checks
}
