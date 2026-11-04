//! A session's recent output, kept so a viewer whose link dropped can
//! resume from the last byte it had instead of a full snapshot (see
//! [`super::PtySession::reattach`]), and so a viewer on a slow link can be
//! sent the bytes since its last update when they are few.

use axum::body::Bytes;
use std::collections::VecDeque;

/// Output bytes kept per session for resuming.
pub const RECENT_BYTES: usize = 512 * 1024;

/// One thing a viewer was sent, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecentItem {
    Output(Bytes),
    Resize { cols: u16, rows: u16 },
}

/// The last [`RECENT_BYTES`] of output (and the resizes between), each
/// item at the output offset (`seq`) it came at.
#[derive(Debug)]
pub(crate) struct Recent {
    items: VecDeque<(u64, RecentItem)>,
    bytes: usize,
    /// The earliest offset a replay can start from.
    start: u64,
}

impl Recent {
    /// Nothing kept yet; output so far ends at `seq`.
    pub(crate) fn new(seq: u64) -> Self {
        Self {
            items: VecDeque::new(),
            bytes: 0,
            start: seq,
        }
    }

    pub(crate) fn push(&mut self, seq: u64, item: RecentItem) {
        if let RecentItem::Output(bytes) = &item {
            self.bytes += bytes.len();
        }
        self.items.push_back((seq, item));
        while self.bytes > RECENT_BYTES {
            let Some((at, dropped)) = self.items.pop_front() else {
                break;
            };
            let end = match &dropped {
                RecentItem::Output(bytes) => {
                    self.bytes -= bytes.len();
                    at + bytes.len() as u64
                }
                // A viewer at `at` may not have had this resize yet.
                RecentItem::Resize { .. } => at + 1,
            };
            self.start = self.start.max(end);
        }
    }

    /// Everything after offset `seq` up to `end` (the output so far), or
    /// `None` when that is no longer kept (or `seq` is not an offset of
    /// this output).
    pub(crate) fn since(&self, seq: u64, end: u64) -> Option<Vec<RecentItem>> {
        if seq < self.start || seq > end {
            return None;
        }
        let mut out = Vec::new();
        for (at, item) in &self.items {
            match item {
                RecentItem::Output(bytes) => {
                    let stop = at + bytes.len() as u64;
                    if stop <= seq {
                        continue;
                    }
                    let from = usize::try_from(seq.saturating_sub(*at)).unwrap_or(usize::MAX);
                    out.push(RecentItem::Output(bytes.slice(from..)));
                }
                RecentItem::Resize { .. } => {
                    if *at >= seq {
                        out.push(item.clone());
                    }
                }
            }
        }
        Some(out)
    }
}

/// Output bytes among `items`.
pub fn output_len(items: &[RecentItem]) -> usize {
    items
        .iter()
        .map(|item| match item {
            RecentItem::Output(bytes) => bytes.len(),
            RecentItem::Resize { .. } => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(s: &str) -> RecentItem {
        RecentItem::Output(Bytes::copy_from_slice(s.as_bytes()))
    }

    fn joined(items: &[RecentItem]) -> String {
        items
            .iter()
            .map(|i| match i {
                RecentItem::Output(b) => String::from_utf8_lossy(b).into_owned(),
                RecentItem::Resize { cols, rows } => format!("[{cols}x{rows}]"),
            })
            .collect()
    }

    #[test]
    fn replays_from_any_offset_mid_chunk_with_resizes_in_order() {
        let mut recent = Recent::new(100);
        recent.push(100, out("hello "));
        recent.push(106, RecentItem::Resize { cols: 80, rows: 24 });
        recent.push(106, out("world"));
        let end = 111;
        assert_eq!(
            joined(&recent.since(100, end).unwrap()),
            "hello [80x24]world"
        );
        assert_eq!(joined(&recent.since(103, end).unwrap()), "lo [80x24]world");
        // At 106 the resize may not have been seen yet: it is replayed.
        assert_eq!(joined(&recent.since(106, end).unwrap()), "[80x24]world");
        assert_eq!(joined(&recent.since(111, end).unwrap()), "");
        assert!(recent.since(99, end).is_none(), "before what is kept");
        assert!(recent.since(112, end).is_none(), "past the output");
    }

    #[test]
    fn forgets_the_oldest_beyond_its_budget() {
        let mut recent = Recent::new(0);
        let chunk = "x".repeat(RECENT_BYTES / 4);
        let mut seq = 0;
        for _ in 0..6 {
            recent.push(seq, out(&chunk));
            seq += chunk.len() as u64;
        }
        recent.push(seq, RecentItem::Resize { cols: 10, rows: 5 });
        assert!(recent.since(0, seq).is_none());
        let kept = recent.since(seq - 4 * chunk.len() as u64, seq).unwrap();
        assert_eq!(output_len(&kept), 4 * chunk.len());
        assert!(recent.bytes <= RECENT_BYTES);
        // A resize falling off the front: a viewer at its offset cannot
        // resume (it may not have had it).
        let mut recent = Recent::new(0);
        recent.push(0, RecentItem::Resize { cols: 10, rows: 5 });
        recent.push(0, out(&"y".repeat(RECENT_BYTES)));
        recent.push(RECENT_BYTES as u64, out("z"));
        assert!(recent.since(0, RECENT_BYTES as u64 + 1).is_none());
    }
}
