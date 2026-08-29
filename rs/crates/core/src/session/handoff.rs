//! **Descriptor handoff** — passing open file descriptors between two daemon processes
//! over a unix socket, via `SCM_RIGHTS` ancillary data.
//!
//! This is the transport under the daemon **live upgrade** (`docs/mux-backend-plan.md`,
//! M1). Today an upgrade that bumps [`proto::PROTO_VER`](super::proto::PROTO_VER) makes
//! [`DaemonClient::new`](super::daemon_client::DaemonClient::new) tear the running daemon
//! down — killing every session. The replacement is a *takeover*: the incumbent hands its
//! pty master descriptors to the freshly spawned daemon and exits. A shell dies from
//! `SIGHUP` when its **pty closes**, not when its parent exits, so as long as the successor
//! holds the master fd open, nothing downstream notices the swap.
//!
//! ## Why raw `libc`
//!
//! `std`'s ancillary-data API (`SocketAncillary`,
//! `UnixStream::send_vectored_with_ancillary`) is still unstable behind
//! `unix_socket_ancillary_data`, so the `sendmsg`/`recvmsg` pair is written out by hand.
//! `libc` is target-gated to unix in `Cargo.toml`.
//!
//! ## Framing
//!
//! `SCM_RIGHTS` attaches descriptors to a **message**, not to a byte range, and the kernel
//! discards the ancillary data if the accompanying normal data is empty — so every send
//! carries at least one byte. A message here is:
//!
//! ```text
//! [ 4-byte big-endian payload length ][ payload bytes ]   + N descriptors out-of-band
//! ```
//!
//! The length prefix and as much payload as fits ride in the single `sendmsg` that carries
//! the descriptors; [`recv_with_fds`] collects the descriptors from that first `recvmsg`
//! and then drains any remaining payload with ordinary reads. That keeps the contract
//! robust against a short read without requiring the whole payload to fit in one datagram's
//! worth of stream buffer.
//!
//! ## Safety notes baked in
//!
//! * Received descriptors are wrapped in [`OwnedFd`] **immediately**, so an error path
//!   anywhere after the `recvmsg` still closes them rather than leaking.
//! * Every received descriptor is marked close-on-exec, so an adopted pty master never
//!   survives an `exec` into an unrelated child. `MSG_CMSG_CLOEXEC` does this atomically
//!   where the platform has it; macOS does not, so an explicit `FD_CLOEXEC` backs it up.
//! * `MSG_CTRUNC` (the control buffer was too small and the kernel silently dropped
//!   descriptors) is treated as a hard error, after closing whatever did arrive — a partial
//!   handoff would strand sessions in neither process.
//! * `EINTR` is retried; every other `errno` propagates.

use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;

/// Hard cap on descriptors in one handoff message. `SCM_RIGHTS` is itself bounded by the
/// kernel's `SCM_MAX_FD` (253 on Linux,
/// similar on the BSDs/macOS), so a handoff of many sessions is *chunked* by the caller
/// rather than sent as one oversized message. Keeping our own cap well under the kernel's
/// means a rejected send is our clear error, not an opaque `EMSGSIZE`.
pub const MAX_FDS_PER_MSG: usize = 64;

/// Hard cap on one message's payload. The handoff payload is per-session JSON metadata
/// (uid, spawn spec, cwd, replay buffer), so this is generous; it exists to bound the
/// receiver's allocation against a hostile or corrupt peer rather than to constrain
/// legitimate traffic.
pub const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

