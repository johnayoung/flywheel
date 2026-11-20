//! Processes on this machine, read from `/proc` (Linux; elsewhere they
//! read as unknown): a process's parent chain and its command line. Used
//! to tell a re-entrant lock ask (its holder is the asker's ancestor) from
//! a real wait, and to say what a holder is doing.

/// A process's parent pid (`/proc/<pid>/stat` field 4).
pub fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command (field 2) may hold spaces and parentheses: count from
    // after its last closing parenthesis.
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// `pid`'s ancestors, nearest first, up to (not including) pid 0; at most
/// 256 (a loop in a racing read ends the walk).
pub fn ancestors(pid: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut at = pid;
    while let Some(up) = parent(at) {
        if up == 0 || up == at || found.contains(&up) || found.len() >= 256 {
            break;
        }
        found.push(up);
        at = up;
    }
    found
}

/// Whether `ancestor` is `pid` itself or one of its ancestors.
pub fn is_within(pid: u32, ancestor: u32) -> bool {
    pid == ancestor || ancestors(pid).contains(&ancestor)
}

/// A process's command line, its arguments joined by spaces and cut to
/// `max` characters; `None` when it is gone (or not readable).
pub fn command(pid: u32, max: usize) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let text = raw
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return None;
    }
    Some(if text.chars().count() > max {
        let cut: String = text.chars().take(max.saturating_sub(3)).collect();
        format!("{cut}...")
    } else {
        text
    })
}

/// `pid (command)`, or just the pid when its command cannot be read.
pub fn describe(pid: u32) -> String {
    match command(pid, 160) {
        Some(cmd) => format!("pid {pid} ({cmd})"),
        None => format!("pid {pid}"),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_child_sees_this_process_among_its_ancestors() {
        let me = std::process::id();
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        assert_eq!(parent(child.id()), Some(me));
        assert!(is_within(child.id(), me));
        assert!(!is_within(me, child.id()));
        assert!(command(child.id(), 100).unwrap().starts_with("sleep 5"));
        let _ = child.kill();
        let _ = child.wait();
        assert!(!ancestors(me).is_empty());
        assert!(describe(me).starts_with(&format!("pid {me} (")));
    }
}
