//! **The attach client core** (`docs/mux-backend-plan.md` M2) — the transport- and
//! terminal-agnostic half of `hyperpanes attach`.
//!
//! The daemon already owns everything an attach client needs: the PTYs, a 128 KiB rolling
//! [replay buffer](crate::session::replay), a headless screen mirror, and a framed
//! request/response + event-stream protocol ([`proto`](crate::session::proto)). So the
//! client is small: connect to the salt's endpoint, `Attach`, write the `Replay` seed to a
//! sink, then copy `Data` events out and keyboard bytes in until a detach key or the
//! session exits.
//!
//! ## The seed/stream seam
//! `Attach` subscribes the connection to the session's broadcast *before* it snapshots the
//! replay buffer — it has to, or a chunk flushed between the two would be lost. The cost is
//! an **overlap**: whatever is flushed in that window, plus anything already queued on the
//! bus, is inside the seed *and* arrives as a live `Data` event. The GUI can ignore the seed
//! because it keeps its own mirror; this client is the terminal itself and has nothing to
//! compare against, so it splices the other way — the daemon reports the seed's output
//! cursor ([`DaemonMsg::Replay::cursor`], read under the same lock that bumps it) and
//! [`Attachment`] drops every `Data` at or below it. Without that, attaching to a busy pane
//! paints its last chunk twice.
//!
//! Everything here is **pure protocol + policy**: no `termios`, no signals, no stdio. The
//! tty glue lives in the app crate (`app/src/attach_cli.rs`), and M3's SSH channel will
//! drive this same [`Attachment`] with the channel's reader/writer in place of stdin/stdout
//! — which is why the loops are generic over [`Read`]/[`Write`] rather than hard-wired.
//!
//! ## Resize policy — DECIDED: attach at the desktop's grid, letterbox
//! The plan's open question ("mobile resize contention") is settled here as
//! [`ResizePolicy::Observe`], the default: **an attach client never resizes the session.**
//! A phone attaching at 60x20 must not reflow a 200x50 desktop pane — the reflow is
//! destructive (wrapped scrollback is re-wrapped, full-screen apps redraw at the small
//! grid) and it is visible to a person sitting at the desktop who did not ask for it. So
//! the client renders the desktop's grid into whatever space it has: content occupies the
//! top-left `session.cols x session.rows` of the local terminal and the remainder stays
//! blank — a letterbox.
//!
//! [`ResizePolicy::Request`] is the explicit opt-in escape hatch (`--resize`), for the case
//! the plan calls "explicit resize request": the pane is detached, or the person *is* the
//! desktop user and wants the pane to follow this terminal. It is never the default and it
//! is never implicit.
//!
//! A local terminal SMALLER than the session grid is the one case the letterbox cannot
//! render honestly (absolute cursor addressing past the last local row is clamped by the
//! local terminal). The client detects it via [`fits`] and warns; rendering it correctly
//! would need a local VTE re-emulation of the pane's grid, which is not M2.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::session::proto::{
    read_frame, write_frame, ClientMsg, DaemonMsg, SessionMeta, PROTO_VER,
};
use crate::session::transport::{self, Conn};
use crate::session_manager::SessionEvent;

/// How long a request/response round-trip on an otherwise idle connection waits before
/// giving up. Generous (the daemon answers `ListSessions` / `Attach` from memory) but
/// bounded, so a wedged daemon can never hang the CLI.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether an attach client may resize the session it attaches to.
///
/// See the module docs: `Observe` is the shipped default and the answer to the plan's
/// "mobile resize contention" open question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResizePolicy {
    /// **Default.** Never send [`ClientMsg::Resize`]. The session keeps whatever grid the
    /// desktop gave it; this client letterboxes into its own terminal.
    #[default]
    Observe,
    /// Explicitly ask the daemon to resize the session to this client's terminal, at attach
    /// time and on every subsequent `SIGWINCH`. Reflows the pane for **every** viewer,
    /// including the desktop — opt-in only.
    Request,
}

/// Why [`Attachment::pump_output`] returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PumpEnd {
    /// The session's child exited with this code — the pane is gone; the client should say
    /// so and exit rather than pretending it is still attached.
    Exited(i32),
    /// The connection closed: a clean daemon shutdown, or *this* client detaching (the
    /// input side calls [`AttachWriter::disconnect`], which EOFs this read).
    Disconnected,
}

/// Connect to the **already running** daemon for `salt`. Unlike
/// [`DaemonSessionManager`](crate::session::daemon_client::DaemonSessionManager), this
/// never spawns one: `hyperpanes attach` is a client of a live workspace, and silently
/// starting an empty daemon would just produce an empty chooser.
///
/// The endpoint is derived by [`transport::endpoint_for`] — the same salt→address function
/// the GUI and the daemon use, so the hash is never re-derived here.
pub fn connect(salt: &str) -> io::Result<Conn> {
    let endpoint = transport::endpoint_for(salt);
    transport::connect(&endpoint).map_err(|e| match e.kind() {
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound => io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no hyperpanes session daemon is running for this install ({endpoint}). \
                 Start hyperpanes, or check HYPERPANES_USER_DATA_DIR."
            ),
        ),
        _ => e,
    })
}

