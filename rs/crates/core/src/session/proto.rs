//! The **session-daemon wire protocol** (`docs/session-daemon-plan.md` §"Wire
//! protocol"). The GUI becomes a *client* that attaches to a PTY-owning daemon over a
//! framed Unix-domain-socket (Windows: named-pipe) stream; this module is the language
//! they speak. It is entirely in `core` (no Slint, no GUI types) so the daemon and the
//! protocol round-trips are fully headless-testable.
//!
//! ## Framing
//! Length-prefixed: a `u32` little-endian body length followed by a `serde_json` body.
//! [`write_frame`] / [`read_frame`] work over any blocking [`Read`]/[`Write`], so the
//! same code frames over a UDS, a pipe, or an in-memory pipe in tests. JSON (not
//! bincode) keeps the stream inspectable; the daemon is ours so the modest size cost is
//! fine, and the framing layer is transport-agnostic regardless.
//!
//! ## Why a `SpawnSpec`, not `SpawnOptions`
//! The in-process [`SpawnOptions`](crate::session_manager::SpawnOptions) is NOT made
//! `serde` — its `Integration` field is a wiring-layer concern that the daemon resolves
//! itself, and an `Option<EnvMap>`/integration shape on the wire would be fragile. So
//! the wire carries a small, flat, owned [`SpawnSpec`] with exactly the fields the daemon
//! needs, plus a [`SpawnSpec::into_options`] conversion back to the engine's type. The
//! client fills `uid: None` to let the daemon mint the authoritative uid (see the plan's
//! "uid stability" note).
//!
//! ## Versioning
//! [`PROTO_VER`] rides in the [`ClientMsg::Hello`] / [`DaemonMsg::Hello`] handshake. A
//! mismatch lets the client kill + respawn the daemon (lock-step upgrades — the daemon
//! is ours, no third-party compat burden). M0 only carries the field; M3 acts on it: the
//! client compares its own [`PROTO_VER`] against the daemon's `Hello.proto_ver` reply and,
//! on a mismatch, sends [`ClientMsg::Shutdown`] (tearing the stale daemon down) then
//! respawns a fresh one — see [`daemon_client`](crate::session::daemon_client).

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::session::spawn::EnvMap;
use crate::session_manager::{Integration, SessionEvent, SessionSnapshot, SpawnOptions};

/// Wire-protocol version, bumped on any incompatible change to the message shapes.
/// Carried in the `Hello` handshake so a client can detect a stale daemon (M3 acts on
/// the mismatch; M0 just transports it).
///
/// `2` adds [`ClientMsg::Takeover`] — the daemon live upgrade (M1). A version-1 daemon
/// cannot parse that message and simply drops the connection, which is exactly how the
/// successor detects a pre-takeover incumbent and falls back to the old tear-down.
///
/// `3` adds the **claim registry** (M7): [`ClientMsg::Claim`] / [`ClientMsg::Release`] /
/// [`ClientMsg::ListClaims`], the [`DaemonMsg::ClaimResult`] / [`DaemonMsg::Claims`]
/// answers, the unsolicited [`DaemonMsg::SessionsChanged`] push, and `conn_id` on
/// [`DaemonMsg::Hello`]. The bump matters for the same reason `2` did: a version-2 daemon
/// cannot deserialize `Claim`, and an unknown frame drops the connection — so claim traffic
/// must never reach a pre-3 daemon. Normally the lock-step handshake upgrades the daemon
/// (live takeover, M1) first; when it *cannot* — a stale daemon holding live terminals is
/// driven as it is rather than killed for an upgrade — the client suppresses claim traffic
/// entirely instead (`daemon_client::MIN_CLAIM_DAEMON_VER`). The additions are otherwise
/// purely additive, and `Hello.conn_id` carries `#[serde(default)]` so an older peer's reply
/// still parses (as `0`, the "unknown owner" sentinel).
pub const PROTO_VER: u32 = 3;

