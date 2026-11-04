//! Deflated binary messages for terminal viewers that ask (`z=1`): one raw
//! deflate stream per socket, flushed (`Z_SYNC_FLUSH`) at the end of every
//! message, so each message inflates fully on arrival while later ones
//! reuse the earlier ones' dictionary (a redrawn status line costs a few
//! bytes). Browsers inflate with one `DecompressionStream("deflate-raw")`
//! per socket.
//!
//! Each message starts with a tag byte: `0` = the payload follows as is
//! (too short to gain), `1` = a big-endian u32 of the inflated length,
//! then the deflated bytes.

use flate2::{Compress, Compression, FlushCompress};

/// Payloads shorter than this go as they are.
const MIN_DEFLATE: usize = 48;

pub(super) struct Deflater {
    z: Compress,
}

impl Deflater {
    pub(super) fn new() -> Self {
        Self {
            // Fast: terminal output compresses well at any level, and a
            // busy session's every byte goes through here.
            z: Compress::new(Compression::fast(), false),
        }
    }

    /// One message's payload, tagged.
    pub(super) fn message(&mut self, data: &[u8]) -> Vec<u8> {
        let Ok(len) = u32::try_from(data.len()) else {
            return plain(data);
        };
        if data.len() < MIN_DEFLATE {
            return plain(data);
        }
        let mut out = Vec::with_capacity(data.len() / 3 + 64);
        out.push(1);
        out.extend_from_slice(&len.to_be_bytes());
        let mut input = data;
        loop {
            if out.capacity() - out.len() < 1024 {
                out.reserve(input.len() / 2 + 4096);
            }
            let before = self.z.total_in();
            if self
                .z
                .compress_vec(input, &mut out, FlushCompress::Sync)
                .is_err()
            {
                // Not reachable with valid parameters. A fresh stream is a
                // valid continuation for the viewer's inflater (the last
                // message ended on a block boundary; a fresh one only
                // refers back into itself); this message goes plain.
                self.z = Compress::new(Compression::fast(), false);
                return plain(data);
            }
            let used = usize::try_from(self.z.total_in() - before).unwrap_or(input.len());
            input = &input[used..];
            // Done once all input is in and the flush did not fill the
            // buffer (a full buffer may hold back more output).
            if input.is_empty() && out.len() < out.capacity() {
                break;
            }
        }
        out
    }
}

fn plain(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 1);
    out.push(0);
    out.extend_from_slice(data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Decompress, FlushDecompress};

    /// What a browser's single inflater makes of the messages, in order.
    fn inflate_all(messages: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut z = Decompress::new(false);
        messages
            .iter()
            .map(|m| match m[0] {
                0 => m[1..].to_vec(),
                1 => {
                    let len = u32::from_be_bytes(m[1..5].try_into().unwrap()) as usize;
                    let mut out = Vec::with_capacity(len + 16);
                    z.decompress_vec(&m[5..], &mut out, FlushDecompress::Sync)
                        .unwrap();
                    assert_eq!(out.len(), len, "the whole message inflates on arrival");
                    out
                }
                tag => panic!("tag {tag}"),
            })
            .collect()
    }

    #[test]
    fn messages_inflate_whole_and_in_order_sharing_one_stream() {
        let mut d = Deflater::new();
        let status = |i: usize| {
            format!(
                "\r\x1b[2K\x1b[38;5;208m*\x1b[0m Working... ({i}s, esc to interrupt) tokens: 1234"
            )
            .into_bytes()
        };
        let mut sent = Vec::new();
        let mut inputs = Vec::new();
        for i in 0..50 {
            inputs.push(status(i));
            inputs.push(b"x".to_vec());
        }
        // Bigger than any one output buffer step.
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        inputs.push(big);
        for input in &inputs {
            sent.push(d.message(input));
        }
        assert_eq!(inflate_all(&sent), inputs);
        // A repeated status line costs a fraction of itself.
        let later = &sent[40];
        assert!(
            later.len() * 2 < inputs[40].len(),
            "{} vs {}",
            later.len(),
            inputs[40].len()
        );
        assert_eq!(sent[1], vec![0, b'x'], "short ones go plain");
    }
}
