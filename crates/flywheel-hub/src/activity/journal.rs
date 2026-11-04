//! The journal's storage: rows in memory (newest last) mirroring the
//! JSON-lines files on disk, appended one line per row and rotated by size
//! and age (see the module doc of [`crate::activity`]).

use super::{Draft, Filter, JOURNAL_FILE, Row};
use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The journal's bounds.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The current file rotates past this many bytes.
    pub file_cap: u64,
    /// Rotated files kept.
    pub keep_rotated: usize,
    /// The current file rotates once its first row is this old.
    pub rotate_age: chrono::Duration,
    /// Rows older than this are forgotten.
    pub max_age: chrono::Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            file_cap: super::FILE_CAP,
            keep_rotated: super::KEEP_ROTATED,
            rotate_age: chrono::Duration::from_std(super::ROTATE_AGE)
                .unwrap_or(chrono::Duration::days(1)),
            max_age: chrono::Duration::days(super::MAX_AGE_DAYS),
        }
    }
}

/// One file's rows (oldest file first; the last is the current file).
#[derive(Debug, Clone, Copy)]
struct Span {
    first_id: u64,
    first_at: DateTime<Utc>,
    last_at: DateTime<Utc>,
}

/// The journal of one hub.
#[derive(Debug)]
pub struct Journal {
    dir: PathBuf,
    limits: Limits,
    rows: VecDeque<Row>,
    /// Oldest first; the last is the current file (`None`: empty or absent).
    spans: VecDeque<Span>,
    current_bytes: u64,
    current_span: Option<Span>,
    next_id: u64,
}

/// `activity.jsonl` (`n` = 0) or `activity.<n>.jsonl`.
fn file_path(dir: &Path, n: usize) -> PathBuf {
    if n == 0 {
        dir.join(JOURNAL_FILE)
    } else {
        dir.join(format!("activity.{n}.jsonl"))
    }
}

/// The rows of one file (unreadable lines skipped: a torn last line after
/// a crash loses that row only).
fn read_rows(path: &Path) -> (Vec<Row>, u64) {
    let Ok(bytes) = std::fs::read(path) else {
        return (Vec::new(), 0);
    };
    let rows = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<Row>(line).ok())
        .collect();
    (rows, bytes.len() as u64)
}

fn span_of(rows: &[Row]) -> Option<Span> {
    Some(Span {
        first_id: rows.first()?.id,
        first_at: rows.first()?.at,
        last_at: rows.last()?.at,
    })
}

impl Journal {
    /// Read the journal in `dir` (an absent one is empty), dropping rows and
    /// rotated files past `limits.max_age`.
    pub fn open(dir: &Path, limits: Limits, now: DateTime<Utc>) -> Self {
        let cutoff = now - limits.max_age;
        renumber(dir, limits.keep_rotated);
        let mut rows = VecDeque::new();
        let mut spans = VecDeque::new();
        let mut current_bytes = 0;
        let mut current_span = None;
        let mut next_id = 1;
        for n in (0..=limits.keep_rotated).rev() {
            let path = file_path(dir, n);
            let (mut found, bytes) = read_rows(&path);
            found.sort_by_key(|r| r.id);
            if let Some(last) = found.last() {
                next_id = next_id.max(last.id + 1);
            }
            if n == 0 {
                current_bytes = bytes;
                current_span = span_of(&found);
            } else if found.last().is_none_or(|r| r.at < cutoff) {
                // Nothing in it is kept: gone.
                let _ = std::fs::remove_file(&path);
                continue;
            } else if let Some(span) = span_of(&found) {
                spans.push_back(span);
            }
            rows.extend(found.into_iter().filter(|r| r.at >= cutoff));
        }
        let mut journal = Self {
            dir: dir.to_path_buf(),
            limits,
            rows,
            spans,
            current_bytes,
            current_span,
            next_id,
        };
        journal.rows.make_contiguous().sort_by_key(|r| r.id);
        journal.forget_old(now);
        journal
    }

