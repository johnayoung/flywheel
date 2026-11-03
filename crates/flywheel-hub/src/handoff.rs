//! Live handoff: replace a running hub process with another (a new
//! binary) without ending any session.
//!
//! The running hub listens on `<state-dir>/handoff.sock` (a unix socket,
//! 0600, in the 0700 state dir; peers must be this user). A new hub
//! (`fw hub serve --handoff`, which `fw hub upgrade` and `fw hub start
//! --handoff` run) connects and the two speak a versioned protocol of
//! length-prefixed frames -- JSON, then binary -- with descriptors passed
//! as `SCM_RIGHTS`:
//!
//! 1. new -> old `hello` (protocol version). The old hub refuses an
//!    incompatible one (`refused`) and changes nothing.
//! 2. The old hub **freezes**: it stops accepting connections (the
//!    listening socket stays open: connections wait in its backlog) and
//!    answers requests still arriving on open ones with 503, waits for the
//!    ones in flight, pauses agent reports, saves where each transcript
//!    was read to, and pauses every session (nothing is read from or
//!    written to a PTY; typed input is held, in order).
//! 3. old -> new `offer` + the listening TCP socket, the serve lock, and
//!    the report socket; then one `session` frame per live session (its
//!    record id, size, emulator snapshot with all its scrollback, held
//!    input) + its PTY master and pidfd.
//! 4. The new hub builds itself from the state file and these, every
//!    session paused, and answers `ready` (or `failed`).
//! 5. old -> new `commit`; the new hub says `resuming`, resumes the
//!    sessions, serves, and answers `started`; the old process exits
//!    without touching any session (or the socket files, now the new
//!    hub's).
//!
//! Any failure before `commit` (a refusal, an error, a timeout, the new
//! hub dying) thaws the old hub: it carries on serving as if nothing
//! happened. After `commit` the old hub exits once the new one has
//! started -- or rolls back when the new one died before starting
//! (dropping the held input when it had said `resuming`: it may have
//! been typed already, and is never typed twice). The new hub, finding
//! the old one gone after `ready`, takes over (the descriptors it holds
//! are then the only ones).
//!
//! From the freeze on, the old hub closes its WebSockets with 1012
//! (service restart; clients reconnect) and refuses input it could no
//! longer hand over ([`crate::pty::WriteOutcome::HandingOff`]): nothing
//! typed is silently lost.
//!
//! Children of the old hub are not the new one's: their exit is seen
//! through the pidfd (Linux), else by polling the pid, and recorded with an
//! unknown code. A hub running as a systemd service refuses (its sessions
//! live in the unit's cgroup and would die with the old process).

use crate::hub::Hub;
use crate::hub::handoff::{Frozen, Incoming, IncomingSession};
pub(crate) use crate::pty::handoff::process_alive;
use crate::pty::handoff::{Adopt, PtyExport, PtyMeta};
use crate::status::handoff::BoardExport;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The handoff protocol version. Hubs hand off only to a hub speaking the
/// same one; bump it on any incompatible change to the frames below or to
/// what they carry (snapshot format, state file).
pub const PROTOCOL: u32 = 1;
/// The socket's name in the state dir.
pub const SOCKET_FILE: &str = "handoff.sock";
/// `fw hub serve --handoff` exit code when the running hub refused the
/// handoff as incompatible (version skew) -- nothing changed; a restart
/// (`--allow-restart`) is the way forward.
pub const EXIT_REFUSED: i32 = 4;
/// `fw hub serve --handoff` exit code when the running hub refused the
/// handoff for now (another handoff under way, requests or sessions that
/// would not settle, a systemd service) -- nothing changed; never a
/// reason to restart instead.
pub const EXIT_BUSY: i32 = 5;
/// `fw hub serve --handoff` exit code when no hub was there to hand off.
pub const EXIT_NO_HUB: i32 = 3;

/// Largest JSON part of a frame.
const MAX_JSON: usize = 64 * 1024 * 1024;
/// Largest binary part of a frame (one session's snapshot and input).
const MAX_BIN: usize = 512 * 1024 * 1024;
/// Most descriptors one frame carries.
const MAX_FDS: usize = 4;
/// How long the old hub may take to freeze.
pub(crate) const FREEZE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long either side waits for the other's next step (the new hub may
/// ask for less: `fw hub upgrade --timeout`).
const STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// The least step time a new hub may ask for.
const MIN_STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a client may take to say what it wants.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// The handoff socket of the hub serving `state_dir`.
pub fn socket_path(state_dir: &Path) -> std::io::Result<PathBuf> {
    crate::report::private_socket_path(state_dir, SOCKET_FILE, "-handoff")
}

