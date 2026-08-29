//! **Cross-platform blocking transport** for the session-daemon wire protocol.
//!
//! The daemon speaks one framing ([`proto::write_frame`](crate::session::proto::write_frame)
//! / [`read_frame`](crate::session::proto::read_frame)) over one endpoint per salt. What
//! *carries* those frames differs by OS, and that is the only difference:
//!
//! | | unix | Windows |
//! |---|---|---|
//! | endpoint | `$XDG_RUNTIME_DIR/hyperpanes/<hash>.sock` | `\\.\pipe\hyperpanesd.<hash>` |
//! | client connection | [`UnixStream`](std::os::unix::net::UnixStream) | a [`File`](std::fs::File) opened on the pipe |
//! | one-per-salt gate | `flock` on a sibling `.lock` | `first_pipe_instance` on the pipe name |
//!
//! Both client connections are **blocking, bidirectional and cloneable**, which is what the
//! [`daemon_client`](crate::session::daemon_client) is built on (a write half behind a mutex
//! plus a reader thread). So the whole client — shadows, mirror, request/response, the
//! version probe, the M1 takeover fallback — is shared verbatim across platforms; only the
//! four functions here are cfg'd.
//!
//! Named pipes have no `SO_RCVTIMEO`, so the one place the client needs a *bounded* read
//! (the connect-time version probe) goes through [`read_frame_deadline`], which peeks the
//! pipe until a whole frame is buffered rather than setting a socket option.

use std::io;
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

use crate::session::proto::read_frame;
#[cfg(windows)]
use crate::session::proto::MAX_FRAME_LEN;

/// The daemon's address for one salt — a filesystem path on unix, a pipe name on Windows.
/// Carried as a string so the shared client code can log and compare it without cfgs; use
/// [`Endpoint::path`] on unix when a `Path` is wanted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint(String);

impl Endpoint {
    /// Wrap an already-known address (a socket path on unix, a pipe name on Windows).
    /// [`endpoint_for`] is the normal way in; this exists for callers handed an address
    /// rather than a salt — notably tests, which bind a temp socket of their own.
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The endpoint as the OS spells it (a socket path, or a `\\.\pipe\…` name).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The endpoint as a filesystem path — unix only, where the socket *is* a file (the
    /// tear-down path watches for it to disappear).
    #[cfg(unix)]
    pub fn path(&self) -> &std::path::Path {
        std::path::Path::new(&self.0)
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The endpoint a daemon for `salt` serves and a client connects to. Both sides derive it
/// from the salt alone (never pass it around), so a dev/isolated user-data dir gets its own
/// daemon on every OS.
#[cfg(unix)]
pub fn endpoint_for(salt: &str) -> Endpoint {
    Endpoint(
        crate::session::daemon::socket_path_for(salt)
            .to_string_lossy()
            .into_owned(),
    )
}

/// Windows: the salted named pipe (same FNV-1a token as the unix socket name).
#[cfg(windows)]
pub fn endpoint_for(salt: &str) -> Endpoint {
    Endpoint(crate::session::daemon::windows::pipe_name(salt))
}

/// A blocking, cloneable, bidirectional connection to the daemon.
#[cfg(unix)]
pub type Conn = std::os::unix::net::UnixStream;

/// A blocking, cloneable, bidirectional connection to the daemon: a named-pipe client
/// handle. Opening `\\.\pipe\…` with the ordinary file APIs yields exactly that — a
/// synchronous duplex handle whose `Read`/`Write` impls the shared framing can use, and
/// whose `try_clone` (a `DuplicateHandle`) gives the reader thread its own half.
#[cfg(windows)]
pub type Conn = std::fs::File;

/// Open a connection to the daemon at `ep`, or fail if none is listening.
#[cfg(unix)]
pub fn connect(ep: &Endpoint) -> io::Result<Conn> {
    std::os::unix::net::UnixStream::connect(ep.path())
}

/// Windows: open the pipe. `ERROR_PIPE_BUSY` (every pre-armed instance is taken for the
/// instant between a connect and the server arming the next one) is retried briefly — it is
/// a *transient* condition, not "no daemon", and surfacing it would make the client
/// needlessly respawn a daemon that is right there.
#[cfg(windows)]
pub fn connect(ep: &Endpoint) -> io::Result<Conn> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(ep.as_str())
        {
            Ok(f) => return Ok(f),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
}

/// A second handle onto the same connection — the client gives one half to its reader
/// thread and keeps the other for writes.
pub fn try_clone(conn: &Conn) -> io::Result<Conn> {
    conn.try_clone()
}

/// Whether a daemon is answering on `ep` right now. Used by the tear-down path to watch a
/// stale daemon go away; a connect that succeeds is dropped immediately.
pub fn is_live(ep: &Endpoint) -> bool {
    connect(ep).is_ok()
}

/// Read one frame, giving up after `budget`. `Ok(None)` means the budget expired with no
/// frame — the same answer on both platforms, so callers need no cfg.
///
/// Only the connect-time version probe and the takeover handshake need this: everything else
/// is either a fire-and-forget write or a reply the reader thread hands over through a
/// channel (which has its own timeout). Unix uses the socket's own receive timeout; Windows
/// peeks the pipe (see [`read_frame_deadline`]'s Windows body) because a pipe handle has no
/// equivalent option.
///
/// Both callers use it for a *single* frame on an otherwise idle connection, which is the
/// case where a timeout is clean: unix expires with nothing consumed, and Windows never
/// consumes at all. A timeout part-way through a frame's bytes would desync unix's framing,
/// so don't reach for this in the middle of a stream.
#[cfg(unix)]
pub fn read_frame_deadline<T: for<'de> serde::Deserialize<'de>>(
    conn: &Conn,
    budget: Duration,
) -> io::Result<Option<T>> {
    // NB: the timeout is a SOCKET option, shared by every `try_clone` of this connection
    // (including the one the manager later owns), so it MUST be cleared before returning or
    // the reader thread would spuriously time out.
    conn.set_read_timeout(Some(budget))?;
    let mut r = conn.try_clone()?;
    let out = read_frame::<_, T>(&mut r);
    let _ = conn.set_read_timeout(None);
    match out {
        // `SO_RCVTIMEO` reports expiry as an errno (EAGAIN on Linux/macOS, ETIMEDOUT on some
        // others); both mean "nothing arrived in time", which is `Ok(None)` in this API.
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Ok(None)
        }
        other => other,
    }
}

/// Windows: wait (by peeking) until a whole frame is buffered in the pipe, then read it with
/// the shared decoder. Peeking never consumes, so a timeout leaves the stream byte-exact for
/// whoever reads next — unlike a partial blocking read, which would desync the framing.
#[cfg(windows)]
pub fn read_frame_deadline<T: for<'de> serde::Deserialize<'de>>(
    conn: &Conn,
    budget: Duration,
) -> io::Result<Option<T>> {
    let deadline = Instant::now() + budget;
    // Phase 1: the 4-byte length prefix. Phase 2: the body it announces.
    let header = match peek_until(conn, 4, deadline)? {
        Some(bytes) => bytes,
        None => return Ok(None),
    };
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME_LEN",
        ));
    }
    if peek_until(conn, 4 + len as usize, deadline)?.is_none() {
        return Ok(None);
    }
    let mut r = conn.try_clone()?;
    read_frame::<_, T>(&mut r)
}