    /// Append `draft` at `at`; returns the row written (kept in memory even
    /// when the file write fails, which is logged).
    pub fn append(&mut self, draft: Draft, at: DateTime<Utc>) -> Row {
        let row = draft.into_row(self.next_id, at);
        self.next_id += 1;
        let mut line = serde_json::to_vec(&row).unwrap_or_default();
        line.push(b'\n');
        let full =
            self.current_bytes > 0 && self.current_bytes + line.len() as u64 > self.limits.file_cap;
        let stale = self
            .current_span
            .is_some_and(|span| at - span.first_at >= self.limits.rotate_age);
        if (full || stale)
            && let Err(e) = self.rotate()
        {
            eprintln!("fw hub: activity journal: cannot rotate: {e}");
        }
        if let Err(e) = append_line(&file_path(&self.dir, 0), &line) {
            eprintln!("fw hub: activity journal: cannot write: {e}");
        } else {
            self.current_bytes += line.len() as u64;
        }
        match &mut self.current_span {
            Some(span) => span.last_at = row.at,
            None => {
                self.current_span = Some(Span {
                    first_id: row.id,
                    first_at: row.at,
                    last_at: row.at,
                });
            }
        }
        self.rows.push_back(row.clone());
        self.forget_old(at);
        row
    }

    /// The current file becomes `activity.1.jsonl` (each rotated one moves
    /// up; the oldest past [`Limits::keep_rotated`] is deleted, and its rows
    /// forgotten).
    fn rotate(&mut self) -> std::io::Result<()> {
        let keep = self.limits.keep_rotated;
        if keep == 0 {
            std::fs::remove_file(file_path(&self.dir, 0))?;
        } else {
            let _ = std::fs::remove_file(file_path(&self.dir, keep));
            for n in (1..keep).rev() {
                let from = file_path(&self.dir, n);
                if from.exists() {
                    std::fs::rename(&from, file_path(&self.dir, n + 1))?;
                }
            }
            std::fs::rename(file_path(&self.dir, 0), file_path(&self.dir, 1))?;
        }
        if let Some(span) = self.current_span.take() {
            self.spans.push_back(span);
        }
        self.current_bytes = 0;
        while self.spans.len() > keep {
            self.spans.pop_front();
        }
        let oldest_kept = self
            .spans
            .front()
            .map_or(self.next_id, |span| span.first_id);
        while self.rows.front().is_some_and(|r| r.id < oldest_kept) {
            self.rows.pop_front();
        }
        Ok(())
    }

    /// Forget rows past the age limit, and delete rotated files holding
    /// only such rows.
    fn forget_old(&mut self, now: DateTime<Utc>) {
        let cutoff = now - self.limits.max_age;
        while self.rows.front().is_some_and(|r| r.at < cutoff) {
            self.rows.pop_front();
        }
        while self.spans.front().is_some_and(|span| span.last_at < cutoff) {
            // The oldest rotated file is the highest-numbered one present.
            let n = self.spans.len();
            let _ = std::fs::remove_file(file_path(&self.dir, n));
            self.spans.pop_front();
        }
    }

    /// Rows matching `filter` newest first, before row `before` (`None`: from
    /// the newest), at most `limit`; and whether more match past them.
    pub fn page(&self, filter: &Filter, before: Option<u64>, limit: usize) -> (Vec<Row>, bool) {
        let mut out = Vec::new();
        let mut more = false;
        for row in self.rows.iter().rev() {
            if before.is_some_and(|b| row.id >= b) {
                continue;
            }
            if filter.since.is_some_and(|since| row.at < since) {
                break;
            }
            if !filter.matches(row) {
                continue;
            }
            if out.len() == limit {
                more = true;
                break;
            }
            out.push(row.clone());
        }
        (out, more)
    }

    /// Every row matching `filter`, oldest first.
    pub fn matching(&self, filter: &Filter) -> Vec<Row> {
        self.rows
            .iter()
            .filter(|r| filter.matches(r))
            .cloned()
            .collect()
    }

    /// Rows held.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The id the next row gets.
    pub fn next_id(&self) -> u64 {
        self.next_id
    }
}

/// Close gaps in the rotated files' numbers (one deleted by hand), so the
/// oldest is always `activity.<count>.jsonl`; files past `keep` go.
fn renumber(dir: &Path, keep: usize) {
    let present: Vec<usize> = (1..=keep.max(1) * 2)
        .filter(|n| file_path(dir, *n).exists())
        .collect();
    for (slot, n) in present.iter().enumerate() {
        let want = slot + 1;
        if want > keep {
            let _ = std::fs::remove_file(file_path(dir, *n));
        } else if *n != want {
            let _ = std::fs::rename(file_path(dir, *n), file_path(dir, want));
        }
    }
}

/// Append `line` to `path` (created 0600).
fn append_line(path: &Path, line: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata()?.permissions().mode() & 0o777;
        if mode != 0o600 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    // One write per row: a reader never sees half a line but at a crash.
    file.write_all(line)
}