/// Send `payload` together with `fds` as one handoff message.
///
/// The descriptors are **duplicated** into the peer by the kernel; the caller keeps
/// ownership of the originals and is responsible for closing them (for a takeover, by
/// exiting). Blocks until the whole payload is written.
///
/// Errors if `fds` exceeds [`MAX_FDS_PER_MSG`] or `payload` exceeds [`MAX_PAYLOAD`] — a
/// caller that needs more must chunk.
pub fn send_with_fds(sock: &UnixStream, payload: &[u8], fds: &[RawFd]) -> io::Result<()> {
    if fds.len() > MAX_FDS_PER_MSG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} fds exceeds the {MAX_FDS_PER_MSG} per-message cap",
                fds.len()
            ),
        ));
    }
    if payload.len() > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("payload of {} bytes exceeds the cap", payload.len()),
        ));
    }

    // The length prefix always rides in the ancillary-carrying send, so the receiver can
    // size its payload drain from the very first recvmsg.
    let mut header = Vec::with_capacity(4 + payload.len());
    header.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    header.extend_from_slice(payload);

    let sent = sendmsg_with_fds(sock, &header, fds)?;
    // Whatever the first send could not fit is ordinary stream data — no ancillary needed.
    if sent < header.len() {
        let mut w = sock;
        w.write_all(&header[sent..])?;
    }
    Ok(())
}

/// Receive one handoff message: its payload plus the descriptors that rode with it.
///
/// Returns `Ok(None)` on a clean EOF **before any bytes of a message arrived** — the peer
/// closed without sending, which is how a takeover loop learns the incumbent is done.
/// An EOF *mid-message* is an error, since a truncated handoff must never be mistaken for
/// a complete one.
pub fn recv_with_fds(sock: &UnixStream) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
    let mut head = [0u8; 4];
    let (n, fds) = recvmsg_with_fds(sock, &mut head)?;
    if n == 0 && fds.is_empty() {
        return Ok(None); // clean EOF between messages
    }

    // The first recvmsg may have returned fewer than the 4 header bytes; finish it with
    // plain reads. `fds` is already owned, so an error here still closes the descriptors.
    let mut r = sock;
    if n < 4 {
        r.read_exact(&mut head[n..])?;
    }
    let len = u32::from_be_bytes(head) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("peer announced a {len}-byte payload, over the cap"),
        ));
    }

    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((payload, fds)))
}