/// Hard cap on a single frame body, so a corrupt/hostile length prefix can't make a
/// reader allocate unbounded memory. 64 MiB is far above any real replay/screen payload.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// A serde-clean spawn request carried on the wire — the daemon-facing subset of
/// [`SpawnOptions`]. Flat and owned: no `Integration` (the daemon resolves integration
/// itself), no borrowed data. `uid: None` asks the daemon to mint the authoritative uid.
///
/// `into_options` rebuilds a [`SpawnOptions`] for [`SessionRegistry::create`]. The
/// `integration_*` fields let a client pass already-resolved integration args/env
/// through (the GUI computes these); they fold back into a [`Integration`] only when
/// present, matching the in-process additive-no-op default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    /// The session uid. `None` → the daemon assigns one (`SessionRegistry::mint_uid`).
    #[serde(default)]
    pub uid: Option<String>,
    /// Shell to launch; `None` → the daemon's `session::spawn::default_shell()`.
    #[serde(default)]
    pub shell: Option<String>,
    /// A command to run (shell-wrapped unless `args` is also set). `None` → an
    /// interactive shell.
    #[serde(default)]
    pub command: Option<String>,
    /// Program argv (see [`SpawnOptions::args`] for the direct-vs-interactive semantics).
    #[serde(default)]
    pub args: Option<Vec<String>>,
    pub cwd: Option<String>,
    /// Per-pane env override.
    #[serde(default)]
    pub env: Option<EnvMap>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    /// The owning pane's stable id → `HYPERPANES_PANE_ID`.
    #[serde(default)]
    pub pane_id: Option<String>,
    /// Resolved shell-integration leading args (interactive branch only). Empty → none.
    #[serde(default)]
    pub integration_args: Vec<String>,
    /// Resolved shell-integration env (interactive branch only). Empty → none.
    #[serde(default)]
    pub integration_env: EnvMap,
    /// Path to `control.json` → `HYPERPANES_CONTROL_FILE`. `None` → not injected.
    #[serde(default)]
    pub control_file: Option<String>,
}

impl SpawnSpec {
    /// Convert into the engine's [`SpawnOptions`], using `uid` for the session id. The
    /// daemon supplies the (possibly freshly-minted) uid here; the integration fields
    /// fold into an [`Integration`] only when non-empty (else a plain shell, the
    /// in-process default).
    pub fn into_options(self, uid: String) -> SpawnOptions {
        let integration = if self.integration_args.is_empty() && self.integration_env.is_empty() {
            None
        } else {
            Some(Integration {
                args: self.integration_args,
                env: self.integration_env,
            })
        };
        SpawnOptions {
            uid,
            shell: self.shell,
            args: self.args,
            command: self.command,
            cwd: self.cwd,
            env: self.env,
            cols: self.cols,
            rows: self.rows,
            pane_id: self.pane_id,
            integration,
            control_file: self.control_file,
        }
    }
}

/// A summary of one live session, returned by `ListSessions`. Mirrors the read-path
/// accessors a client shadows locally (`output_bytes`, `last_output_at`, cwd).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub uid: String,
    /// Last sniffed cwd, if any has been reported yet.
    pub cwd: Option<String>,
    /// Monotonic UTF-16 output cursor (`SessionRegistry::output_bytes`).
    pub output_bytes: u64,
    /// Epoch-ms of the last flush, or `None` if nothing has been emitted.
    pub last_output_at: Option<u64>,
    /// Whether the session is still live (always `true` in a `ListSessions` reply; the
    /// field exists so a future cache of dead sessions can be expressed).
    pub alive: bool,
    /// Current pty grid width (`SessionRegistry::dims`). The
    /// [attach client](crate::session::attach) needs it to decide whether its own terminal
    /// can show the pane without clipping — its resize policy is to letterbox at the
    /// desktop's grid rather than reflow it (`docs/mux-backend-plan.md` M2).
    ///
    /// ADDITIVE, `#[serde(default)]`: a daemon that predates the field simply omits it and
    /// the client reads `None` ("grid unknown, don't warn"). Serde ignores unknown fields in
    /// the other direction, so no `PROTO_VER` bump is needed — and the Windows pty-host's
    /// frozen-surface contract (only optional additions) is respected.
    #[serde(default)]
    pub cols: Option<u16>,
    /// Current pty grid height — see [`cols`](Self::cols).
    #[serde(default)]
    pub rows: Option<u16>,
}

