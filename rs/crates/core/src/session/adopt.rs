//! **Adopting a pty master handed over by a predecessor daemon** — the receiving half of
//! the live upgrade (`docs/mux-backend-plan.md`, M1; transport in
//! [`handoff`](super::handoff)).
//!
//! [`spawn_pty`](super::pty::spawn_pty) creates a pty *and* a child. This module creates
//! neither: it wraps a master descriptor that arrived over `SCM_RIGHTS` and drives it with
//! the same reader-thread → [`PtyEvent`] pipeline, so from the registry's point of view an
//! adopted session is indistinguishable from a spawned one.
//!
//! ## What changes when a session is adopted
//!
//! **Exit detection moves from `waitpid` to pty EOF.** The child was forked by the *previous*
//! daemon; once that process exits the child is reparented to init and is no longer waitable
//! by anyone. EOF on the master is therefore the only available signal that the session
//! ended — and it is the reliable one, since a master only reaches EOF once every slave
//! descriptor is closed. The cost is the **exit code**, which no longer exists to be read:
//! an adopted session reports [`UNKNOWN_EXIT`], the same `-1` sentinel `spawn_pty` already
//! uses when a child cannot be reaped.
//!
//! **Killing hangs the terminal up** instead of calling a `ChildKiller` — see
//! [`AdoptedPty::kill`].
//!
//! Everything else — writes, resizes, the byte stream, replay, the screen mirror — is
//! unchanged, because all of it flows through the master descriptor that just changed hands.
//!
//! ## The trap on the *sending* side
//!
//! The incumbent must give its ptys up with [`Pty::relinquish`], never a plain drop:
//! `portable-pty`'s master writer transmits `\n` + EOT (`^D`) from its destructor, which an
//! interactive shell reads as end-of-input. Dropping the outgoing ptys would therefore kill
//! exactly the sessions the upgrade exists to preserve.

use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Mutex;
use std::thread;

use super::pty::{HandoffInfo, Pty, PtyEvent};

/// The exit code reported for an adopted session, whose real status died with the daemon
/// that forked it. Matches the sentinel `spawn_pty` already emits for an unreapable child,
/// so downstream consumers need no new case.
pub const UNKNOWN_EXIT: i32 = -1;

/// A pty whose master descriptor was inherited from a predecessor daemon rather than opened
/// here.
struct AdoptedPty {
    /// The master, owned. `None` once [`AdoptedPty::kill`] has closed it — the descriptor is
    /// the kill mechanism, so killing consumes it and later writes fail as a broken pipe.
    /// The reader thread holds its own duplicate, so it keeps draining until EOF regardless
    /// of this lock.
    master: Mutex<Option<OwnedFd>>,
    /// The child's foreground process group as the predecessor last saw it. Advisory only —
    /// [`AdoptedPty::kill`] re-reads the live value first, since the group recorded at
    /// handoff time may since have been replaced by whatever the shell ran next.
    pgrp: Option<i32>,
}

impl AdoptedPty {
    /// Run `f` with the master descriptor, or fail with a broken pipe if this pty was killed.
    fn with_fd<T>(&self, f: impl FnOnce(RawFd) -> io::Result<T>) -> io::Result<T> {
        let guard = self.master.lock().unwrap();
        match guard.as_ref() {
            Some(fd) => f(fd.as_raw_fd()),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "adopted pty was killed",
            )),
        }
    }
}

/// The foreground process group of the terminal on `fd`, if it has one.
fn foreground_pgrp(fd: RawFd) -> Option<i32> {
    // SAFETY: `fd` is a live pty master descriptor for the duration of the call.
    match unsafe { libc::tcgetpgrp(fd) } {
        pid if pid > 0 => Some(pid),
        _ => None,
    }
}