/// One protocol message (the JSON part of a frame).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Msg {
    /// `fw hub upgrade` asks how the hub runs (any protocol version).
    Describe,
    Description(Description),
    Hello {
        protocol: u32,
        version: String,
        pid: u32,
        /// How long the old hub may stay frozen waiting for each of this
        /// hub's steps, in seconds (capped at [`STEP_TIMEOUT`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step_secs: Option<u64>,
    },
    Refused {
        reason: String,
        #[serde(default)]
        kind: Refusal,
    },
    /// Descriptors: the listening TCP socket, the serve lock, and (when
    /// `report_path`) the report socket.
    Offer {
        version: String,
        pid: u32,
        listen: SocketAddr,
        report_path: Option<PathBuf>,
        board: BoardExport,
        sessions: usize,
    },
    /// Binary: the snapshot (`snapshot_len` bytes), then held input.
    /// Descriptors: the PTY master, then (when `pidfd`) the leader's pidfd.
    Session {
        id: String,
        pty: PtyMeta,
        snapshot_len: usize,
        pidfd: bool,
    },
    Ready,
    Failed {
        reason: String,
    },
    Commit,
    /// The new hub is about to resume the sessions (from here it may have
    /// written their held input).
    Resuming,
    Abort {
        reason: String,
    },
    Started {
        pid: u32,
    },
}

/// How a running hub runs: what `fw hub upgrade` starts its successor
/// with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Description {
    pub protocol: u32,
    /// The hub's flywheel version.
    pub version: String,
    pub pid: u32,
    /// Its `serve` options (absolute state dir included).
    pub serve_args: Vec<String>,
    pub log_file: Option<PathBuf>,
    /// The program it runs (`fw hub status`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<PathBuf>,
    /// Why it cannot hand off at all (a systemd service), if so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported: Option<String>,
}

/// Why a running hub refused a handoff.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// The two hubs speak different protocol versions: only a restart
    /// gets the new binary in.
    Protocol,
    /// Not now: another handoff is under way, or requests or sessions
    /// would not settle (retry).
    #[default]
    Busy,
    /// Never by handoff: a systemd service (restart the unit).
    Unsupported,
}

/// Handoff failures.
#[derive(Debug, thiserror::Error)]
pub enum HandoffError {
    /// No hub is listening for a handoff (none runs, or it predates live
    /// handoff).
    #[error("{0}")]
    NoHub(String),
    /// The running hub refused; it carries on unchanged.
    #[error("the running hub refused the handoff: {reason}")]
    Refused { reason: String, kind: Refusal },
    #[error("{0}")]
    Failed(String),
}

fn io_failed(context: &str) -> impl Fn(std::io::Error) -> HandoffError + '_ {
    move |e| HandoffError::Failed(format!("{context}: {e}"))
}

// --- frames -----------------------------------------------------------------

/// Send one frame: an 8-byte header (JSON and binary lengths, big endian)
/// carrying `fds`, then the JSON, then the binary.
fn send_frame(sock: &UnixStream, msg: &Msg, bin: &[u8], fds: &[RawFd]) -> std::io::Result<()> {
    let json = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    let mut header = [0u8; 8];
    header[..4].copy_from_slice(
        &u32::try_from(json.len())
            .map_err(std::io::Error::other)?
            .to_be_bytes(),
    );
    header[4..].copy_from_slice(
        &u32::try_from(bin.len())
            .map_err(std::io::Error::other)?
            .to_be_bytes(),
    );
    let sent = send_with_fds(sock, &header, fds)?;
    let mut writer = sock;
    writer.write_all(&header[sent..])?;
    writer.write_all(&json)?;
    writer.write_all(bin)?;
    writer.flush()
}