/// A request from a client to the daemon. Fire-and-forget for mutators; request/response
/// for `ListSessions` / `Attach` / `RenderScreen` / `Ping`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// pre-existing; deferred per repo lint policy (test.yml)
#[allow(clippy::large_enum_variant)]
pub enum ClientMsg {
    /// Handshake: announce the client's protocol version. The daemon replies with
    /// [`DaemonMsg::Hello`].
    Hello { proto_ver: u32 },
    /// List every live session (→ [`DaemonMsg::Sessions`]).
    ListSessions,
    /// Begin receiving this session's streamed [`SessionEvent`]s on this connection, and
    /// get its replay buffer ONCE to seed a fresh grid (→ [`DaemonMsg::Replay`]).
    Attach { uid: String },
    /// Spawn a new session. `spec.uid` may be `None` to let the daemon assign the uid;
    /// the daemon replies with [`DaemonMsg::Created`] carrying the final uid.
    Create(SpawnSpec),
    /// Write input bytes (as a UTF-8 string, mirroring `SessionRegistry::write`).
    Write { uid: String, data: String },
    /// Resize a session's grid.
    Resize { uid: String, cols: u16, rows: u16 },
    /// Kill one session (silent — the natural-exit event is suppressed).
    Kill { uid: String },
    /// Kill every session.
    KillAll,
    /// Serialize a session's current screen (→ [`DaemonMsg::Screen`]).
    RenderScreen { uid: String },
    /// Liveness probe (→ [`DaemonMsg::Pong`]).
    Ping,
    /// Ask the daemon to **hand its sessions over** to the sender and exit — the daemon
    /// live upgrade (M1, `docs/mux-backend-plan.md`). Unlike [`Shutdown`](Self::Shutdown),
    /// the sessions are NOT killed: the daemon replies on this same socket with one or more
    /// [`HandoffPayload`] messages carrying each session's state, with the pty master
    /// descriptors attached as `SCM_RIGHTS` ancillary data
    /// (`session::handoff`), then unlinks its socket and exits. The successor adopts the
    /// descriptors and rebinds. A shell dies from `SIGHUP` when its *pty closes*, not when
    /// its parent exits, so nothing downstream notices the swap.
    ///
    /// The reply does **not** use the [`write_frame`] framing — descriptor passing needs its
    /// own message-oriented transport — so a connection that sends this must switch to
    /// `handoff::recv_with_fds` and use the socket for nothing else. Sent only by another
    /// daemon of this binary, never by the GUI.
    Takeover,
    /// **Take ownership of `uid` for this connection** (M7). The daemon answers with
    /// [`DaemonMsg::ClaimResult`]: `granted` when the uid was free (or already ours),
    /// otherwise `granted: false` plus the incumbent connection.
    ///
    /// This is the *only* safe way to adopt an orphaned session: the check and the take are
    /// one atomic step inside the daemon, so two windows racing for one orphan produce
    /// exactly one winner. The claim lives as long as this connection does — see
    /// [`claims`](crate::session::claims) for why connection lifetime, not a heartbeat, is
    /// the liveness signal.
    Claim { uid: String },
    /// Give up this connection's claim on `uid` (fire-and-forget). A release from a
    /// connection that does not own the uid is ignored. Sent when a pane closes or moves
    /// out of this process; it is never *required* — dropping the connection releases
    /// everything it held.
    Release { uid: String },
    /// Ask for the current claim table (→ [`DaemonMsg::Claims`]). Rarely needed: the daemon
    /// pushes a fresh [`DaemonMsg::Claims`] snapshot to every connection at connect and on
    /// every change, so a client's claim shadow stays current without polling. Kept for
    /// diagnostics and for a client that wants to resynchronise explicitly.
    ListClaims,
    /// Ask the daemon to **shut down**: kill every session and exit the process cleanly,
    /// releasing the lock + socket (M3). Drives the app's `--kill-daemon` path and the
    /// quit-vs-keep-alive "OFF" branch. The daemon kills its sessions, then exits — so the
    /// connection simply drops (no reply frame; the EOF is the acknowledgement).
    Shutdown,
}

/// One chunk of a [`ClientMsg::Takeover`] response: the state of the sessions whose pty
/// masters ride along as this message's descriptors.
///
/// Chunked because `SCM_RIGHTS` is bounded by the kernel's per-message descriptor limit
/// (see [`handoff::MAX_FDS_PER_MSG`](crate::session::handoff::MAX_FDS_PER_MSG)); each
/// snapshot's `fd_index` addresses the descriptor array of **its own** message. The
/// incumbent always sends at least one chunk, even with zero sessions, so the successor can
/// tell "handed over nothing" apart from "the peer never understood the request".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffPayload {
    pub snapshots: Vec<SessionSnapshot>,
}