impl Pty for AdoptedPty {
    /// Write to the slave's stdin. There is no `Box<dyn Write>` to borrow — only the raw
    /// master — so this is a plain `write(2)` loop: partial writes are resumed and `EINTR`
    /// is retried.
    fn write(&self, data: &[u8]) -> io::Result<()> {
        self.with_fd(|fd| {
            let mut sent = 0usize;
            while sent < data.len() {
                // SAFETY: `fd` is live for the lock's duration; the slice is in bounds.
                let n = unsafe {
                    libc::write(
                        fd,
                        data[sent..].as_ptr() as *const libc::c_void,
                        data.len() - sent,
                    )
                };
                if n < 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(err);
                }
                sent += n as usize;
            }
            Ok(())
        })
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let ws = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        self.with_fd(|fd| {
            // SAFETY: `fd` is a live pty master and `ws` is the struct TIOCSWINSZ expects.
            if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })
    }

    /// Terminate the session by **hanging the terminal up**.
    ///
    /// There is no `ChildKiller` to call — the child belongs to a process that has exited —
    /// and no single pid is worth signalling either: the group the predecessor recorded was
    /// the foreground group at *handoff* time, which the shell may since have replaced. So
    /// this does what unplugging a terminal does: `SIGHUP` to whatever group is in the
    /// foreground *now*, then close the master. Closing is the part that always works — the
    /// kernel hangs up the session, and anything that ignored the signal hits `EIO` on its
    /// next read or write.
    ///
    /// Closing is also what drives exit detection: the reader thread's duplicate sees EOF
    /// once the child's slave descriptors are gone, and emits the single [`PtyEvent::Exit`].
    ///
    /// Idempotent — a second call is a no-op, as `kill` on an already-dead child is.
    fn kill(&self) -> io::Result<()> {
        let mut guard = self.master.lock().unwrap();
        let Some(master) = guard.take() else {
            return Ok(());
        };
        if let Some(pgrp) = foreground_pgrp(master.as_raw_fd()).or(self.pgrp) {
            // SAFETY: a plain signal send. A failure means the group is already gone
            // (ESRCH), which is the outcome this is asking for, so the result is ignored.
            unsafe { libc::killpg(pgrp, libc::SIGHUP) };
        }
        drop(master); // the hangup itself
        Ok(())
    }

    /// An adopted pty is itself handoff-able, so a *second* upgrade carries the same session
    /// on again. Without this a terminal would survive exactly one upgrade and die on the
    /// next. `None` once killed — there is no longer a descriptor to hand over.
    fn handoff_info(&self) -> Option<HandoffInfo> {
        let guard = self.master.lock().unwrap();
        let fd = guard.as_ref()?.as_raw_fd();
        Some(HandoffInfo {
            master_fd: fd,
            pgrp: foreground_pgrp(fd).or(self.pgrp),
        })
    }
}

/// Duplicate `fd` into a fresh owned descriptor (close-on-exec).
pub(crate) fn dup_cloexec(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: `fd` is live for the call; F_DUPFD_CLOEXEC returns a new descriptor owned by
    // no one else, wrapped immediately below so it cannot leak.
    let raw = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh descriptor with no other owner.
    Ok(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) })
}

