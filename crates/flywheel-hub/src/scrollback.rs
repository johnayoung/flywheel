//! A bounded ring of raw terminal output bytes.
//!
//! Every byte a session's PTY emits is appended here before it is broadcast
//! to attached clients, so a client that attaches later (or reconnects after a
//! drop) replays the most recent `capacity` bytes and then continues live.
//! When the ring is full the oldest bytes are discarded; the trim point is
//! advanced past any UTF-8 continuation bytes so a replay never starts in the
//! middle of a multi-byte character.

use std::collections::VecDeque;

/// Default per-session scrollback: 2 MiB of raw output.
pub const DEFAULT_SCROLLBACK_BYTES: usize = 2 * 1024 * 1024;

/// The bounded output ring.
#[derive(Debug, Clone)]
pub struct Scrollback {
    buf: VecDeque<u8>,
    capacity: usize,
    total: u64,
}

impl Scrollback {
    /// An empty ring holding at most `capacity` bytes (minimum 1).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            buf: VecDeque::with_capacity(capacity.min(64 * 1024)),
            capacity,
            total: 0,
        }
    }

    /// The configured bound in bytes.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Bytes currently retained.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing is retained.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Bytes ever appended (retained or discarded).
    pub fn total_written(&self) -> u64 {
        self.total
    }

    /// Append `bytes`, discarding the oldest data past the bound.
    pub fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        if bytes.len() >= self.capacity {
            self.buf.clear();
            let tail = &bytes[bytes.len() - self.capacity..];
            self.buf.extend(tail);
        } else {
            let overflow = (self.buf.len() + bytes.len()).saturating_sub(self.capacity);
            self.buf.drain(..overflow);
            self.buf.extend(bytes);
        }
        self.trim_partial_char();
    }

    /// The retained bytes, oldest first.
    pub fn snapshot(&self) -> Vec<u8> {
        let (a, b) = self.buf.as_slices();
        let mut out = Vec::with_capacity(a.len() + b.len());
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        out
    }

    /// Drop leading UTF-8 continuation bytes (at most three: the longest
    /// possible tail of a cut character) so the ring starts on a boundary.
    fn trim_partial_char(&mut self) {
        let mut dropped = 0;
        while dropped < 3 && matches!(self.buf.front(), Some(b) if (b & 0xC0) == 0x80) {
            self.buf.pop_front();
            dropped += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_everything_under_the_bound() {
        let mut ring = Scrollback::new(16);
        ring.push(b"hello ");
        ring.push(b"world");
        assert_eq!(ring.snapshot(), b"hello world");
        assert_eq!(ring.len(), 11);
        assert_eq!(ring.total_written(), 11);
        assert!(!ring.is_empty());
    }

    #[test]
    fn discards_oldest_bytes_past_the_bound() {
        let mut ring = Scrollback::new(8);
        ring.push(b"abcdef");
        ring.push(b"ghij");
        assert_eq!(ring.snapshot(), b"cdefghij");
        assert_eq!(ring.len(), 8);
        assert_eq!(ring.total_written(), 10);
    }

    #[test]
    fn a_chunk_larger_than_the_bound_keeps_its_tail() {
        let mut ring = Scrollback::new(4);
        ring.push(b"xy");
        ring.push(b"0123456789");
        assert_eq!(ring.snapshot(), b"6789");
    }

    #[test]
    fn never_starts_mid_character() {
        // "é" is 0xC3 0xA9; cut so the ring would start on 0xA9.
        let mut ring = Scrollback::new(4);
        ring.push("aé".as_bytes()); // 61 C3 A9
        ring.push(b"bc"); // 61 C3 A9 62 63 -> keep last 4 = C3 A9 62 63
        assert_eq!(ring.snapshot(), "ébc".as_bytes());
        ring.push(b"d"); // A9 62 63 64 -> continuation trimmed
        assert_eq!(ring.snapshot(), b"bcd");
        assert!(ring.len() <= ring.capacity());
    }

    #[test]
    fn zero_capacity_is_clamped() {
        let mut ring = Scrollback::new(0);
        ring.push(b"abc");
        assert_eq!(ring.capacity(), 1);
        assert_eq!(ring.snapshot(), b"c");
    }
}