/// One `sendmsg` carrying `buf` plus an `SCM_RIGHTS` control message for `fds`. Returns
/// the number of `buf` bytes the kernel accepted (a stream socket may take fewer).
fn sendmsg_with_fds(sock: &UnixStream, buf: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    debug_assert!(!buf.is_empty(), "SCM_RIGHTS needs at least one data byte");

    // SAFETY: every pointer below addresses a live local; `cmsg` is sized by CMSG_SPACE for
    // exactly `fds.len()` descriptors and written through the CMSG macros, which is the
    // documented way to build the control buffer.
    unsafe {
        let fd_bytes = std::mem::size_of_val(fds) as u32;
        let mut cmsg_buf = vec![0u8; libc::CMSG_SPACE(fd_bytes) as usize];

        let mut iov = libc::iovec {
            iov_base: buf.as_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = cmsg_buf.len() as _;

            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr(),
                libc::CMSG_DATA(cmsg) as *mut RawFd,
                fds.len(),
            );
        }

        loop {
            let n = libc::sendmsg(sock.as_raw_fd(), &msg, 0);
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

/// `MSG_CMSG_CLOEXEC` closes the window between `recvmsg` and the explicit `FD_CLOEXEC`
/// below, in which a concurrent `fork`+`exec` could inherit an adopted pty master. Linux and
/// the BSDs have it; **macOS does not**, so there the flag is 0 and [`set_cloexec`] alone
/// carries the guarantee (a race that needs another thread to exec in that instant).
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
const RECV_FLAGS: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(any(target_os = "macos", target_os = "ios"))]
const RECV_FLAGS: libc::c_int = 0;

/// Mark `fd` close-on-exec. Called on every received descriptor — redundant where
/// [`RECV_FLAGS`] already set it, and load-bearing on macOS where it did not.
fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is owned by the caller (already wrapped in an `OwnedFd`) and live.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// One `recvmsg` into `buf`, collecting any `SCM_RIGHTS` descriptors. Returns the byte
/// count plus the descriptors, already owned.
fn recvmsg_with_fds(sock: &UnixStream, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    // SAFETY: as in `sendmsg_with_fds` — locals outlive the call, and the control buffer is
    // sized by CMSG_SPACE for the maximum descriptor count we are willing to accept.
    unsafe {
        let max_bytes = (MAX_FDS_PER_MSG * std::mem::size_of::<RawFd>()) as u32;
        let mut cmsg_buf = vec![0u8; libc::CMSG_SPACE(max_bytes) as usize];

        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_buf.len() as _;

        let n = loop {
            // An adopted pty master must not leak into an unrelated exec — see RECV_FLAGS.
            let n = libc::recvmsg(sock.as_raw_fd(), &mut msg, RECV_FLAGS);
            if n >= 0 {
                break n as usize;
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        };

        // Take ownership of every descriptor FIRST, so any error below still closes them.
        let mut fds = Vec::new();
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let payload_len = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let count = payload_len / std::mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(cmsg) as *const RawFd;
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }

        // Belt-and-braces close-on-exec (the only guarantee on macOS, see RECV_FLAGS).
        for fd in &fds {
            set_cloexec(fd.as_raw_fd())?;
        }

        // A truncated control buffer means the kernel dropped descriptors on the floor.
        // Half a handoff is worse than none: fail loudly (dropping `fds` closes what came).
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SCM_RIGHTS control message truncated — descriptors were dropped",
            ));
        }
        Ok((n, fds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    /// A connected pair to hand descriptors across, standing in for the incumbent daemon
    /// and its successor.
    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socketpair")
    }

    /// A temp file with known contents, used as a stand-in for a pty master: the assertion
    /// that matters is that the *receiver's* descriptor reads the same open file
    /// description, which is exactly the property a live pty needs.
    fn temp_file_with(contents: &[u8]) -> std::fs::File {
        let path = std::env::temp_dir().join(format!("hp-handoff-{}", uuid::Uuid::new_v4()));
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .expect("temp file");
        f.write_all(contents).expect("write");
        f.seek(SeekFrom::Start(0)).expect("rewind");
        // Unlink now: the open descriptor keeps it alive, and nothing is left behind even
        // if the test panics.
        let _ = std::fs::remove_file(&path);
        f
    }

    /// The core contract: a descriptor sent over the socket arrives usable on the other
    /// side, and the payload that rode with it survives intact.
    #[test]
    fn sends_a_descriptor_and_its_payload() {
        let (a, b) = pair();
        let file = temp_file_with(b"session-output");

        send_with_fds(&a, b"{\"uid\":\"pane-1\"}", &[file.as_raw_fd()]).expect("send");

        let (payload, fds) = recv_with_fds(&b).expect("recv").expect("a message");
        assert_eq!(payload, b"{\"uid\":\"pane-1\"}");
        assert_eq!(fds.len(), 1, "exactly the one descriptor sent");

        // The received descriptor reads the same open file description.
        let mut got = String::new();
        std::fs::File::from(fds.into_iter().next().unwrap())
            .read_to_string(&mut got)
            .expect("read through the received fd");
        assert_eq!(got, "session-output");
    }

    /// The sender's copy stays valid after the handoff — the kernel duplicates rather than
    /// moves. This is what lets the incumbent daemon keep serving until it chooses to exit.
    #[test]
    fn sender_keeps_its_own_descriptor() {
        let (a, b) = pair();
        let mut file = temp_file_with(b"still-mine");

        send_with_fds(&a, b"x", &[file.as_raw_fd()]).expect("send");
        let (_, fds) = recv_with_fds(&b).expect("recv").expect("a message");
        drop(fds); // successor closes its copy

        file.seek(SeekFrom::Start(0)).expect("rewind");
        let mut got = String::new();
        file.read_to_string(&mut got)
            .expect("sender's fd still open");
        assert_eq!(got, "still-mine");
    }

    /// A whole batch of sessions moves in one message, in order.
    #[test]
    fn sends_many_descriptors_in_order() {
        let (a, b) = pair();
        let files: Vec<_> = (0..8)
            .map(|i| temp_file_with(format!("pane-{i}").as_bytes()))
            .collect();
        let raw: Vec<RawFd> = files.iter().map(|f| f.as_raw_fd()).collect();

        send_with_fds(&a, b"batch", &raw).expect("send");
        let (payload, fds) = recv_with_fds(&b).expect("recv").expect("a message");

        assert_eq!(payload, b"batch");
        assert_eq!(fds.len(), 8);
        for (i, fd) in fds.into_iter().enumerate() {
            let mut got = String::new();
            std::fs::File::from(fd).read_to_string(&mut got).unwrap();
            assert_eq!(got, format!("pane-{i}"), "descriptor order is preserved");
        }
    }

    /// A payload far larger than any single socket buffer round-trips: the ancillary send
    /// carries what it can and the rest drains as ordinary stream bytes. This is the short-
    /// read path, which a naive one-recvmsg implementation gets wrong.
    #[test]
    fn large_payload_survives_a_short_first_send() {
        let (a, b) = pair();
        let file = temp_file_with(b"fd");
        let big: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();

        // The reader must run concurrently — a payload this size will not fit in the
        // socket buffer, so `send_with_fds` blocks partway through.
        let reader = std::thread::spawn(move || recv_with_fds(&b).expect("recv"));
        send_with_fds(&a, &big, &[file.as_raw_fd()]).expect("send");

        let (payload, fds) = reader.join().expect("reader thread").expect("a message");
        assert_eq!(payload.len(), big.len());
        assert_eq!(payload, big, "every byte survives the split send");
        assert_eq!(fds.len(), 1, "the descriptor rode the first chunk");
    }

    /// Descriptors are optional: a plain metadata message (say, "that was the last batch")
    /// works through the same framing.
    #[test]
    fn sends_a_payload_with_no_descriptors() {
        let (a, b) = pair();
        send_with_fds(&a, b"done", &[]).expect("send");
        let (payload, fds) = recv_with_fds(&b).expect("recv").expect("a message");
        assert_eq!(payload, b"done");
        assert!(fds.is_empty());
    }

    /// Several messages queue up and are read back one at a time, so a takeover can stream
    /// sessions in chunks rather than building one oversized message.
    #[test]
    fn messages_are_framed_not_merged() {
        let (a, b) = pair();
        send_with_fds(&a, b"first", &[]).expect("send");
        send_with_fds(&a, b"second", &[]).expect("send");

        let (p1, _) = recv_with_fds(&b).expect("recv").expect("first");
        let (p2, _) = recv_with_fds(&b).expect("recv").expect("second");
        assert_eq!(p1, b"first");
        assert_eq!(p2, b"second");
    }

    /// A peer that closes between messages reads as a clean end of stream, not an error —
    /// that is how the successor learns the incumbent has finished handing over.
    #[test]
    fn clean_eof_between_messages_is_none() {
        let (a, b) = pair();
        send_with_fds(&a, b"only", &[]).expect("send");
        drop(a);

        assert!(recv_with_fds(&b).expect("recv").is_some(), "the message");
        assert!(
            recv_with_fds(&b).expect("recv").is_none(),
            "then a clean EOF"
        );
    }

    /// A peer that dies mid-message is an error, never a silently short payload — a
    /// truncated handoff must not be mistaken for a complete one.
    #[test]
    fn eof_mid_message_is_an_error() {
        let (a, b) = pair();
        // Announce 64 bytes, then send only 4 and hang up.
        let mut w = &a;
        w.write_all(&64u32.to_be_bytes()).expect("header");
        w.write_all(b"tiny").expect("partial");
        drop(a);

        let err = recv_with_fds(&b).expect_err("truncated payload must fail");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// Over-many descriptors are refused by us with a clear error rather than reaching the
    /// kernel as an opaque `EMSGSIZE` — the caller's cue to chunk.
    #[test]
    fn refuses_more_fds_than_one_message_allows() {
        let (a, _b) = pair();
        let file = temp_file_with(b"x");
        let too_many = vec![file.as_raw_fd(); MAX_FDS_PER_MSG + 1];

        let err = send_with_fds(&a, b"x", &too_many).expect_err("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// A corrupt or hostile peer cannot make the receiver allocate without bound.
    #[test]
    fn refuses_an_absurd_announced_payload() {
        let (a, b) = pair();
        let mut w = &a;
        w.write_all(&u32::MAX.to_be_bytes()).expect("header");

        let err = recv_with_fds(&b).expect_err("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
