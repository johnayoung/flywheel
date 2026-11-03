//! The real terminal under `fw hub attach`: raw mode, its restoration on
//! every way out (return, error, panic, signal), the size, and a stdin
//! reader that can be stopped (so the picker can read keys itself).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Once};
use std::thread::JoinHandle;
use std::time::Duration;

/// Raw mode is on (restoring it is due).
static RAW: AtomicBool = AtomicBool::new(false);
/// The picker's alternate screen is up.
static ALT: AtomicBool = AtomicBool::new(false);
static HOOK: Once = Once::new();

/// What a session may have left the terminal in, undone: synchronized
/// update, scroll region, pen, charsets, hyperlink, mouse and focus
/// reporting, bracketed paste, cursor keys and keypad, insert / newline /
/// origin modes, autowrap, cursor visibility and style, palette and
/// default colours, the alternate screen.
pub const RESET: &[u8] = b"\x1b[?2026l\x1b[r\x1b[0m\x1b(B\x0f\x1b]8;;\x1b\\\
\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\
\x1b[?1l\x1b>\x1b[4l\x1b[20l\x1b[?6l\x1b[?7h\x1b[?25h\x1b[0 q\
\x1b]104\x07\x1b]110\x07\x1b]111\x07\x1b]112\x07\x1b[?1049l";

/// Raw mode for as long as it lives; dropping it restores the terminal.
#[derive(Debug)]
pub struct Tty {
    _private: (),
}

impl Tty {
    pub fn enter() -> std::io::Result<Self> {
        HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore();
                previous(info);
            }));
        });
        crossterm::terminal::enable_raw_mode()?;
        RAW.store(true, Ordering::SeqCst);
        Ok(Self { _private: () })
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        restore();
    }
}

/// Put the terminal back as it was: leave the picker's screen, undo what
/// the session set ([`RESET`]), cooked mode. Idempotent.
pub fn restore() {
    let mut out = std::io::stdout();
    if ALT.swap(false, Ordering::SeqCst) {
        let _ = out.write_all(b"\x1b[?1049l\x1b[?25h");
    }
    if RAW.swap(false, Ordering::SeqCst) {
        let _ = out.write_all(RESET);
        let _ = out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
    let _ = out.flush();
}

/// Enter / leave the picker's alternate screen.
pub fn alternate_screen(on: bool) {
    let mut out = std::io::stdout();
    if on {
        if !ALT.swap(true, Ordering::SeqCst) {
            let _ = out.write_all(b"\x1b[?1049h\x1b[H\x1b[2J");
        }
    } else if ALT.swap(false, Ordering::SeqCst) {
        let _ = out.write_all(b"\x1b[?25h\x1b[?1049l");
    }
    let _ = out.flush();
}

/// `(cols, rows)` of the terminal (80x24 when it cannot say).
pub fn size() -> (u16, u16) {
    match crossterm::terminal::size() {
        Ok((cols, rows)) if cols > 0 && rows > 0 => (cols, rows),
        _ => (80, 24),
    }
}

/// Write `bytes` to the terminal.
pub fn write(bytes: &[u8]) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()
}

/// Reads stdin on its own thread into `on_input` until stopped (dropped).
pub struct StdinReader {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// What the stdin reader sends.
pub enum Input {
    Bytes(Vec<u8>),
    /// End of input or a read error.
    Closed(String),
}

impl StdinReader {
    pub fn start<T: Send + 'static>(tx: Sender<T>, wrap: fn(Input) -> T) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("fw-hub-attach-stdin".into())
            .spawn(move || read_loop(&flag, &tx, wrap))
            .ok();
        Self { stop, thread }
    }
}

impl Drop for StdinReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Poll stdin (100 ms at a time, so a stop is noticed) and forward reads.
/// Never reads once stopped: what is typed next belongs to whoever reads
/// stdin then (the picker).
fn read_loop<T>(stop: &AtomicBool, tx: &Sender<T>, wrap: fn(Input) -> T) {
    let mut buf = vec![0u8; 64 * 1024];
    let poll_ms = i32::try_from(Duration::from_millis(100).as_millis()).unwrap_or(100);
    while !stop.load(Ordering::SeqCst) {
        let mut fds = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut fds, 1, poll_ms) };
        if ready < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            let _ = tx.send(wrap(Input::Closed(format!("stdin: {e}"))));
            return;
        }
        if ready == 0 || stop.load(Ordering::SeqCst) {
            continue;
        }
        // SAFETY: reads into an owned buffer of the given length.
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if matches!(
                e.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
            ) {
                continue;
            }
            let _ = tx.send(wrap(Input::Closed(format!("stdin: {e}"))));
            return;
        }
        if n == 0 {
            let _ = tx.send(wrap(Input::Closed("end of input".into())));
            return;
        }
        let n = usize::try_from(n).unwrap_or(0);
        if tx.send(wrap(Input::Bytes(buf[..n].to_vec()))).is_err() {
            return;
        }
    }
}