/// Take ownership of a pty master received from a predecessor daemon and start driving it.
///
/// Mirrors [`spawn_pty`](super::pty::spawn_pty)'s contract — returns once the reader thread
/// is live, emits [`PtyEvent::Data`] chunks, then exactly one [`PtyEvent::Exit`] — so the
/// registry can install the result as an ordinary session.
///
/// `pgrp` is the child's foreground process group as reported by the predecessor
/// ([`HandoffInfo::pgrp`]); pass `None` if it was unavailable.
pub fn adopt_pty(
    master: OwnedFd,
    pgrp: Option<i32>,
    on_event: impl Fn(PtyEvent) + Send + 'static,
) -> io::Result<Box<dyn Pty>> {
    // The reader blocks in `read` for the life of the session, so it gets its own descriptor
    // rather than holding the lock that writes and `kill` need.
    let read_fd = dup_cloexec(master.as_raw_fd())?;

    thread::Builder::new()
        .name("hp-pty-adopted".into())
        .spawn(move || {
            let mut f = std::fs::File::from(read_fd);
            let mut buf = [0u8; 65536];
            loop {
                match f.read(&mut buf) {
                    Ok(0) => break, // EOF: every slave descriptor is closed — session over
                    Ok(n) => on_event(PtyEvent::Data(buf[..n].to_vec())),
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    // EIO is how a pty master reports "the slave hung up"; it is a normal
                    // end of stream here, not a fault, so it is handled as EOF.
                    Err(_) => break,
                }
            }
            // No `waitpid` is possible — the child was reparented when the predecessor
            // exited. EOF is the exit signal; the code itself is unrecoverable.
            on_event(PtyEvent::Exit(UNKNOWN_EXIT));
        })
        .map_err(|e| io::Error::other(e.to_string()))?;

    Ok(Box::new(AdoptedPty {
        master: Mutex::new(Some(master)),
        pgrp,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::handoff::{recv_with_fds, send_with_fds};
    use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// An "incumbent daemon" holding a live shell: the pty master plus the child, and
    /// **no reader thread**.
    ///
    /// Deliberately not [`spawn_pty`](crate::session::pty::spawn_pty). Two readers on one
    /// master race for every byte, and in a real upgrade there is only ever one — the
    /// incumbent's process is gone by the time the successor reads. A reader-less incumbent
    /// reproduces that and makes the assertions deterministic. `take_writer` is likewise
    /// never called, so no `UnixMasterWriter` exists to type `^D` on drop (see
    /// [`Pty::relinquish`]) and dropping this master is the clean close the incumbent's exit
    /// performs.
    struct Incumbent {
        master: Box<dyn MasterPty + Send>,
        _child: Box<dyn Child + Send + Sync>,
    }

    impl Incumbent {
        /// A `/bin/sh -i` on a fresh pty, with a bare prompt so output is easy to match.
        fn shell() -> Incumbent {
            let pair = native_pty_system()
                .openpty(PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .expect("openpty");
            let mut cmd = CommandBuilder::new("/bin/sh");
            cmd.arg("-i");
            cmd.env("PS1", "");
            // An explicit cwd, because `portable-pty` falls back to `$HOME` when none is set
            // and other tests in this binary overwrite `HOME` with a path that does not
            // exist (`paths.rs`), which would fail the spawn with ENOENT.
            cmd.cwd("/");
            let child = pair.slave.spawn_command(cmd).expect("spawn");
            // Drop our slave end: from here the child holds the only slave descriptors, so
            // EOF on the master means the session really ended.
            drop(pair.slave);
            Incumbent {
                master: pair.master,
                _child: child,
            }
        }

        /// What this pty would hand to a successor — the same shape `Pty::handoff_info`
        /// produces from a real `PortablePty`.
        fn handoff_info(&self) -> HandoffInfo {
            HandoffInfo {
                master_fd: self.master.as_raw_fd().expect("a unix master has an fd"),
                pgrp: self.master.process_group_leader(),
            }
        }
    }

    fn sink() -> (mpsc::Sender<PtyEvent>, mpsc::Receiver<PtyEvent>) {
        mpsc::channel()
    }

    /// Accumulate output until `pred` matches, or give up. Returns whatever was seen.
    fn wait_for_output(
        rx: &mpsc::Receiver<PtyEvent>,
        timeout: Duration,
        pred: impl Fn(&str) -> bool,
    ) -> String {
        let mut acc = String::new();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(PtyEvent::Data(d)) => {
                    acc.push_str(&String::from_utf8_lossy(&d));
                    if pred(&acc) {
                        return acc;
                    }
                }
                Ok(PtyEvent::Exit(_)) => return acc,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => return acc,
            }
        }
        acc
    }

    /// Wait for the single `Exit` event, or `None` on timeout.
    fn wait_for_exit(rx: &mpsc::Receiver<PtyEvent>, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(PtyEvent::Exit(code)) => return Some(code),
                Ok(PtyEvent::Data(_)) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => return None,
            }
        }
        None
    }

    /// **The whole M1 premise, end to end.** A live shell's pty master crosses a socket the
    /// way it will cross from an outgoing daemon to its successor, the incumbent then closes
    /// its own master exactly as its exit would, and the shell neither dies nor notices: the
    /// adopted side still reads its output and still writes to its stdin.
    #[test]
    fn a_live_shell_survives_the_handoff_and_stays_interactive() {
        let incumbent = Incumbent::shell();
        let info = incumbent.handoff_info();

        // The outgoing daemon hands the master over, with the session metadata alongside it.
        let (out, incoming) = UnixStream::pair().expect("socketpair");
        send_with_fds(&out, br#"{"uid":"pane-1"}"#, &[info.master_fd]).expect("send the master");
        let (meta, mut fds) = recv_with_fds(&incoming).expect("recv").expect("a message");
        assert_eq!(meta, br#"{"uid":"pane-1"}"#, "metadata rides with the fd");
        assert_eq!(fds.len(), 1);

        // The successor adopts it and starts driving.
        let (tx, rx) = sink();
        let adopted = adopt_pty(fds.pop().unwrap(), info.pgrp, move |ev| {
            let _ = tx.send(ev);
        })
        .expect("adopt");

        // The incumbent exits: its master descriptor and its socket close.
        drop(incumbent);
        drop(out);

        // The shell is still there, and still listening.
        adopted.write(b"echo after-handoff\n").expect("write");
        let got = wait_for_output(&rx, Duration::from_secs(10), |s| {
            s.contains("after-handoff")
        });
        assert!(
            got.contains("after-handoff"),
            "the shell survived the handoff and is still interactive, got: {got:?}"
        );

        adopted.kill().expect("kill");
    }

    /// Resizing an adopted pty reaches the child — `stty size` reports the new grid. This is
    /// the `TIOCSWINSZ` path, which a spawned pty gets from `portable-pty` for free but the
    /// adopted one implements itself.
    #[test]
    fn resize_on_an_adopted_pty_reaches_the_child() {
        let incumbent = Incumbent::shell();
        let info = incumbent.handoff_info();

        let (tx, rx) = sink();
        let adopted = adopt_pty(dup_cloexec(info.master_fd).unwrap(), info.pgrp, move |ev| {
            let _ = tx.send(ev);
        })
        .expect("adopt");
        drop(incumbent);

        adopted.resize(100, 40).expect("resize");
        adopted.write(b"stty size\n").expect("write");

        let got = wait_for_output(&rx, Duration::from_secs(10), |s| s.contains("40 100"));
        assert!(
            got.contains("40 100"),
            "the child sees the resized grid, got: {got:?}"
        );
        adopted.kill().expect("kill");
    }

    /// An adopted session still ends. The child exits on its own, its slave descriptors
    /// close, the master reaches EOF, and exactly one `Exit` is emitted — carrying
    /// [`UNKNOWN_EXIT`], because the real status died with the daemon that forked the child.
    #[test]
    fn adopted_session_reports_exit_at_pty_eof() {
        let incumbent = Incumbent::shell();
        let info = incumbent.handoff_info();

        let (tx, rx) = sink();
        let adopted = adopt_pty(dup_cloexec(info.master_fd).unwrap(), info.pgrp, move |ev| {
            let _ = tx.send(ev);
        })
        .expect("adopt");
        drop(incumbent);

        adopted.write(b"exit 3\n").expect("write");

        assert_eq!(
            wait_for_exit(&rx, Duration::from_secs(10)),
            Some(UNKNOWN_EXIT),
            "EOF ends the adopted session; the real exit code is unrecoverable"
        );
    }

    /// `kill` hangs the terminal up, which ends the session and produces the `Exit` event —
    /// the adopted equivalent of the `ChildKiller` that no longer exists here. It is also
    /// idempotent, and leaves nothing to hand on or write to.
    #[test]
    fn kill_hangs_up_the_terminal_and_ends_the_session() {
        let incumbent = Incumbent::shell();
        let info = incumbent.handoff_info();

        let (tx, rx) = sink();
        let adopted = adopt_pty(dup_cloexec(info.master_fd).unwrap(), info.pgrp, move |ev| {
            let _ = tx.send(ev);
        })
        .expect("adopt");
        drop(incumbent);

        adopted.kill().expect("kill");
        assert_eq!(
            wait_for_exit(&rx, Duration::from_secs(10)),
            Some(UNKNOWN_EXIT),
            "kill ends the adopted session"
        );

        adopted.kill().expect("a second kill is a no-op");
        assert!(
            adopted.handoff_info().is_none(),
            "a killed pty has no descriptor to hand on"
        );
        assert_eq!(
            adopted.write(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe,
            "writes to a killed pty fail rather than reaching a recycled descriptor"
        );
    }

    /// A session that has already been adopted can be adopted **again** — otherwise a
    /// terminal would survive exactly one upgrade and die on the next.
    ///
    /// Both adoptions are live in this one process, so both reader threads drain the same
    /// master and the kernel hands each byte to exactly one of them. That race cannot happen
    /// in a real second upgrade — the first successor's process is gone before the next one
    /// reads — so the test reproduces the part that matters (a descriptor handed on from an
    /// *adopted* pty still drives the shell) and merges both sinks rather than pretending the
    /// split is deterministic.
    #[test]
    fn an_adopted_pty_can_itself_be_handed_on() {
        let incumbent = Incumbent::shell();
        let first_info = incumbent.handoff_info();
        let (tx, rx) = sink();

        let relay = tx.clone();
        let first = adopt_pty(
            dup_cloexec(first_info.master_fd).unwrap(),
            first_info.pgrp,
            move |ev| {
                let _ = relay.send(ev);
            },
        )
        .expect("first adopt");
        drop(incumbent);

        // The first successor is upgraded away in turn: it surrenders its descriptor and
        // gives the pty up without disturbing the child.
        let info = first
            .handoff_info()
            .expect("an adopted pty is handoff-able too");
        let second = adopt_pty(dup_cloexec(info.master_fd).unwrap(), info.pgrp, move |ev| {
            let _ = tx.send(ev);
        })
        .expect("second adopt");
        first.relinquish();

        // Writing through the twice-handed-on descriptor still reaches the same shell.
        second.write(b"echo twice-adopted\n").expect("write");
        let got = wait_for_output(&rx, Duration::from_secs(10), |s| {
            s.contains("twice-adopted")
        });
        assert!(
            got.contains("twice-adopted"),
            "the session survives a second upgrade, got: {got:?}"
        );
        second.kill().expect("kill");
    }
}
