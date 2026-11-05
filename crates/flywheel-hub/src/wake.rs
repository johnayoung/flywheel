//! A wakeup for a thread blocked in `poll(2)`: a socket pair whose read
//! end joins the poll set and becomes readable once signalled. Threads
//! that would otherwise poll with a timeout just to notice a flag (stop,
//! pause, release) block without one; whoever changes the flag sets it,
//! then signals.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

#[derive(Debug)]
pub(crate) struct Waker {
    rx: UnixStream,
    tx: UnixStream,
}

impl Waker {
    pub(crate) fn new() -> std::io::Result<Self> {
        let (rx, tx) = UnixStream::pair()?;
        rx.set_nonblocking(true)?;
        tx.set_nonblocking(true)?;
        Ok(Self { rx, tx })
    }

    /// The descriptor to poll for `POLLIN`.
    pub(crate) fn fd(&self) -> RawFd {
        self.rx.as_raw_fd()
    }

    /// Make the read end readable. A full buffer (`WouldBlock`) already is.
    pub(crate) fn signal(&self) {
        let _ = (&self.tx).write(&[1]);
    }

    /// Consume every pending signal, so the next `poll` blocks again.
    pub(crate) fn drain(&self) {
        let mut buf = [0u8; 64];
        while matches!((&self.rx).read(&mut buf), Ok(n) if n > 0) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readable(waker: &Waker) -> bool {
        let mut pfd = libc::pollfd {
            fd: waker.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        unsafe { libc::poll(&raw mut pfd, 1, 0) > 0 }
    }

    #[test]
    fn a_signal_stays_readable_until_drained() {
        let waker = Waker::new().unwrap();
        assert!(!readable(&waker));
        // Many signals before a drain: one wakeup, never a full buffer
        // that blocks the signaller.
        for _ in 0..100_000 {
            waker.signal();
        }
        assert!(readable(&waker));
        assert!(readable(&waker), "polling does not consume it");
        waker.drain();
        assert!(!readable(&waker));
        waker.signal();
        assert!(readable(&waker));
    }
}