/// `sendmsg` of `data` with `fds` attached (to its first byte); returns
/// how much of `data` went.
fn send_with_fds(sock: &UnixStream, data: &[u8], fds: &[RawFd]) -> std::io::Result<usize> {
    assert!(fds.len() <= MAX_FDS && !data.is_empty());
    let mut iov = libc::iovec {
        iov_base: data.as_ptr().cast_mut().cast(),
        iov_len: data.len(),
    };
    let fd_bytes = std::mem::size_of_val(fds);
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(u32::try_from(fd_bytes).unwrap_or(0)) } as usize;
    let mut control = vec![0u8; space.max(1)];
    // SAFETY: an all-zero msghdr is valid; the fields set below point at
    // live buffers for the duration of the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: `msg` has a control buffer of CMSG_SPACE(fd_bytes) bytes,
        // so the first header and its data fit.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(u32::try_from(fd_bytes).unwrap_or(0)) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(cmsg),
                fd_bytes,
            );
        }
    }
    loop {
        // SAFETY: `msg` and everything it points at are valid for the call.
        let sent = unsafe { libc::sendmsg(sock.as_raw_fd(), &raw const msg, 0) };
        if sent >= 0 {
            return Ok(usize::try_from(sent).unwrap_or(0));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Receive one frame (see [`send_frame`]).
fn recv_frame(sock: &UnixStream) -> std::io::Result<(Msg, Vec<u8>, Vec<OwnedFd>)> {
    let mut header = [0u8; 8];
    let (got, fds) = recv_with_fds(sock, &mut header)?;
    if got == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the other hub closed the connection",
        ));
    }
    let mut reader = sock;
    reader.read_exact(&mut header[got..])?;
    let json_len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let bin_len = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if json_len > MAX_JSON || bin_len > MAX_BIN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "oversized handoff frame",
        ));
    }
    let mut json = vec![0u8; json_len];
    reader.read_exact(&mut json)?;
    let mut bin = vec![0u8; bin_len];
    reader.read_exact(&mut bin)?;
    let msg = serde_json::from_slice(&json).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unexpected handoff message (another protocol version?): {e}"),
        )
    })?;
    Ok((msg, bin, fds))
}

/// `recvmsg` into `buf`, collecting passed descriptors (close-on-exec).
fn recv_with_fds(sock: &UnixStream, buf: &mut [u8]) -> std::io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let fd_bytes = MAX_FDS * std::mem::size_of::<RawFd>();
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(u32::try_from(fd_bytes).unwrap_or(0)) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: an all-zero msghdr is valid; the fields set below point at
    // live buffers for the duration of the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    let got = loop {
        // SAFETY: `msg` and everything it points at are valid for the call.
        let got = unsafe { libc::recvmsg(sock.as_raw_fd(), &raw mut msg, flags) };
        if got >= 0 {
            break usize::try_from(got).unwrap_or(0);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    let mut fds = Vec::new();
    // SAFETY: walking the control buffer the kernel filled, with the
    // CMSG_* accessors, within `msg_controllen`.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg);
                let len = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for i in 0..len / std::mem::size_of::<RawFd>() {
                    let fd = std::ptr::read_unaligned(data.cast::<RawFd>().add(i));
                    // The kernel installed it in this process for us.
                    let fd = OwnedFd::from_raw_fd(fd);
                    #[cfg(not(target_os = "linux"))]
                    libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
                    fds.push(fd);
                }
            }
            cmsg = libc::CMSG_NXTHDR(&raw const msg, cmsg);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "handoff descriptors were truncated",
        ));
    }
    Ok((got, fds))
}

/// The uid of the process at the other end of `sock`.
fn peer_uid(sock: &UnixStream) -> std::io::Result<u32> {
    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>()).unwrap_or(0);
        // SAFETY: SO_PEERCRED writes one `ucred` into the buffer given.
        let rc = unsafe {
            libc::getsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast(),
                &raw mut len,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(cred.uid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: getpeereid writes the two ids.
        if unsafe { libc::getpeereid(sock.as_raw_fd(), &raw mut uid, &raw mut gid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(uid)
    }
}

fn same_user(sock: &UnixStream) -> std::io::Result<()> {
    // SAFETY: getuid cannot fail.
    let me = unsafe { libc::getuid() };
    let peer = peer_uid(sock)?;
    if peer != me {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("handoff peer runs as uid {peer}, not {me}"),
        ));
    }
    Ok(())
}

fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

// --- the gate ---------------------------------------------------------------

/// The HTTP side of a freeze: whether the server accepts connections and
/// requests, and how many requests are in flight.
#[derive(Debug, Clone)]
pub struct Gate {
    inner: Arc<GateInner>,
}

#[derive(Debug)]
struct GateInner {
    open: tokio::sync::watch::Sender<bool>,
    in_flight: AtomicUsize,
    /// A handoff is under way (a shutdown waits for it) or done.
    handing_off: tokio::sync::watch::Sender<bool>,
    /// The hub is frozen for a handoff: open WebSockets close with
    /// [`crate::server::WS_CLOSE_RESTART`] so clients reconnect (to the
    /// new process, or to this one after a rollback).
    restarting: tokio::sync::watch::Sender<bool>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(GateInner {
                open: tokio::sync::watch::Sender::new(true),
                in_flight: AtomicUsize::new(0),
                handing_off: tokio::sync::watch::Sender::new(false),
                restarting: tokio::sync::watch::Sender::new(false),
            }),
        }
    }

    pub fn is_open(&self) -> bool {
        *self.inner.open.borrow()
    }

    fn set_open(&self, open: bool) {
        self.inner.open.send_replace(open);
    }

    fn set_handing_off(&self, on: bool) {
        self.inner.handing_off.send_replace(on);
    }

    fn set_restarting(&self, on: bool) {
        self.inner.restarting.send_replace(on);
    }

    /// Resolves once a handoff has frozen the hub (a WebSocket then closes
    /// with [`crate::server::WS_CLOSE_RESTART`]).
    pub async fn restarting(&self) {
        let mut rx = self.inner.restarting.subscribe();
        let _ = rx.wait_for(|on| *on).await;
    }

    /// Resolves once a handoff has closed the gate (at once while it is
    /// closed): the accept loop drops connections no request has
    /// authenticated, so the freeze never waits on them.
    pub async fn frozen(&self) {
        let mut rx = self.inner.open.subscribe();
        gate_becomes(&mut rx, false).await;
    }

    /// Resolves once no handoff is under way.
    pub async fn handoff_settled(&self) {
        let mut rx = self.inner.handing_off.subscribe();
        let _ = rx.wait_for(|on| !*on).await;
    }

    /// Wait (blocking) until no request is in flight; false after
    /// `timeout`.
    fn drain(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.inner.in_flight.load(Ordering::Acquire) > 0 {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }
}