/// Handshake: send `Hello` and read the daemon's. Returns the daemon's `PROTO_VER`.
///
/// The attach client deliberately does **not** act on a mismatch. The lock-step
/// tear-down/takeover in `daemon_client` exists because the GUI *owns* the daemon's
/// lifetime; an attach client does not, and killing a daemon full of someone's live shells
/// because a CLI is a build behind would be the worst possible failure mode. The caller
/// warns and proceeds.
pub fn handshake(conn: &Conn) -> io::Result<u32> {
    let mut w = transport::try_clone(conn)?;
    write_frame(
        &mut w,
        &ClientMsg::Hello {
            proto_ver: PROTO_VER,
        },
    )?;
    let deadline_end = std::time::Instant::now() + REQUEST_TIMEOUT;
    while std::time::Instant::now() < deadline_end {
        match transport::read_frame_deadline::<DaemonMsg>(conn, REQUEST_TIMEOUT)? {
            Some(DaemonMsg::Hello { proto_ver, .. }) => return Ok(proto_ver),
            // Nothing else should be in flight yet (we have attached to nothing), but be
            // tolerant of an unexpected frame rather than desyncing.
            Some(_) => continue,
            None => break,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "the session daemon did not answer the handshake",
    ))
}

/// Ask the daemon for every live session (→ [`DaemonMsg::Sessions`]). Used by the chooser
/// and by uid resolution.
pub fn list_sessions(conn: &Conn) -> io::Result<Vec<SessionMeta>> {
    let mut w = transport::try_clone(conn)?;
    write_frame(&mut w, &ClientMsg::ListSessions)?;
    let deadline_end = std::time::Instant::now() + REQUEST_TIMEOUT;
    while std::time::Instant::now() < deadline_end {
        match transport::read_frame_deadline::<DaemonMsg>(conn, REQUEST_TIMEOUT)? {
            Some(DaemonMsg::Sessions(list)) => return Ok(list),
            Some(_) => continue,
            None => break,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "the session daemon did not answer ListSessions",
    ))
}

/// How a uid query matched the live session set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UidMatch {
    /// Exactly one session matched — its full uid.
    One(String),
    /// Nothing matched.
    None,
    /// Several matched; the caller must disambiguate (these are the candidates).
    Ambiguous(Vec<String>),
}

/// Resolve a user-typed pane reference against the live sessions.
///
/// Uids are `pane-<uuid>` (`SessionManager::fresh_uid`), which nobody is going to type in
/// full, so the match ladder is: **exact**, then **prefix**, then **substring** — each rung
/// only tried when the one above found nothing, so an exact uid can never be ambiguous with
/// a prefix of another. A rung that matches several is reported as
/// [`Ambiguous`](UidMatch::Ambiguous) rather than silently picking one: attaching to the
/// wrong live shell is worse than an error message.
pub fn resolve_uid(sessions: &[SessionMeta], query: &str) -> UidMatch {
    let query = query.trim();
    if query.is_empty() {
        return UidMatch::None;
    }
    for rung in [
        |uid: &str, q: &str| uid == q,
        |uid: &str, q: &str| uid.starts_with(q),
        |uid: &str, q: &str| uid.contains(q),
    ] {
        let hits: Vec<String> = sessions
            .iter()
            .filter(|s| rung(&s.uid, query))
            .map(|s| s.uid.clone())
            .collect();
        match hits.len() {
            0 => continue,
            1 => return UidMatch::One(hits.into_iter().next().expect("len == 1")),
            _ => return UidMatch::Ambiguous(hits),
        }
    }
    UidMatch::None
}

/// Whether a `(cols, rows)` terminal can show a `(cols, rows)` session grid without
/// clipping. `None` for an unknown session grid (a daemon older than the `SessionMeta`
/// grid fields), which the caller treats as "can't tell, don't warn".
///
/// This is the letterbox test: `true` means the pane occupies the top-left of the terminal
/// and the rest is blank; `false` means the local terminal is too small and absolute cursor
/// addressing will be clamped, which no amount of escape-sequence forwarding can fix.
pub fn fits(terminal: (u16, u16), session: (Option<u16>, Option<u16>)) -> Option<bool> {
    match session {
        (Some(cols), Some(rows)) => Some(terminal.0 >= cols && terminal.1 >= rows),
        _ => None,
    }
}

/// A live attachment to one session: the read half of the connection plus the uid it is
/// following. Build with [`Attachment::open`]; drive with [`pump_output`](Self::pump_output)
/// on one thread and an [`AttachWriter`] on another.
pub struct Attachment {
    read: Conn,
    write: Arc<Mutex<Conn>>,
    uid: String,
    /// Events that arrived before the `Replay` reply. The daemon's per-connection writer
    /// drains replies before broadcast events, but `Attach` inserts the uid into the
    /// attached set *before* queueing `Replay`, so a broadcast already being forwarded can
    /// land first. Stashing them keeps the seam gapless: seed, then these, then the stream.
    pending: VecDeque<SessionEvent>,
    /// The session's output cursor at the instant the seed was snapshotted
    /// ([`DaemonMsg::Replay::cursor`]) — the **splice point**.
    ///
    /// `Attach` subscribes before it snapshots, so the seed and the live stream overlap by
    /// however much was flushed (or was already sitting on the bus) in that window. A GUI
    /// client can drop the seed instead, because it keeps its own mirror; this client *is*
    /// the terminal and has nothing to compare against, so it drops the other side: every
    /// `Data` whose own cursor is at or below this one is already painted.
    ///
    /// `0` means "the daemon did not report one" (a pre-`cursor` build) — no real chunk can
    /// end at cursor 0, so that value disables the filter rather than eating live output.
    seed_cursor: u64,
    /// Bytes written before a mid-stream `Replay` (a repaint — see
    /// [`AttachWriter::request_repaint`]). The caller supplies them because "clear the
    /// screen" is a terminal escape sequence and this module knows nothing about terminals;
    /// the tty client passes `ESC[H ESC[2J`. Empty by default.
    repaint_prefix: Vec<u8>,
}

