//! Incremental reading of a JSON Lines file that is being appended to.

use super::{MAX_LINE_BYTES, READ_CHUNK};
use std::fs::File;
use std::io::{Read as _, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Instant;

/// What [`JsonlTail::read`] hands out.
#[derive(Debug, PartialEq, Eq)]
pub enum Read<'a> {
    /// The file at the path was replaced or truncated: it is read again
    /// from the start (forget what came from it before).
    Restart,
    /// One complete line (without its newline), starting at byte `start`.
    Line { bytes: &'a [u8], start: u64 },
}

/// A reader of one JSONL file from a byte offset: no re-reads, the
/// incomplete last line held until its newline arrives, lines longer than
/// [`MAX_LINE_BYTES`] skipped unread.
#[derive(Debug, Default)]
pub struct JsonlTail {
    pub(crate) path: PathBuf,
    /// Bytes consumed from the file.
    offset: u64,
    /// Where the incomplete last line starts (`offset` minus `pending`,
    /// or inside a skipped line): the position to resume from.
    line_start: u64,
    /// (device, inode) when last read: a different file at the path
    /// (rotation) is read from the start.
    identity: Option<(u64, u64)>,
    /// The incomplete last line.
    pub(crate) pending: Vec<u8>,
    /// Inside an over-long line: drop bytes up to its newline.
    skipping: bool,
}

impl JsonlTail {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            ..Self::default()
        }
    }

    /// A tail resumed at `offset` (the start of a line) of the file that
    /// had `identity`.
    pub fn resume(path: PathBuf, offset: u64, identity: Option<(u64, u64)>) -> Self {
        Self {
            path,
            offset,
            line_start: offset,
            identity,
            ..Self::default()
        }
    }

    /// The start of the first line not yet handed out: where to resume.
    pub fn position(&self) -> u64 {
        self.line_start
    }

    /// The file's (device, inode) when last read.
    pub fn identity(&self) -> Option<(u64, u64)> {
        self.identity
    }

    /// Bytes held of the incomplete last line.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Read what was appended since the last call, handing `each` the
    /// complete lines as they are read (a long backlog is never held
    /// whole). Stops early at `deadline`, if given; returns whether the
    /// file was read to its end. A read that fails part way keeps what
    /// was handed out (the next call goes on from there).
    pub fn read(
        &mut self,
        deadline: Option<Instant>,
        each: &mut dyn FnMut(Read<'_>),
    ) -> std::io::Result<bool> {
        let mut file = File::open(&self.path)?;
        let meta = file.metadata()?;
        let identity = crate::util::file_identity(&meta);
        if self.identity.is_some_and(|known| known != identity) || meta.len() < self.offset {
            // Replaced or truncated: start over.
            self.offset = 0;
            self.line_start = 0;
            self.pending.clear();
            self.skipping = false;
            each(Read::Restart);
        }
        self.identity = Some(identity);
        if meta.len() == self.offset {
            return Ok(true);
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut chunk = vec![0u8; READ_CHUNK];
        loop {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(false);
            }
            let n = file.read(&mut chunk)?;
            if n == 0 {
                return Ok(true);
            }
            let base = self.offset;
            self.offset += n as u64;
            let mut rest = &chunk[..n];
            let mut at_in_chunk = 0usize;
            while let Some(at) = rest.iter().position(|&b| b == b'\n') {
                let (head, tail) = rest.split_at(at);
                rest = &tail[1..];
                at_in_chunk += at + 1;
                let start = self.line_start;
                self.line_start = base + at_in_chunk as u64;
                if self.skipping {
                    self.skipping = false;
                    continue;
                }
                if self.pending.len() + head.len() > MAX_LINE_BYTES {
                    // Its end arrived in this chunk: still too long.
                    self.pending = Vec::new();
                } else if self.pending.is_empty() {
                    each(Read::Line { bytes: head, start });
                } else {
                    self.pending.extend_from_slice(head);
                    let line = std::mem::take(&mut self.pending);
                    each(Read::Line {
                        bytes: &line,
                        start,
                    });
                }
            }
            if !self.skipping {
                if self.pending.len() + rest.len() > MAX_LINE_BYTES {
                    self.pending = Vec::new();
                    self.skipping = true;
                } else {
                    self.pending.extend_from_slice(rest);
                }
            }
        }
    }
}