/// Counts a request in flight while alive.
struct InFlight(Arc<GateInner>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Middleware: while a handoff has the hub frozen, refuse requests (503,
/// retry in a second: the new hub answers then); count the others, and
/// hand them the [`Gate`] (an extension: WebSockets watch it).
pub async fn gate_requests(
    axum::extract::State(gate): axum::extract::State<Gate>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    gate.inner.in_flight.fetch_add(1, Ordering::AcqRel);
    let guard = InFlight(Arc::clone(&gate.inner));
    if !gate.is_open() {
        drop(guard);
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            [(axum::http::header::RETRY_AFTER, "1")],
            "the hub is handing over to a new process; retry\n",
        )
            .into_response();
    }
    request.extensions_mut().insert(gate.clone());
    let response = next.run(request).await;
    drop(guard);
    response
}

/// Resolves once the gate is `open` (at once when it is).
async fn gate_becomes(rx: &mut tokio::sync::watch::Receiver<bool>, open: bool) {
    loop {
        let now = *rx.borrow_and_update();
        if now == open {
            return;
        }
        if rx.changed().await.is_err() {
            // The gate outlives every listener; never reached.
            std::future::pending::<()>().await;
        }
    }
}

/// A TCP listener that accepts only while its [`Gate`] is open (a frozen
/// hub leaves connections in the backlog for its successor).
#[derive(Debug)]
pub struct GatedListener {
    inner: tokio::net::TcpListener,
    gate: Gate,
}

impl GatedListener {
    pub fn new(inner: tokio::net::TcpListener, gate: Gate) -> Self {
        Self { inner, gate }
    }

    pub fn gate(&self) -> &Gate {
        &self.gate
    }
}

impl axum::serve::Listener for GatedListener {
    type Io = tokio::net::TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let mut open = self.gate.inner.open.subscribe();
            gate_becomes(&mut open, true).await;
            tokio::select! {
                accepted = self.inner.accept() => match accepted {
                    Ok(pair) => return pair,
                    Err(e) => {
                        if !matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionRefused
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::ConnectionReset
                        ) {
                            eprintln!("fw hub: accept error: {e}");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                },
                () = gate_becomes(&mut open, false) => {}
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

// --- the old hub: handing off -----------------------------------------------

/// What the running hub needs to hand itself off.
pub struct Source {
    pub hub: Arc<Hub>,
    pub gate: Gate,
    /// The listening TCP socket's descriptor (open for the hub's life).
    pub listener_fd: RawFd,
    pub listen: SocketAddr,
    pub serve_args: Vec<String>,
    pub log_file: Option<PathBuf>,
}

/// The running hub's handoff socket: a thread answering `describe` and
/// handing off to a new hub that says `hello`.
#[derive(Debug)]
pub struct HandoffServer {
    path: PathBuf,
}

impl HandoffServer {
    /// Bind `<state-dir>/handoff.sock` (replacing a stale one: the caller
    /// holds the serve lock) and serve it on a thread. On a successful
    /// handoff the process exits from that thread.
    pub fn start(source: Source) -> std::io::Result<Self> {
        let path = socket_path(source.hub.state_dir())?;
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&path)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        let source = Arc::new(source);
        let busy = Arc::new(Mutex::new(()));
        std::thread::Builder::new()
            .name("hub-handoff".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    };
                    let source = Arc::clone(&source);
                    let busy = Arc::clone(&busy);
                    let _ = std::thread::Builder::new()
                        .name("hub-handoff-conn".into())
                        .spawn(move || serve_peer(&source, &busy, &stream));
                }
            })?;
        Ok(Self { path })
    }

    /// Remove the socket file (a clean shutdown; never after handing off).
    pub fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Why this hub cannot hand off, when it is a systemd unit's main process
/// (`fw hub install-service`): its sessions live in the unit's cgroup, and
/// systemd ends them when the old process exits.
fn systemd_service(detached: bool) -> Option<String> {
    if detached || std::env::var_os("INVOCATION_ID").is_none() {
        return None;
    }
    let parent = std::os::unix::process::parent_id();
    let comm = std::fs::read_to_string(format!("/proc/{parent}/comm")).unwrap_or_default();
    (comm.trim() == "systemd").then(|| {
        "this hub runs as a systemd service: its sessions live in the unit's cgroup and \
         would end with the old process; restart it with `systemctl --user restart`"
            .to_owned()
    })
}

/// The systemd unit process `pid` is the main process of, when it is one
/// (Linux: `INVOCATION_ID` in its environment, systemd its parent, the
/// unit from its cgroup) -- for a hub that cannot say so itself.
pub fn systemd_unit(pid: u32) -> Option<String> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    if !environ
        .split(|&b| b == 0)
        .any(|entry| entry.starts_with(b"INVOCATION_ID="))
    {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid ...`: comm may hold spaces and parentheses.
    let ppid = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?;
    let comm = std::fs::read_to_string(format!("/proc/{ppid}/comm")).ok()?;
    if comm.trim() != "systemd" {
        return None;
    }
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    cgroup
        .lines()
        .filter_map(|line| line.rsplit('/').next())
        .find(|unit| unit.ends_with(".service"))
        .map(str::to_owned)
}

fn serve_peer(source: &Source, busy: &Mutex<()>, stream: &UnixStream) {
    if let Err(e) = same_user(stream) {
        eprintln!("fw hub: handoff connection refused: {e}");
        return;
    }
    let _ = stream.set_read_timeout(Some(FIRST_FRAME_TIMEOUT));
    let _ = stream.set_write_timeout(Some(STEP_TIMEOUT));
    let Ok((msg, _, _)) = recv_frame(stream) else {
        return;
    };
    match msg {
        Msg::Describe => {
            let description = Description {
                protocol: PROTOCOL,
                version: version(),
                pid: std::process::id(),
                serve_args: source.serve_args.clone(),
                log_file: source.log_file.clone(),
                binary: std::env::current_exe().ok(),
                unsupported: systemd_service(source.log_file.is_some()),
            };
            let _ = send_frame(stream, &Msg::Description(description), &[], &[]);
        }
        Msg::Hello {
            protocol,
            version: theirs,
            pid,
            step_secs,
        } => {
            let refuse = |reason: String, kind: Refusal| {
                eprintln!("fw hub: refused a handoff to pid {pid}: {reason}");
                let _ = send_frame(stream, &Msg::Refused { reason, kind }, &[], &[]);
            };
            if protocol != PROTOCOL {
                return refuse(
                    format!(
                        "this hub (pid {}, flywheel {}) speaks handoff protocol {PROTOCOL}; the new one \
                     (flywheel {theirs}) speaks {protocol}",
                        std::process::id(),
                        version()
                    ),
                    Refusal::Protocol,
                );
            }
            if let Some(reason) = systemd_service(source.log_file.is_some()) {
                return refuse(reason, Refusal::Unsupported);
            }
            let Ok(_one) = busy.try_lock() else {
                return refuse("another handoff is under way".into(), Refusal::Busy);
            };
            let step = step_secs.map_or(STEP_TIMEOUT, |secs| {
                Duration::from_secs(secs).clamp(MIN_STEP_TIMEOUT, STEP_TIMEOUT)
            });
            hand_off(source, stream, pid, step);
        }
        _ => {}
    }
}

/// Run one handoff to the hub at the other end of `stream` (pid `peer`).
/// Returns only when it failed and this hub carries on; on success the
/// process exits.
fn hand_off(source: &Source, stream: &UnixStream, peer: u32, step: Duration) {
    eprintln!("fw hub: handing off to pid {peer}");
    source.gate.set_handing_off(true);
    source.gate.set_open(false);
    // `resumed`: the new hub may have written the held input already.
    let rollback = |reason: &str, frozen: bool, resumed: bool| {
        if frozen {
            source.hub.handoff_thaw(resumed);
        }
        source.gate.set_restarting(false);
        source.gate.set_open(true);
        source.gate.set_handing_off(false);
        eprintln!("fw hub: handoff to pid {peer} failed, still serving: {reason}");
    };
    if !source.gate.drain(FREEZE_TIMEOUT) {
        let reason = "requests were still running";
        let _ = send_frame(
            stream,
            &Msg::Refused {
                reason: reason.into(),
                kind: Refusal::Busy,
            },
            &[],
            &[],
        );
        return rollback(reason, false, false);
    }
    // Clients reconnect (and wait in the backlog) instead of typing into
    // a hub that is about to go.
    source.gate.set_restarting(true);
    let frozen = match source.hub.handoff_freeze(FREEZE_TIMEOUT) {
        Ok(frozen) => frozen,
        Err(reason) => {
            let _ = send_frame(
                stream,
                &Msg::Refused {
                    reason: reason.clone(),
                    kind: Refusal::Busy,
                },
                &[],
                &[],
            );
            // The freeze thawed what it had frozen.
            return rollback(&reason, false, false);
        }
    };
    if let Err(e) = send_offer(source, stream, &frozen) {
        return rollback(&format!("sending the sessions: {e}"), true, false);
    }
    let _ = stream.set_read_timeout(Some(step));
    match recv_frame(stream) {
        Ok((Msg::Ready, _, _)) => {}
        Ok((Msg::Failed { reason }, _, _)) => {
            return rollback(&format!("the new hub failed: {reason}"), true, false);
        }
        Ok((other, _, _)) => {
            let reason = format!("unexpected answer {other:?}");
            let _ = send_frame(
                stream,
                &Msg::Abort {
                    reason: reason.clone(),
                },
                &[],
                &[],
            );
            return rollback(&reason, true, false);
        }
        Err(e) => {
            let reason = format!("no answer from the new hub within {}s: {e}", step.as_secs());
            let _ = send_frame(
                stream,
                &Msg::Abort {
                    reason: reason.clone(),
                },
                &[],
                &[],
            );
            return rollback(&reason, true, false);
        }
    }
    if let Err(e) = send_frame(stream, &Msg::Commit, &[], &[]) {
        return rollback(&format!("the new hub went away: {e}"), true, false);
    }
    // From here the new hub may be reading the sessions. It closes the
    // link without `started` only when it never committed (it gave up
    // waiting) or died: resume then -- without the held input once it said
    // `resuming`. A new hub still silent at the timeout may be mid-commit:
    // never resume while it lives.
    let mut resumed = false;
    let started = loop {
        match recv_frame(stream) {
            Ok((Msg::Resuming, _, _)) => resumed = true,
            Ok((Msg::Started { .. }, _, _)) => break true,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break process_alive(peer);
            }
            _ => break false,
        }
    };
    if !started {
        return rollback("the new hub died while starting", true, resumed);
    }
    source.hub.handoff_release();
    eprintln!(
        "fw hub: handed off {} session{} to pid {peer}; exiting",
        frozen.sessions.len(),
        if frozen.sessions.len() == 1 { "" } else { "s" }
    );
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // Let a detached hub's log copier write the last lines out.
    std::thread::sleep(Duration::from_millis(200));
    // No destructors: nothing here may hang up a session or remove a
    // socket file the new hub now owns.
    std::process::exit(0);
}

fn send_offer(source: &Source, stream: &UnixStream, frozen: &Frozen) -> std::io::Result<()> {
    let fds = source.hub.handoff_fds();
    let report_path = fds.report.map(|_| source.hub.report_socket());
    let mut passed = vec![source.listener_fd, fds.serve_lock];
    passed.extend(fds.report);
    send_frame(
        stream,
        &Msg::Offer {
            version: version(),
            pid: std::process::id(),
            listen: source.listen,
            report_path,
            board: frozen.board.clone(),
            sessions: frozen.sessions.len(),
        },
        &[],
        &passed,
    )?;
    for frozen in &frozen.sessions {
        let Some(master) = frozen.session.master_fd() else {
            return Err(std::io::Error::other(format!(
                "session {} has no terminal descriptor",
                frozen.id
            )));
        };
        let pidfd = frozen.session.pidfd();
        let mut fds = vec![master];
        fds.extend(pidfd);
        let PtyExport {
            meta,
            snapshot,
            held_input,
        } = &frozen.export;
        let mut bin = Vec::with_capacity(snapshot.len() + held_input.len());
        bin.extend_from_slice(snapshot);
        bin.extend_from_slice(held_input);
        send_frame(
            stream,
            &Msg::Session {
                id: frozen.id.clone(),
                pty: meta.clone(),
                snapshot_len: snapshot.len(),
                pidfd: pidfd.is_some(),
            },
            &bin,
            &fds,
        )?;
    }
    Ok(())
}

// --- the new hub: taking over -----------------------------------------------

/// What a new hub received from the old one.
#[derive(Debug)]
pub struct Received {
    pub(crate) incoming: Incoming,
    /// The listening socket (blocking mode as passed; the caller makes it
    /// non-blocking for tokio).
    pub listener: std::net::TcpListener,
    pub old_pid: u32,
    pub old_version: String,
    pub sessions: usize,
    link: UnixStream,
}

/// Ask the hub serving `state_dir` how it runs (`fw hub upgrade`).
pub fn describe(state_dir: &Path) -> Result<Description, HandoffError> {
    describe_within(state_dir, FIRST_FRAME_TIMEOUT)
}

/// [`describe`], waiting at most `timeout` for the answer.
pub fn describe_within(state_dir: &Path, timeout: Duration) -> Result<Description, HandoffError> {
    let stream = connect(state_dir)?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    send_frame(&stream, &Msg::Describe, &[], &[]).map_err(io_failed("asking the hub"))?;
    match recv_frame(&stream) {
        Ok((Msg::Description(description), _, _)) => Ok(description),
        Ok((other, _, _)) => Err(HandoffError::Failed(format!(
            "unexpected answer from the hub: {other:?}"
        ))),
        Err(e) => Err(HandoffError::Failed(format!("asking the hub: {e}"))),
    }
}

fn connect(state_dir: &Path) -> Result<UnixStream, HandoffError> {
    let path = socket_path(state_dir).map_err(io_failed("handoff socket"))?;
    let stream = UnixStream::connect(&path).map_err(|e| {
        HandoffError::NoHub(format!(
            "no hub answers on {} ({e}): none is running for state dir {}, or it predates live \
             handoff",
            path.display(),
            state_dir.display()
        ))
    })?;
    same_user(&stream).map_err(io_failed("handoff socket"))?;
    Ok(stream)
}

/// Take over from the hub serving `state_dir`: say hello and receive its
/// sockets, state, and sessions. The old hub is frozen from here until
/// [`Received::ready`] (or the connection drops: it then thaws).
pub fn receive(
    state_dir: &Path,
    scrollback_lines: usize,
    step: Option<Duration>,
) -> Result<Received, HandoffError> {
    let stream = connect(state_dir)?;
    let _ = stream.set_write_timeout(Some(STEP_TIMEOUT));
    let _ = stream.set_read_timeout(Some(STEP_TIMEOUT));
    send_frame(
        &stream,
        &Msg::Hello {
            protocol: PROTOCOL,
            version: version(),
            pid: std::process::id(),
            step_secs: step.map(|step| step.as_secs()),
        },
        &[],
        &[],
    )
    .map_err(io_failed("greeting the running hub"))?;
    let (msg, _, fds) = recv_frame(&stream).map_err(io_failed("waiting for the running hub"))?;
    let (old_version, old_pid, report_path, board, count) = match msg {
        Msg::Offer {
            version,
            pid,
            report_path,
            board,
            sessions,
            ..
        } => (version, pid, report_path, board, sessions),
        Msg::Refused { reason, kind } => return Err(HandoffError::Refused { reason, kind }),
        other => {
            return Err(HandoffError::Failed(format!(
                "unexpected message from the running hub: {other:?}"
            )));
        }
    };
    let mut fds = fds.into_iter();
    let (Some(tcp), Some(lock)) = (fds.next(), fds.next()) else {
        return Err(HandoffError::Failed(
            "the running hub sent no listening socket or serve lock".into(),
        ));
    };
    let report = match (report_path, fds.next()) {
        (Some(path), Some(fd)) => Some((path, UnixListener::from(fd))),
        (None, None) => None,
        _ => {
            return Err(HandoffError::Failed(
                "the running hub's report socket did not arrive".into(),
            ));
        }
    };
    let serve_lock = crate::remote::ServeLock::adopt(std::fs::File::from(lock))
        .map_err(io_failed("serve lock"))?;
    let listener = std::net::TcpListener::from(tcp);
    let mut sessions = Vec::with_capacity(count);
    for _ in 0..count {
        let (msg, bin, fds) = recv_frame(&stream).map_err(io_failed("receiving the sessions"))?;
        let Msg::Session {
            id,
            pty,
            snapshot_len,
            pidfd,
        } = msg
        else {
            return Err(HandoffError::Failed(format!(
                "expected a session, got {msg:?}"
            )));
        };
        let mut fds = fds.into_iter();
        let Some(master) = fds.next() else {
            return Err(HandoffError::Failed(format!(
                "session {id}: no terminal descriptor arrived"
            )));
        };
        let pidfd = if pidfd { fds.next() } else { None };
        if snapshot_len > bin.len() {
            return Err(HandoffError::Failed(format!(
                "session {id}: truncated snapshot"
            )));
        }
        let mut snapshot = bin;
        let held_input = snapshot.split_off(snapshot_len);
        sessions.push(IncomingSession {
            id,
            adopt: Adopt {
                master,
                pidfd,
                export: PtyExport {
                    meta: pty,
                    snapshot,
                    held_input,
                },
                scrollback_lines,
            },
        });
    }
    Ok(Received {
        incoming: Incoming {
            serve_lock,
            report,
            board,
            sessions,
        },
        listener,
        old_pid,
        old_version,
        sessions: count,
        link: stream,
    })
}

/// The link to the old hub once the new hub is built.
#[derive(Debug)]
pub struct Link {
    stream: UnixStream,
    old_pid: u32,
}

impl Received {
    /// How many of the sessions' processes still ran.
    pub fn running(&self) -> usize {
        self.incoming
            .sessions
            .iter()
            .filter(|s| s.adopt.export.meta.exit.is_none())
            .count()
    }

    /// Split off what [`Hub`] construction consumes.
    pub(crate) fn take(self) -> (Incoming, std::net::TcpListener, Link) {
        (
            self.incoming,
            self.listener,
            Link {
                stream: self.link,
                old_pid: self.old_pid,
            },
        )
    }
}

impl Link {
    /// Building the new hub failed: tell the old one (it thaws).
    pub fn fail(self, reason: &str) {
        let _ = send_frame(
            &self.stream,
            &Msg::Failed {
                reason: reason.to_owned(),
            },
            &[],
            &[],
        );
    }

    /// Tell the old hub this one is ready, wait for it to let go, then
    /// start `hub`'s sessions and confirm. An old hub that vanished after
    /// `ready` is taken over from all the same.
    pub fn ready(self, hub: &Arc<Hub>) -> Result<(), HandoffError> {
        send_frame(&self.stream, &Msg::Ready, &[], &[])
            .map_err(io_failed("telling the running hub"))?;
        match recv_frame(&self.stream) {
            Ok((Msg::Commit, _, _)) => {}
            Ok((Msg::Abort { reason }, _, _)) => {
                return Err(HandoffError::Failed(format!(
                    "the running hub called the handoff off: {reason}"
                )));
            }
            Ok((other, _, _)) => {
                return Err(HandoffError::Failed(format!(
                    "unexpected message from the running hub: {other:?}"
                )));
            }
            Err(e) if process_alive(self.old_pid) => {
                return Err(HandoffError::Failed(format!(
                    "the running hub did not let go: {e}"
                )));
            }
            Err(_) => {
                eprintln!(
                    "fw hub: the old hub (pid {}) is gone; taking over",
                    self.old_pid
                );
            }
        }
        let _ = send_frame(&self.stream, &Msg::Resuming, &[], &[]);
        hub.handoff_commit();
        let _ = send_frame(
            &self.stream,
            &Msg::Started {
                pid: std::process::id(),
            },
            &[],
            &[],
        );
        Ok(())
    }
}

/// Wait (up to `timeout`) for process `pid` to be gone.
pub fn wait_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while process_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_carry_json_binary_and_descriptors() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut pipe_read, pipe_write) = std::io::pipe().unwrap();
        let bin: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let writer = std::thread::spawn(move || {
            send_frame(
                &a,
                &Msg::Abort {
                    reason: "because".into(),
                },
                &bin,
                &[pipe_write.as_raw_fd()],
            )
            .unwrap();
            send_frame(&a, &Msg::Commit, &[], &[]).unwrap();
            bin
        });
        let (msg, got, fds) = recv_frame(&b).unwrap();
        let bin = writer.join().unwrap();
        assert!(matches!(msg, Msg::Abort { reason } if reason == "because"));
        assert_eq!(got, bin);
        assert_eq!(fds.len(), 1);
        // The passed descriptor is the pipe's write end, close-on-exec.
        // SAFETY: fcntl on a descriptor we own.
        let flags = unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        std::fs::File::from(fds.into_iter().next().unwrap())
            .write_all(b"through")
            .unwrap();
        let mut buf = [0u8; 7];
        pipe_read.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"through");
        let (msg, got, fds) = recv_frame(&b).unwrap();
        assert!(matches!(msg, Msg::Commit) && got.is_empty() && fds.is_empty());
    }

    #[test]
    fn a_closed_peer_and_a_foreign_message_are_errors() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(a);
        assert_eq!(
            recv_frame(&b).unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        let (a, b) = UnixStream::pair().unwrap();
        let json = br#"{"type":"from_the_future"}"#;
        let mut header = [0u8; 8];
        header[..4].copy_from_slice(&u32::try_from(json.len()).unwrap().to_be_bytes());
        (&a).write_all(&header).unwrap();
        (&a).write_all(json).unwrap();
        let error = recv_frame(&b).unwrap_err();
        assert!(error.to_string().contains("protocol version"), "{error}");
    }

    #[test]
    fn peers_must_be_this_user() {
        let (a, _b) = UnixStream::pair().unwrap();
        same_user(&a).unwrap();
    }
}