impl Attachment {
    /// `Attach` to `uid` and return the attachment plus its **replay seed** — the daemon's
    /// rolling buffer for that session, to be written to the terminal once before the live
    /// stream starts. Empty when the session has produced nothing yet.
    pub fn open(conn: Conn, uid: &str) -> io::Result<(Self, String)> {
        let read = transport::try_clone(&conn)?;
        let write = Arc::new(Mutex::new(conn));
        {
            let mut w = write.lock().expect("attach write half");
            write_frame(
                &mut *w,
                &ClientMsg::Attach {
                    uid: uid.to_string(),
                },
            )?;
        }

        let mut pending = VecDeque::new();
        let mut seed = None;
        let deadline_end = std::time::Instant::now() + REQUEST_TIMEOUT;
        while std::time::Instant::now() < deadline_end {
            match transport::read_frame_deadline::<DaemonMsg>(&read, REQUEST_TIMEOUT)? {
                Some(DaemonMsg::Replay {
                    uid: got,
                    data,
                    cursor,
                }) if got == uid => {
                    seed = Some((data, cursor));
                    break;
                }
                Some(DaemonMsg::Event(ev)) => pending.push_back(ev),
                Some(_) => continue,
                None => break,
            }
        }
        let (seed, seed_cursor) = seed.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("the session daemon did not answer Attach for '{uid}'"),
            )
        })?;

        Ok((
            Self {
                read,
                write,
                uid: uid.to_string(),
                pending,
                seed_cursor,
                repaint_prefix: Vec::new(),
            },
            seed,
        ))
    }

    /// The uid this attachment follows.
    pub fn uid(&self) -> &str {
        &self.uid
    }

    /// Bytes to emit before a repaint's replay seed (typically a clear-screen sequence).
    pub fn set_repaint_prefix(&mut self, prefix: impl Into<Vec<u8>>) {
        self.repaint_prefix = prefix.into();
    }

    /// A cloneable handle for the input direction (keystrokes, resizes, detach). Safe to
    /// move to another thread while [`pump_output`](Self::pump_output) owns the read half.
    pub fn writer(&self) -> AttachWriter {
        AttachWriter {
            conn: Arc::clone(&self.write),
            uid: self.uid.clone(),
        }
    }

    /// Copy this session's live output to `out` until the child exits or the connection
    /// closes. Blocks; run it on its own thread (or as the caller's main loop).
    ///
    /// Events for other uids are ignored — the daemon only forwards uids this connection
    /// attached to, but the filter keeps the contract local. Non-`Data` events (cwd, OSC 133
    /// markers, agent state) carry no bytes for a dumb terminal and are dropped.
    pub fn pump_output<W: Write>(&mut self, out: &mut W) -> io::Result<PumpEnd> {
        while let Some(ev) = self.pending.pop_front() {
            if let Some(end) = self.write_event(ev, out)? {
                return Ok(end);
            }
        }
        loop {
            let frame = match read_frame::<_, DaemonMsg>(&mut self.read) {
                Ok(Some(f)) => f,
                Ok(None) => return Ok(PumpEnd::Disconnected),
                // A detach shuts the socket down under this read; the OS reports that as a
                // reset/not-connected rather than EOF on some platforms. Either way the
                // attachment is over — it is not an error the user needs to see.
                Err(e) if is_disconnect(&e) => return Ok(PumpEnd::Disconnected),
                Err(e) => return Err(e),
            };
            match frame {
                DaemonMsg::Event(ev) => {
                    if let Some(end) = self.write_event(ev, out)? {
                        return Ok(end);
                    }
                }
                // A mid-stream `Replay` is a repaint we asked for (`request_repaint`),
                // typically after a `SIGWINCH` under `ResizePolicy::Observe`: the pane's grid
                // did NOT change, so the honest response to the local terminal changing is to
                // redraw what the pane already contains rather than reflow the pane.
                //
                // The splice point moves with it. The screen now shows exactly the new
                // snapshot, so any `Data` at or below its cursor is already painted —
                // including chunks the daemon broadcast before the snapshot but the writer
                // delivers after it (replies are drained ahead of the bus, so that reordering
                // is the normal case, not the exotic one).
                DaemonMsg::Replay {
                    uid: got,
                    data,
                    cursor,
                } if got == self.uid => {
                    out.write_all(&self.repaint_prefix)?;
                    out.write_all(data.as_bytes())?;
                    out.flush()?;
                    self.seed_cursor = self.seed_cursor.max(cursor);
                }
                _ => {}
            }
        }
    }

    /// Write one event's bytes to `out`, or report that the pump should stop.
    ///
    /// `Data` already covered by the seed is dropped — see [`seed_cursor`](Self::seed_cursor).
    /// Non-`Data` events (cwd, OSC 133 markers, agent state) carry no bytes for a dumb
    /// terminal.
    fn write_event<W: Write>(
        &mut self,
        ev: SessionEvent,
        out: &mut W,
    ) -> io::Result<Option<PumpEnd>> {
        match ev {
            SessionEvent::Data {
                uid: got,
                data,
                cursor,
            } if got == self.uid => {
                // `cursor` is the value AFTER the chunk, and `flush_into` bumps it under the
                // same lock `replay_with_cursor` snapshots under — so a chunk is wholly
                // inside the seed or wholly outside it, never straddling. `cursor == 0` is
                // the "not reported" sentinel from a pre-`cursor` peer; forward it rather
                // than swallow live output.
                if cursor != 0 && cursor <= self.seed_cursor {
                    return Ok(None);
                }
                out.write_all(data.as_bytes())?;
                out.flush()?;
                Ok(None)
            }
            SessionEvent::Exit { uid: got, code } if got == self.uid => {
                Ok(Some(PumpEnd::Exited(code)))
            }
            _ => Ok(None),
        }
    }
}

