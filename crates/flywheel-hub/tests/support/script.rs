//! Stand-in scripts, for the integration tests and (via `#[path]`) the
//! crate's unit tests.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Write `contents` to `path` as an executable (0755) script that this
/// process can exec at once.
///
/// A short-lived `/bin/sh` child writes the file; this process never holds
/// a writable descriptor to it. One here -- close-on-exec or not -- is
/// copied into every child another test thread forks meanwhile (PTY
/// spawns, agent CLIs), and until that child execs, exec of the script
/// fails with ETXTBSY ("Text file busy").
pub fn write_script(path: &Path, contents: &str) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn /bin/sh to write a script");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(contents.as_bytes())
        .expect("write the script");
    let status = child.wait().expect("wait for /bin/sh");
    assert!(status.success(), "writing {}: {status}", path.display());
}