/// A message from the daemon to a client: handshake/replies plus the streamed event feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonMsg {
    /// Handshake reply: the daemon's protocol version + its pid (for diagnostics / the
    /// future kill-on-mismatch path), plus the [`ConnId`](crate::session::claims::ConnId)
    /// the daemon assigned to THIS connection.
    ///
    /// `conn_id` is how a client tells its own claims apart from other processes' in a
    /// [`Claims`](Self::Claims) snapshot — an identity the daemon *assigns* rather than one
    /// the client asserts, so no pid is trusted and none has to be peer-credentialed.
    /// `#[serde(default)]` (→ `0`, the "unknown" sentinel) keeps a pre-M7 daemon's reply
    /// parseable.
    Hello {
        proto_ver: u32,
        daemon_pid: u32,
        #[serde(default)]
        conn_id: crate::session::claims::ConnId,
    },
    /// Reply to [`ClientMsg::ListSessions`].
    Sessions(Vec<SessionMeta>),
    /// Reply to [`ClientMsg::Create`]: the (possibly daemon-minted) uid of the new
    /// session, so the client can correlate its request.
    Created { uid: String },
    /// Reply to [`ClientMsg::Attach`]: the session's replay buffer, to seed a fresh grid
    /// exactly once. Empty string when the session has produced nothing yet.
    ///
    /// `cursor` is the session's monotonic UTF-16 output cursor **at the instant the
    /// buffer was snapshotted** (`SessionRegistry::replay_with_cursor` reads the pair under
    /// the replay lock, so it can never be torn). It exists because `Attach` subscribes the
    /// connection to the session's broadcast *before* snapshotting: without it, a chunk
    /// flushed in that window — or one already queued on the bus — is both inside the seed
    /// and delivered as a live [`SessionEvent::Data`], and a client with no mirror of its
    /// own (the M2 attach client) paints it twice. A client splices by dropping every
    /// `Data` whose own `cursor` is `<= ` this one.
    ///
    /// ADDITIVE, `#[serde(default)]`: a daemon that predates the field sends `0`, which
    /// means "not reported" — no real chunk can end at cursor 0, so a client reading 0
    /// simply keeps the old (duplicating) behaviour instead of dropping live output.
    Replay {
        uid: String,
        data: String,
        #[serde(default)]
        cursor: u64,
    },
    /// Reply to [`ClientMsg::RenderScreen`]: the serialized screen, or `None` if the
    /// session is gone.
    Screen { uid: String, text: Option<String> },
    /// A streamed live session event (Data / Cwd / Exit) for any session this connection
    /// has attached to (or all, per the daemon's broadcast policy).
    Event(SessionEvent),
    /// Reply to [`ClientMsg::Ping`].
    Pong,
    /// Reply to [`ClientMsg::Claim`] (M7): whether this connection now owns `uid`, and —
    /// when it does not — which connection does.
    ClaimResult {
        uid: String,
        granted: bool,
        owner: Option<crate::session::claims::ConnId>,
    },
    /// The **whole** claim table (M7). Sent unsolicited to every connection when the daemon
    /// accepts it and again on every change, and as the reply to [`ClientMsg::ListClaims`].
    ///
    /// A full snapshot rather than a delta on purpose: applying it is idempotent, a dropped
    /// or reordered push cannot desync a client's shadow, and the table holds one entry per
    /// visible pane on the machine (tens), so the bandwidth is irrelevant.
    Claims(Vec<crate::session::claims::ClaimInfo>),
    /// The daemon's live session set changed (M7): a session was created, killed, or
    /// adopted. Pushed unsolicited to every connection, carrying the same
    /// [`SessionMeta`] rows a [`ListSessions`](ClientMsg::ListSessions) would return.
    ///
    /// This closes the M5 residual where a client's uid shadow — seeded once by
    /// `ListSessions` at connect and then maintained only from the `Exit` stream plus its
    /// own creates — never learned about sessions *other* clients created, so an orphan
    /// from a window opened after us stayed invisible until reconnect. Like
    /// [`Claims`](Self::Claims) it is a full snapshot, for the same reasons.
    SessionsChanged(Vec<SessionMeta>),
}