/// Whether an I/O error means "the connection is gone" rather than a real fault.
fn is_disconnect(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
    )
}

/// The input direction of an attachment: keystrokes, resize requests, and the shutdown
/// that unblocks the output pump on detach. Cheap to clone-by-handle (`Attachment::writer`).
#[derive(Clone)]
pub struct AttachWriter {
    conn: Arc<Mutex<Conn>>,
    uid: String,
}

impl AttachWriter {
    /// Forward decoded keyboard text to the session's pty.
    pub fn send_input(&self, data: &str) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let mut w = self.conn.lock().expect("attach write half");
        write_frame(
            &mut *w,
            &ClientMsg::Write {
                uid: self.uid.clone(),
                data: data.to_string(),
            },
        )
    }

    /// Ask the daemon to resize the session. **Only** called under
    /// [`ResizePolicy::Request`] — see the module docs; this reflows the pane for every
    /// viewer, the desktop included.
    pub fn request_resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let mut w = self.conn.lock().expect("attach write half");
        write_frame(
            &mut *w,
            &ClientMsg::Resize {
                uid: self.uid.clone(),
                cols,
                rows,
            },
        )
    }

    /// Ask the daemon to re-send this session's replay buffer, which the output pump renders
    /// as a repaint (clear + re-seed). `Attach` is idempotent daemon-side — the uid is
    /// already in this connection's attached set — so this is a pure read.
    ///
    /// This is what a `SIGWINCH` does under [`ResizePolicy::Observe`]: the local terminal
    /// changed, the pane did not, so we redraw instead of reflowing.
    pub fn request_repaint(&self) -> io::Result<()> {
        let mut w = self.conn.lock().expect("attach write half");
        write_frame(
            &mut *w,
            &ClientMsg::Attach {
                uid: self.uid.clone(),
            },
        )
    }

    /// Detach: close the connection so the daemon drops this client and the output pump's
    /// blocking read returns. The **session keeps running** — no `Kill`, no `Shutdown` is
    /// ever sent from the attach client.
    #[cfg(unix)]
    pub fn disconnect(&self) {
        use std::net::Shutdown;
        if let Ok(w) = self.conn.lock() {
            let _ = w.shutdown(Shutdown::Both);
        }
    }

    /// Windows: a named-pipe `File` has no half-close, so detaching relies on dropping the
    /// last handle. The unix CLI is the only consumer today (see `attach_cli`), and M3's
    /// SSH channel closes its own transport; this keeps `core` compiling for the
    /// windows-latest leg without pretending to a capability the handle lacks.
    #[cfg(windows)]
    pub fn disconnect(&self) {}
}

// ---------------------------------------------------------------------------
// Detach key
// ---------------------------------------------------------------------------

/// The default detach prefix: `Ctrl-\` (0x1C, `FS`).
///
/// Chosen because it is the least-used control byte a shell user can produce: `Ctrl-C`,
/// `Ctrl-D`, `Ctrl-Z`, `Ctrl-A`/`Ctrl-B` (readline, tmux, screen) and `Ctrl-]` (telnet, and
/// vim's tag jump) are all live keys in a terminal. `Ctrl-\` is only `SIGQUIT`, and the
/// literal is still reachable by pressing it twice.
pub const DEFAULT_DETACH_PREFIX: u8 = 0x1C;

/// Parse a detach-key spec into the control byte it names.
///
/// Accepts `C-x`, `c-x`, `ctrl-x`, `^x` and a bare single character (which must already be a
/// control byte). The key must be a control byte: a printable detach key would eat a
/// character the shell needs on every keystroke.
pub fn parse_detach_key(spec: &str) -> Result<u8, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("empty detach key".to_string());
    }
    let lower = spec.to_ascii_lowercase();
    let rest = if let Some(r) = lower.strip_prefix("ctrl-") {
        r
    } else if let Some(r) = lower.strip_prefix("c-") {
        r
    } else if let Some(r) = lower.strip_prefix('^') {
        r
    } else {
        // A bare spec is only meaningful if it is already the control byte itself.
        let bytes = spec.as_bytes();
        if bytes.len() == 1 && bytes[0] < 0x20 {
            return Ok(bytes[0]);
        }
        return Err(format!(
            "invalid detach key '{spec}' (use e.g. C-\\, C-], ctrl-o)"
        ));
    };
    let bytes = rest.as_bytes();
    if bytes.len() != 1 {
        return Err(format!(
            "invalid detach key '{spec}' (name exactly one key, e.g. C-\\)"
        ));
    }
    // Ctrl-<c> is the ASCII control byte for `c` uppercased: @ A..Z [ \ ] ^ _ → 0x00..0x1F.
    let c = bytes[0].to_ascii_uppercase();
    if !(0x3F..=0x5F).contains(&c) {
        return Err(format!(
            "'{spec}' is not a control key (Ctrl- works on @, A-Z, [, \\, ], ^, _)"
        ));
    }
    Ok(c & 0x1F)
}

/// Render a control byte back as a `C-x` spec, for help text and the attach banner.
pub fn detach_key_label(key: u8) -> String {
    if key < 0x20 {
        format!("C-{}", (key | 0x40) as char)
    } else {
        format!("0x{key:02x}")
    }
}