/// Peek the pipe until at least `want` bytes are buffered, returning the first 4 of them
/// (all any caller needs). `Ok(None)` means the deadline passed with fewer buffered — the
/// caller treats that as "no answer", and nothing has been consumed.
#[cfg(windows)]
fn peek_until(conn: &Conn, want: usize, deadline: Instant) -> io::Result<Option<[u8; 4]>> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::PeekNamedPipe;

    let handle = HANDLE(conn.as_raw_handle());
    loop {
        let mut head = [0u8; 4];
        let mut avail: u32 = 0;
        // SAFETY: `handle` is a live pipe handle owned by `conn` for this call's duration;
        // the two out-params are stack locals sized to what we pass.
        let ok = unsafe {
            PeekNamedPipe(
                handle,
                Some(head.as_mut_ptr() as *mut _),
                head.len() as u32,
                None,
                Some(&mut avail),
                None,
            )
        };
        if let Err(e) = ok {
            // A closed/broken pipe lands here too; the probe treats any error as "could not
            // confirm", which is the same conservative answer as a timeout.
            return Err(io::Error::other(format!("PeekNamedPipe failed: {e}")));
        }
        if avail as usize >= want {
            return Ok(Some(head));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One endpoint per salt, derived from the salt alone on both sides of the connection —
    // the property that lets a client and a daemon find each other without passing an address.
    #[test]
    fn endpoint_for_is_stable_per_salt_and_distinct_across_salts() {
        let a = endpoint_for("/home/x/.local/share/hyperpanes");
        let b = endpoint_for("/home/x/.local/share/hyperpanes");
        let c = endpoint_for("/home/x/.local/share/hyperpanes-dev");

        assert_eq!(a, b, "the same salt always resolves to the same endpoint");
        assert_ne!(a, c, "a dev/isolated user-data dir gets its own daemon");
        assert!(!a.as_str().is_empty());
    }

    // `Endpoint::new` is the "I was handed an address" door; it must not reinterpret it.
    #[test]
    fn endpoint_new_round_trips_verbatim() {
        let raw = if cfg!(windows) {
            r"\\.\pipe\hyperpanesd.0123456789abcdef"
        } else {
            "/run/user/1000/hyperpanes/0123456789abcdef.sock"
        };
        assert_eq!(Endpoint::new(raw).as_str(), raw);
        assert_eq!(Endpoint::new(raw).to_string(), raw);
    }

    // The bounded read used by the version probe: it returns a whole frame when one is there,
    // and — the part that matters — a timeout must leave the stream byte-exact, so the frame
    // that arrives late is still readable in full afterwards. (Windows gets this by peeking
    // rather than reading; unix by a socket receive timeout that consumes nothing.)
    #[cfg(unix)]
    #[test]
    fn read_frame_deadline_reads_a_frame_and_a_timeout_consumes_nothing() {
        use crate::session::proto::{write_frame, ClientMsg, PROTO_VER};

        let (client, server) = std::os::unix::net::UnixStream::pair().expect("pair");

        // Nothing sent yet → the budget expires with no frame.
        let empty =
            read_frame_deadline::<ClientMsg>(&client, Duration::from_millis(50)).expect("no error");
        assert!(
            empty.is_none(),
            "an empty stream times out rather than EOFs"
        );

        let mut w = server;
        write_frame(
            &mut w,
            &ClientMsg::Hello {
                proto_ver: PROTO_VER,
            },
        )
        .expect("write");

        let got = read_frame_deadline::<ClientMsg>(&client, Duration::from_secs(2))
            .expect("no error")
            .expect("a frame is waiting");
        assert_eq!(
            got,
            ClientMsg::Hello {
                proto_ver: PROTO_VER
            }
        );

        // The timeout must not have been left armed on the shared socket.
        assert_eq!(client.read_timeout().expect("read_timeout"), None);
    }
}