/// Write one length-prefixed JSON frame: a `u32` LE body length then the JSON body.
/// Flushes so the peer sees the frame promptly. Errors on serialization or I/O failure.
pub fn write_frame<W: Write>(w: &mut W, msg: &impl Serialize) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len: u32 = body
        .len()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame body exceeds u32"))?;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame body exceeds MAX_FRAME_LEN",
        ));
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one length-prefixed JSON frame written by [`write_frame`]. Returns `Ok(None)` on
/// a clean EOF *before any byte of a frame* (the peer closed between frames); a partial
/// frame (EOF mid-length or mid-body) is an `UnexpectedEof` error. `read_exact` transparently
/// reassembles a frame delivered across multiple reads (partial-read safe).
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    // Distinguish a clean between-frames EOF from a truncated length prefix.
    match read_exact_or_eof(r, &mut len_buf)? {
        ReadEof::Eof => return Ok(None),
        ReadEof::Filled => {}
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame length exceeds MAX_FRAME_LEN",
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?; // mid-body EOF → UnexpectedEof (a truncated frame is an error)
    let msg = serde_json::from_slice(&body).map_err(io::Error::other)?;
    Ok(Some(msg))
}

enum ReadEof {
    /// `buf` was filled completely.
    Filled,
    /// EOF hit before any byte was read (a clean between-frames close).
    Eof,
}