/// What the caller must do with a chunk of keyboard bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterOut {
    /// Bytes to forward to the session (the prefix itself is never forwarded unless the
    /// user asked for a literal by pressing it twice).
    pub forward: Vec<u8>,
    /// The detach sequence completed — stop reading input and disconnect. Bytes typed
    /// *after* it in the same chunk are dropped: they were meant for whatever the user
    /// does next, not for the pane they just left.
    pub detach: bool,
}

/// The detach-key state machine.
///
/// `<prefix> d` detaches. `<prefix> <prefix>` sends one literal prefix byte through (so
/// `Ctrl-\`'s `SIGQUIT` is still reachable). `<prefix> <anything else>` discards the prefix
/// and forwards the other byte — a mistyped command should not send a stray control byte
/// into the shell. The prefix is stateful across chunks, because a `read` boundary can fall
/// between the two keystrokes.
#[derive(Debug, Default)]
pub struct DetachFilter {
    prefix: u8,
    armed: bool,
}

impl DetachFilter {
    /// A filter watching for `prefix`.
    pub fn new(prefix: u8) -> Self {
        Self {
            prefix,
            armed: false,
        }
    }

    /// Whether the prefix has been seen and the machine is waiting for the command key.
    pub fn armed(&self) -> bool {
        self.armed
    }

    /// Classify one chunk of raw keyboard bytes.
    pub fn feed(&mut self, buf: &[u8]) -> FilterOut {
        let mut forward = Vec::with_capacity(buf.len());
        for &b in buf {
            if self.armed {
                self.armed = false;
                if b == self.prefix {
                    forward.push(b); // literal: the prefix, typed twice
                } else if b == b'd' || b == b'D' {
                    return FilterOut {
                        forward,
                        detach: true,
                    };
                } else {
                    forward.push(b); // unknown command: drop the prefix, keep the key
                }
            } else if b == self.prefix {
                self.armed = true;
            } else {
                forward.push(b);
            }
        }
        FilterOut {
            forward,
            detach: false,
        }
    }
}

/// Read keyboard bytes from `input` and forward them to the session until the detach key or
/// EOF. Returns `true` if the user detached (as opposed to stdin closing).
///
/// Bytes are UTF-8 **stream**-decoded before being put on the wire ([`ClientMsg::Write`]
/// carries a `String`), so a multi-byte character split across two `read`s is not corrupted
/// into replacement characters — the same decoder the pty read path uses.
pub fn pump_input<R: Read>(
    mut input: R,
    writer: &AttachWriter,
    filter: &mut DetachFilter,
) -> io::Result<bool> {
    let mut buf = [0u8; 4096];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => return Ok(false),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if is_disconnect(&e) => return Ok(false),
            Err(e) => return Err(e),
        };
        let out = filter.feed(&buf[..n]);
        let text = crate::session_manager::decode_utf8_streaming(&mut carry, &out.forward);
        // A lone prefix keystroke (or the tail of a split multi-byte char) forwards nothing;
        // don't put an empty `Write` on the wire for it.
        if !text.is_empty() {
            writer.send_input(&text)?;
        }
        if out.detach {
            return Ok(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(uid: &str) -> SessionMeta {
        SessionMeta {
            uid: uid.to_string(),
            cwd: None,
            output_bytes: 0,
            last_output_at: None,
            alive: true,
            cols: None,
            rows: None,
        }
    }

    // ---- detach key parsing ----

    #[test]
    fn detach_key_accepts_the_spellings_a_person_types() {
        assert_eq!(parse_detach_key("C-\\"), Ok(0x1C));
        assert_eq!(parse_detach_key("c-\\"), Ok(0x1C));
        assert_eq!(parse_detach_key("ctrl-\\"), Ok(0x1C));
        assert_eq!(parse_detach_key("^\\"), Ok(0x1C));
        assert_eq!(parse_detach_key("C-]"), Ok(0x1D));
        assert_eq!(parse_detach_key("C-a"), Ok(0x01));
        assert_eq!(parse_detach_key("C-A"), Ok(0x01));
        assert_eq!(parse_detach_key("ctrl-o"), Ok(0x0F));
        // The raw control byte itself.
        assert_eq!(parse_detach_key("\u{1c}"), Ok(0x1C));
        assert_eq!(DEFAULT_DETACH_PREFIX, 0x1C);
    }

    #[test]
    fn detach_key_rejects_anything_that_would_eat_a_normal_keystroke() {
        // A printable key would be swallowed on every press.
        assert!(parse_detach_key("x").is_err());
        assert!(parse_detach_key("C-1").is_err());
        assert!(parse_detach_key("C-").is_err());
        assert!(parse_detach_key("").is_err());
        assert!(parse_detach_key("C-esc").is_err());
    }

    #[test]
    fn detach_key_label_round_trips_the_default() {
        assert_eq!(detach_key_label(DEFAULT_DETACH_PREFIX), "C-\\");
        assert_eq!(detach_key_label(0x01), "C-A");
        assert_eq!(
            parse_detach_key(&detach_key_label(DEFAULT_DETACH_PREFIX)),
            Ok(DEFAULT_DETACH_PREFIX)
        );
    }

    // ---- the detach state machine ----

    #[test]
    fn ordinary_keystrokes_pass_through_untouched() {
        let mut f = DetachFilter::new(DEFAULT_DETACH_PREFIX);
        let out = f.feed(b"ls -la\r");
        assert_eq!(out.forward, b"ls -la\r");
        assert!(!out.detach);
        assert!(!f.armed());
    }

    #[test]
    fn prefix_then_d_detaches_and_forwards_nothing_after_it() {
        let mut f = DetachFilter::new(DEFAULT_DETACH_PREFIX);
        let out = f.feed(b"echo\x1cdrest");
        assert_eq!(out.forward, b"echo", "the prefix is never forwarded");
        assert!(out.detach);
    }

    #[test]
    fn the_prefix_is_stateful_across_read_boundaries() {
        // A `read` can split the two keystrokes; the machine must remember.
        let mut f = DetachFilter::new(DEFAULT_DETACH_PREFIX);
        let first = f.feed(b"\x1c");
        assert!(first.forward.is_empty());
        assert!(!first.detach);
        assert!(f.armed());
        let second = f.feed(b"d");
        assert!(second.detach);
    }

    #[test]
    fn doubling_the_prefix_sends_one_literal_through() {
        let mut f = DetachFilter::new(DEFAULT_DETACH_PREFIX);
        let out = f.feed(b"\x1c\x1c");
        assert_eq!(out.forward, b"\x1c", "SIGQUIT is still reachable");
        assert!(!out.detach);
        assert!(!f.armed());
    }

    #[test]
    fn an_unknown_command_key_drops_the_prefix_and_keeps_the_key() {
        let mut f = DetachFilter::new(DEFAULT_DETACH_PREFIX);
        let out = f.feed(b"\x1cz");
        assert_eq!(out.forward, b"z");
        assert!(!out.detach);
    }

    #[test]
    fn a_custom_prefix_is_honoured_and_the_default_becomes_ordinary_input() {
        let mut f = DetachFilter::new(parse_detach_key("C-]").unwrap());
        let out = f.feed(b"\x1c\x1dd");
        assert_eq!(out.forward, b"\x1c", "C-\\ is just a byte now");
        assert!(out.detach);
    }

    // ---- uid resolution ----

    #[test]
    fn resolve_uid_prefers_an_exact_match_over_a_prefix_of_another() {
        let s = [meta("pane-ab"), meta("pane-abcd")];
        assert_eq!(resolve_uid(&s, "pane-ab"), UidMatch::One("pane-ab".into()));
    }

    #[test]
    fn resolve_uid_accepts_a_unique_prefix_or_substring() {
        let s = [meta("pane-aaa111"), meta("pane-bbb222")];
        assert_eq!(
            resolve_uid(&s, "pane-aaa"),
            UidMatch::One("pane-aaa111".into())
        );
        // Nobody types the `pane-` prefix; the uuid tail alone resolves by substring.
        assert_eq!(resolve_uid(&s, "bbb"), UidMatch::One("pane-bbb222".into()));
    }

    #[test]
    fn resolve_uid_reports_ambiguity_rather_than_guessing() {
        let s = [meta("pane-aaa111"), meta("pane-aaa222")];
        match resolve_uid(&s, "pane-aaa") {
            UidMatch::Ambiguous(v) => assert_eq!(v.len(), 2),
            other => panic!("expected ambiguity, got {other:?}"),
        }
        assert_eq!(resolve_uid(&s, "zzz"), UidMatch::None);
        assert_eq!(resolve_uid(&s, "  "), UidMatch::None);
    }

    // ---- the resize policy (the plan's open question, decided) ----

    #[test]
    fn observe_is_the_default_resize_policy() {
        assert_eq!(ResizePolicy::default(), ResizePolicy::Observe);
    }

    #[test]
    fn fits_is_the_letterbox_test() {
        // Bigger terminal than pane: letterboxes cleanly (pane top-left, rest blank).
        assert_eq!(fits((120, 40), (Some(80), Some(24))), Some(true));
        assert_eq!(fits((80, 24), (Some(80), Some(24))), Some(true));
        // Narrower or shorter: absolute cursor addressing gets clamped — warn.
        assert_eq!(fits((60, 40), (Some(80), Some(24))), Some(false));
        assert_eq!(fits((120, 20), (Some(80), Some(24))), Some(false));
        // A daemon that doesn't report a grid: no opinion, no false warning.
        assert_eq!(fits((80, 24), (None, None)), None);
        assert_eq!(fits((80, 24), (Some(80), None)), None);
    }

    // ---- the input pump ----

    /// A socketpair standing in for the daemon connection, so the input pump can be driven
    /// end-to-end and the frames it produced read back off the wire.
    #[cfg(unix)]
    fn pair() -> (Conn, Conn) {
        std::os::unix::net::UnixStream::pair().expect("socketpair")
    }

    // ---- the output pump ----

    #[cfg(unix)]
    fn attachment_over(client: Conn, uid: &str, repaint_prefix: &[u8]) -> Attachment {
        Attachment {
            read: client.try_clone().expect("clone"),
            write: Arc::new(Mutex::new(client)),
            uid: uid.to_string(),
            pending: VecDeque::new(),
            seed_cursor: 0,
            repaint_prefix: repaint_prefix.to_vec(),
        }
    }

    /// A `Data` event with no cursor — what a daemon predating `DaemonMsg::Replay::cursor`
    /// sends. The splice filter must pass these through untouched.
    #[cfg(unix)]
    fn data(uid: &str, s: &str) -> DaemonMsg {
        at(uid, s, 0)
    }

    /// A `Data` event ending at `cursor` (the monotonic value AFTER the chunk).
    #[cfg(unix)]
    fn at(uid: &str, s: &str, cursor: u64) -> DaemonMsg {
        DaemonMsg::Event(SessionEvent::Data {
            uid: uid.into(),
            data: s.into(),
            cursor,
        })
    }

    #[cfg(unix)]
    #[test]
    fn pump_output_copies_data_repaints_on_replay_and_stops_at_exit() {
        let (client, mut daemon) = pair();
        let mut att = attachment_over(client, "pane-1", b"<CLR>");
        write_frame(&mut daemon, &data("pane-1", "hi ")).expect("w");
        // A mid-stream Replay is a repaint: prefix, then the seed.
        write_frame(
            &mut daemon,
            &DaemonMsg::Replay {
                uid: "pane-1".into(),
                data: "redrawn".into(),
                cursor: 0,
            },
        )
        .expect("w");
        // Another session's traffic must never reach this terminal.
        write_frame(&mut daemon, &data("pane-2", "LEAK")).expect("w");
        write_frame(
            &mut daemon,
            &DaemonMsg::Event(SessionEvent::Exit {
                uid: "pane-1".into(),
                code: 3,
            }),
        )
        .expect("w");

        let mut out = Vec::new();
        assert_eq!(att.pump_output(&mut out).expect("pump"), PumpEnd::Exited(3));
        assert_eq!(String::from_utf8(out).expect("utf8"), "hi <CLR>redrawn");
    }

    // The attach seam. `ClientMsg::Attach` subscribes the connection BEFORE it snapshots the
    // replay buffer, so a chunk flushed in that window (or already queued on the bus) is both
    // inside the seed and delivered as a live event. Without the cursor splice the terminal
    // paints it twice — the pane's last line duplicated on every attach of a busy shell.
    #[cfg(unix)]
    #[test]
    fn data_already_inside_the_replay_seed_is_not_painted_twice() {
        let (client, mut daemon) = pair();
        let fake = std::thread::spawn(move || {
            let _ = read_frame::<_, ClientMsg>(&mut daemon).expect("attach req");
            // The seed covers everything up to cursor 10.
            write_frame(
                &mut daemon,
                &DaemonMsg::Replay {
                    uid: "pane-1".into(),
                    data: "SEED".into(),
                    cursor: 10,
                },
            )
            .expect("w");
            // The overlap: broadcast before the snapshot, delivered after it (the writer
            // drains replies ahead of the bus, so this ordering is the normal one).
            write_frame(&mut daemon, &at("pane-1", "SEED-tail", 10)).expect("w");
            write_frame(&mut daemon, &at("pane-1", "older", 4)).expect("w");
            // Strictly past the seed → genuinely new output.
            write_frame(&mut daemon, &at("pane-1", "NEW", 13)).expect("w");
            write_frame(
                &mut daemon,
                &DaemonMsg::Event(SessionEvent::Exit {
                    uid: "pane-1".into(),
                    code: 0,
                }),
            )
            .expect("w");
            daemon
        });

        let (mut att, seed) = Attachment::open(client, "pane-1").expect("open");
        assert_eq!(seed, "SEED");
        let mut out = Vec::new();
        assert_eq!(att.pump_output(&mut out).expect("pump"), PumpEnd::Exited(0));
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "NEW",
            "only output past the seed's cursor reaches the terminal"
        );
        let _ = fake.join();
    }

    // A repaint re-seeds the whole screen, so it moves the splice point too — otherwise the
    // bus backlog it just overwrote gets painted again underneath it.
    #[cfg(unix)]
    #[test]
    fn a_repaint_moves_the_splice_point_to_its_own_snapshot() {
        let (client, mut daemon) = pair();
        let mut att = attachment_over(client, "pane-1", b"<CLR>");
        write_frame(
            &mut daemon,
            &DaemonMsg::Replay {
                uid: "pane-1".into(),
                data: "REDRAWN".into(),
                cursor: 40,
            },
        )
        .expect("w");
        write_frame(&mut daemon, &at("pane-1", "already-drawn", 40)).expect("w");
        write_frame(&mut daemon, &at("pane-1", "after", 45)).expect("w");
        // A stale repaint must never rewind the splice point past output already dropped.
        write_frame(
            &mut daemon,
            &DaemonMsg::Replay {
                uid: "pane-1".into(),
                data: "".into(),
                cursor: 5,
            },
        )
        .expect("w");
        write_frame(&mut daemon, &at("pane-1", "stale", 20)).expect("w");
        write_frame(
            &mut daemon,
            &DaemonMsg::Event(SessionEvent::Exit {
                uid: "pane-1".into(),
                code: 0,
            }),
        )
        .expect("w");

        let mut out = Vec::new();
        assert_eq!(att.pump_output(&mut out).expect("pump"), PumpEnd::Exited(0));
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "<CLR>REDRAWNafter<CLR>"
        );
    }

    // Against a daemon that predates `DaemonMsg::Replay::cursor`, every cursor on the wire is
    // the `0` sentinel. The filter must degrade to "forward everything" rather than mistake
    // live output for seeded output and show a frozen pane.
    #[cfg(unix)]
    #[test]
    fn an_uncursored_daemon_still_gets_every_byte_forwarded() {
        let (client, mut daemon) = pair();
        let mut att = attachment_over(client, "pane-1", b"");
        write_frame(&mut daemon, &data("pane-1", "one ")).expect("w");
        write_frame(&mut daemon, &data("pane-1", "two")).expect("w");
        write_frame(
            &mut daemon,
            &DaemonMsg::Event(SessionEvent::Exit {
                uid: "pane-1".into(),
                code: 0,
            }),
        )
        .expect("w");
        let mut out = Vec::new();
        assert_eq!(att.pump_output(&mut out).expect("pump"), PumpEnd::Exited(0));
        assert_eq!(String::from_utf8(out).expect("utf8"), "one two");
    }

    #[cfg(unix)]
    #[test]
    fn pump_output_reports_a_closed_connection_as_a_detach_not_an_error() {
        let (client, daemon) = pair();
        let mut att = attachment_over(client, "pane-1", b"");
        drop(daemon);
        let mut out = Vec::new();
        assert_eq!(
            att.pump_output(&mut out).expect("EOF is not an error"),
            PumpEnd::Disconnected
        );
    }

    // The gapless seam: an event broadcast before the `Replay` reply lands must be stashed
    // and replayed AFTER the seed, not dropped and not written out of order.
    #[cfg(unix)]
    #[test]
    fn open_stashes_events_that_arrive_before_the_replay_seed() {
        let (client, mut daemon) = pair();
        let fake = std::thread::spawn(move || {
            // The client's `Attach` request.
            let _ = read_frame::<_, ClientMsg>(&mut daemon).expect("attach req");
            write_frame(&mut daemon, &data("pane-1", "early")).expect("w");
            write_frame(
                &mut daemon,
                &DaemonMsg::Replay {
                    uid: "pane-1".into(),
                    data: "SEED".into(),
                    cursor: 0,
                },
            )
            .expect("w");
            write_frame(&mut daemon, &data("pane-1", "late")).expect("w");
            write_frame(
                &mut daemon,
                &DaemonMsg::Event(SessionEvent::Exit {
                    uid: "pane-1".into(),
                    code: 0,
                }),
            )
            .expect("w");
            daemon
        });

        let (mut att, seed) = Attachment::open(client, "pane-1").expect("open");
        assert_eq!(seed, "SEED");
        let mut out = Vec::new();
        assert_eq!(att.pump_output(&mut out).expect("pump"), PumpEnd::Exited(0));
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "earlylate",
            "the pre-seed event is not lost, and stays ahead of the live stream"
        );
        let _ = fake.join();
    }

    #[cfg(unix)]
    #[test]
    fn pump_input_forwards_keystrokes_as_write_frames_and_stops_on_detach() {
        let (client, mut daemon) = pair();
        let writer = AttachWriter {
            conn: Arc::new(Mutex::new(client)),
            uid: "pane-1".into(),
        };
        let mut filter = DetachFilter::new(DEFAULT_DETACH_PREFIX);
        // "ls\r" then the detach sequence; the trailing "xyz" must never reach the pane.
        let detached =
            pump_input(&b"ls\r\x1cdxyz"[..], &writer, &mut filter).expect("pump_input succeeds");
        assert!(detached, "the detach key ends the pump");
        drop(writer);

        let mut got = Vec::new();
        while let Some(msg) = read_frame::<_, ClientMsg>(&mut daemon).expect("frame") {
            if let ClientMsg::Write { data, .. } = msg {
                got.push(data);
            }
        }
        assert_eq!(
            got.concat(),
            "ls\r",
            "only the pre-detach bytes are written"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pump_input_reassembles_a_multibyte_char_split_across_reads() {
        // A `read` boundary inside a UTF-8 sequence must not become U+FFFD on the wire.
        struct Chunked(Vec<Vec<u8>>);
        impl Read for Chunked {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() {
                    return Ok(0);
                }
                let chunk = self.0.remove(0);
                buf[..chunk.len()].copy_from_slice(&chunk);
                Ok(chunk.len())
            }
        }
        let (client, mut daemon) = pair();
        let writer = AttachWriter {
            conn: Arc::new(Mutex::new(client)),
            uid: "pane-1".into(),
        };
        let mut filter = DetachFilter::new(DEFAULT_DETACH_PREFIX);
        // "é" is 0xC3 0xA9 — split it.
        let input = Chunked(vec![vec![0xC3], vec![0xA9, b'!']]);
        assert!(!pump_input(input, &writer, &mut filter).expect("pump"));
        drop(writer);

        let mut got = String::new();
        while let Some(msg) = read_frame::<_, ClientMsg>(&mut daemon).expect("frame") {
            if let ClientMsg::Write { data, .. } = msg {
                got.push_str(&data);
            }
        }
        assert_eq!(got, "é!");
    }

    #[cfg(unix)]
    #[test]
    fn observe_policy_never_puts_a_resize_on_the_wire() {
        // The guard on the plan's open question: nothing in the input path emits Resize.
        let (client, mut daemon) = pair();
        let writer = AttachWriter {
            conn: Arc::new(Mutex::new(client)),
            uid: "pane-1".into(),
        };
        let mut filter = DetachFilter::new(DEFAULT_DETACH_PREFIX);
        pump_input(&b"hello\x1cd"[..], &writer, &mut filter).expect("pump");
        // …and Request explicitly does.
        writer.request_resize(100, 30).expect("resize");
        drop(writer);

        let mut msgs = Vec::new();
        while let Some(msg) = read_frame::<_, ClientMsg>(&mut daemon).expect("frame") {
            msgs.push(msg);
        }
        let resizes: Vec<_> = msgs
            .iter()
            .filter(|m| matches!(m, ClientMsg::Resize { .. }))
            .collect();
        assert_eq!(
            resizes.len(),
            1,
            "exactly the one explicit request_resize, none from typing"
        );
        assert!(
            !msgs.iter().any(|m| matches!(
                m,
                ClientMsg::Kill { .. } | ClientMsg::KillAll | ClientMsg::Shutdown
            )),
            "an attach client must never kill or shut anything down"
        );
    }
}