/// Like [`Read::read_exact`] but reports a clean EOF *before the first byte* distinctly
/// from a partial read (EOF after some bytes → `UnexpectedEof`, as a truncated length
/// prefix is a protocol error, not an orderly shutdown).
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<ReadEof> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                return if filled == 0 {
                    Ok(ReadEof::Eof)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "EOF mid length prefix",
                    ))
                };
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(ReadEof::Filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // ---- SpawnSpec → SpawnOptions conversion ----

    #[test]
    fn spawn_spec_into_options_carries_fields_and_uses_passed_uid() {
        let spec = SpawnSpec {
            uid: None, // the daemon will mint; into_options takes the resolved uid
            shell: Some("/bin/zsh".into()),
            command: Some("ls".into()),
            args: None,
            cwd: Some("/tmp".into()),
            env: Some(env(&[("FOO", "bar")])),
            cols: Some(120),
            rows: Some(40),
            pane_id: Some("pane-7".into()),
            integration_args: vec!["-i".into()],
            integration_env: env(&[("HP_SHELL", "1")]),
            control_file: Some("/data/control.json".into()),
        };
        let opts = spec.into_options("s42".into());
        assert_eq!(opts.uid, "s42");
        assert_eq!(opts.shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(opts.command.as_deref(), Some("ls"));
        assert_eq!(opts.cwd.as_deref(), Some("/tmp"));
        assert_eq!(opts.cols, Some(120));
        assert_eq!(opts.pane_id.as_deref(), Some("pane-7"));
        assert_eq!(opts.control_file.as_deref(), Some("/data/control.json"));
        let integ = opts.integration.expect("integration folded in");
        assert_eq!(integ.args, vec!["-i".to_string()]);
        assert_eq!(integ.env.get("HP_SHELL").map(String::as_str), Some("1"));
    }

    #[test]
    fn spawn_spec_without_integration_yields_a_plain_shell() {
        let spec = SpawnSpec::default();
        let opts = spec.into_options("s1".into());
        assert!(
            opts.integration.is_none(),
            "no integration fields → plain shell (no-op default)"
        );
        assert_eq!(opts.uid, "s1");
    }

    // ---- framing round-trips ----

    fn roundtrip_client(msg: &ClientMsg) -> ClientMsg {
        let mut buf = Vec::new();
        write_frame(&mut buf, msg).unwrap();
        let mut cur = Cursor::new(buf);
        read_frame::<_, ClientMsg>(&mut cur).unwrap().unwrap()
    }

    fn roundtrip_daemon(msg: &DaemonMsg) -> DaemonMsg {
        let mut buf = Vec::new();
        write_frame(&mut buf, msg).unwrap();
        let mut cur = Cursor::new(buf);
        read_frame::<_, DaemonMsg>(&mut cur).unwrap().unwrap()
    }

    #[test]
    fn every_client_msg_round_trips() {
        let msgs = [
            ClientMsg::Hello {
                proto_ver: PROTO_VER,
            },
            ClientMsg::ListSessions,
            ClientMsg::Attach { uid: "s1".into() },
            ClientMsg::Create(SpawnSpec {
                command: Some("echo hi".into()),
                ..Default::default()
            }),
            ClientMsg::Write {
                uid: "s1".into(),
                data: "ls\n".into(),
            },
            ClientMsg::Resize {
                uid: "s1".into(),
                cols: 100,
                rows: 30,
            },
            ClientMsg::Kill { uid: "s1".into() },
            ClientMsg::KillAll,
            ClientMsg::RenderScreen { uid: "s1".into() },
            ClientMsg::Ping,
            ClientMsg::Shutdown,
        ];
        for m in &msgs {
            assert_eq!(&roundtrip_client(m), m);
        }
    }

    #[test]
    fn every_daemon_msg_round_trips() {
        let msgs = [
            DaemonMsg::Hello {
                proto_ver: PROTO_VER,
                daemon_pid: 4242,
                conn_id: 7,
            },
            DaemonMsg::Sessions(vec![SessionMeta {
                uid: "s1".into(),
                cwd: Some("/home/me".into()),
                output_bytes: 12,
                last_output_at: Some(1000),
                alive: true,
                cols: Some(120),
                rows: Some(40),
            }]),
            DaemonMsg::Created { uid: "s9".into() },
            DaemonMsg::Replay {
                uid: "s1".into(),
                data: "recent output".into(),
                cursor: 13,
            },
            DaemonMsg::Screen {
                uid: "s1".into(),
                text: Some("clean screen".into()),
            },
            DaemonMsg::Event(SessionEvent::Data {
                uid: "s1".into(),
                data: "hi".into(),
                cursor: 2,
            }),
            DaemonMsg::Event(SessionEvent::Cwd {
                uid: "s1".into(),
                cwd: "/tmp".into(),
            }),
            DaemonMsg::Event(SessionEvent::Exit {
                uid: "s1".into(),
                code: 0,
            }),
            DaemonMsg::Pong,
            DaemonMsg::ClaimResult {
                uid: "s1".into(),
                granted: false,
                owner: Some(3),
            },
            DaemonMsg::Claims(vec![crate::session::claims::ClaimInfo {
                uid: "s1".into(),
                owner: 3,
            }]),
            DaemonMsg::SessionsChanged(vec![SessionMeta {
                uid: "s2".into(),
                cwd: None,
                output_bytes: 0,
                last_output_at: None,
                alive: true,
                cols: Some(120),
                rows: Some(34),
            }]),
        ];
        for m in &msgs {
            assert_eq!(&roundtrip_daemon(m), m);
        }
    }

    // The grid fields the attach client reads are ADDITIVE: a daemon that predates them
    // omits them, and that payload must still parse (as "grid unknown") rather than making
    // `ListSessions` fail against an older peer. This is the frozen-host-surface contract.
    #[test]
    fn session_meta_parses_a_payload_with_no_grid_fields() {
        let legacy =
            r#"{"uid":"s1","cwd":null,"output_bytes":7,"last_output_at":null,"alive":true}"#;
        let meta: SessionMeta = serde_json::from_str(legacy).expect("legacy SessionMeta parses");
        assert_eq!(meta.uid, "s1");
        assert_eq!(meta.output_bytes, 7);
        assert_eq!(meta.cols, None, "an unreported grid is None, not a default");
        assert_eq!(meta.rows, None);
    }

    // `Replay.cursor` is ADDITIVE for the same reason: a daemon that predates it omits the
    // key, and the payload must still parse. `0` is the "not reported" sentinel — no real
    // chunk ends at cursor 0 — so an attach client reading it disables its splice filter
    // instead of mistaking live output for already-seeded output.
    #[test]
    fn replay_parses_a_payload_with_no_cursor() {
        let legacy = r#"{"Replay":{"uid":"s1","data":"recent output"}}"#;
        let msg: DaemonMsg = serde_json::from_str(legacy).expect("legacy Replay parses");
        assert_eq!(
            msg,
            DaemonMsg::Replay {
                uid: "s1".into(),
                data: "recent output".into(),
                cursor: 0,
            }
        );
    }

    #[test]
    fn the_hello_handshake_carries_the_version_field() {
        // The version must survive the wire so a client can detect a stale daemon.
        let DaemonMsg::Hello {
            proto_ver,
            daemon_pid,
            conn_id,
        } = roundtrip_daemon(&DaemonMsg::Hello {
            proto_ver: PROTO_VER,
            daemon_pid: 77,
            conn_id: 5,
        })
        else {
            panic!("expected Hello");
        };
        assert_eq!(proto_ver, PROTO_VER);
        assert_eq!(daemon_pid, 77);
        assert_eq!(conn_id, 5);
    }

    /// `Hello.conn_id` is `#[serde(default)]` so a pre-M7 daemon's two-field reply still
    /// parses - it lands on `0`, the "unknown owner" sentinel the claim registry never mints.
    #[test]
    fn a_pre_m7_hello_without_conn_id_still_parses() {
        let body = br#"{"Hello":{"proto_ver":2,"daemon_pid":9}}"#;
        let msg: DaemonMsg = serde_json::from_slice(body).expect("legacy Hello parses");
        assert!(matches!(
            msg,
            DaemonMsg::Hello {
                proto_ver: 2,
                daemon_pid: 9,
                conn_id: 0
            }
        ));
    }

    /// The claim messages must survive the wire in both directions - the whole
    /// no-double-adoption path runs over them.
    #[test]
    fn claim_messages_round_trip() {
        for m in [
            ClientMsg::Claim { uid: "s1".into() },
            ClientMsg::Release { uid: "s1".into() },
            ClientMsg::ListClaims,
        ] {
            assert_eq!(roundtrip_client(&m), m);
        }
    }

    // ---- framing edge cases ----

    #[test]
    fn multiple_frames_read_back_in_order_from_one_stream() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ClientMsg::Ping).unwrap();
        write_frame(&mut buf, &ClientMsg::ListSessions).unwrap();
        write_frame(&mut buf, &ClientMsg::Kill { uid: "s2".into() }).unwrap();
        let mut cur = Cursor::new(buf);
        assert_eq!(
            read_frame::<_, ClientMsg>(&mut cur).unwrap(),
            Some(ClientMsg::Ping)
        );
        assert_eq!(
            read_frame::<_, ClientMsg>(&mut cur).unwrap(),
            Some(ClientMsg::ListSessions)
        );
        assert_eq!(
            read_frame::<_, ClientMsg>(&mut cur).unwrap(),
            Some(ClientMsg::Kill { uid: "s2".into() })
        );
        // A clean EOF between frames returns None (peer closed).
        assert_eq!(read_frame::<_, ClientMsg>(&mut cur).unwrap(), None);
    }

    /// A `Read` that hands out at most `chunk` bytes per call, to prove `read_frame`
    /// reassembles a frame delivered across many short reads (partial-read safety — the
    /// real socket does exactly this).
    struct DribbleReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }
    impl Read for DribbleReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let n = self.chunk.min(buf.len()).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn read_frame_reassembles_across_partial_reads() {
        let mut buf = Vec::new();
        let msg = DaemonMsg::Replay {
            uid: "s1".into(),
            data: "a".repeat(500),
            cursor: 500,
        };
        write_frame(&mut buf, &msg).unwrap();
        // One byte at a time: the length prefix AND the body both arrive piecemeal.
        let mut r = DribbleReader {
            data: buf,
            pos: 0,
            chunk: 1,
        };
        assert_eq!(read_frame::<_, DaemonMsg>(&mut r).unwrap(), Some(msg));
        // Stream exhausted → clean EOF.
        assert_eq!(read_frame::<_, DaemonMsg>(&mut r).unwrap(), None);
    }

    #[test]
    fn a_truncated_length_prefix_is_an_error_not_a_clean_eof() {
        // Two bytes of a 4-byte length, then EOF: a truncated frame is a protocol error.
        let mut r = Cursor::new(vec![0x10u8, 0x00]);
        let err = read_frame::<_, ClientMsg>(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_truncated_body_is_an_unexpected_eof() {
        // A valid length prefix promising more body than is present.
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(b"only a few bytes");
        let mut r = Cursor::new(buf);
        let err = read_frame::<_, ClientMsg>(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn an_oversized_length_prefix_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME_LEN + 1).to_le_bytes());
        let mut r = Cursor::new(buf);
        let err = read_frame::<_, ClientMsg>(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
