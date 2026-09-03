//! **`DaemonSessionManager`** (`docs/session-daemon-plan.md` M1) — a daemon-backed
//! implementation of the exact [`SessionManager`](crate::session_manager::SessionManager)
//! surface. Where the in-process manager owns the PTYs directly, this owns a socket to the
//! [session daemon](crate::session::daemon) (which owns the PTYs) and presents the same
//! create/write/resize/kill/replay/render_screen/… API so the GUI's `Arc<SessionManager>`
//! and every call site are untouched (the backend is chosen behind
//! `HYPERPANES_SESSION_DAEMON`, default-on — see [`SessionManager::new_daemon`]).
//!
//! ## Keeping the synchronous API non-blocking
//! The plan's "Keeping `SessionManager`'s synchronous API non-blocking" table is the spec
//! for *how* each method avoids a blocking socket round-trip on the hot path:
//!
//! | Method | Strategy here |
//! | --- | --- |
//! | `has` / `uids` | **Client shadow** — a `HashMap<uid, Shadow>` seeded by `ListSessions` on connect, then maintained from the `Exit` event stream (+ the local `create`). |
//! | `output_bytes` / `last_output_at` / cwd | **Client shadow** — every `Data`/`Cwd` event the reader sees updates the shadow; reads are a plain map lookup. |
//! | `replay(uid)` | **Client mirror buffer** — a per-uid rolling [`Replay`] grown by `Data` events → a local return, no round-trip. Seeded ONCE from the `Attach` reply on (re)connect so a survivor's history is restored. |
//! | `render_screen(uid)` | **Bounded request/response** (`RenderScreen` → `Screen`). Off the hot path (control-API screen reads only), so a short blocking round-trip is fine. |
//! | `create` / `write` / `resize` / `kill` / `kill_all` | **Fire-and-forget** request (no reply awaited). |
//!
//! Net: the GUI tick/render loop and every shadow read are pure in-memory map lookups; the
//! only blocking I/O is the rare `render_screen` and the one-time reconnect `Attach`.
//!
//! ## uid ownership
//! The GUI passes its own `uid` in [`SpawnOptions`], so `create` PINS it in the wire
//! [`SpawnSpec`] (the daemon honors a pinned uid). That keeps the shadow + mirror keyed
//! immediately and the uid stable across this manager's lifetime — the GUI never has to
//! wait for the daemon's `Created` reply to know its uid. (The daemon's mint-a-uid path is
//! for clients that leave `uid: None`; the GUI doesn't.)
//!
//! ## Reader thread
//! One background thread owns the read half of the socket. It demultiplexes inbound
//! [`DaemonMsg`]s: streamed `Event`s update the shadow + mirror and are forwarded verbatim
//! to the GUI's existing `UnboundedSender<SessionEvent>` (so the renderer is fed exactly as
//! the in-process path feeds it); request/response replies (`Sessions` / `Replay` /
//! `Screen` / `Hello` / `Pong` / `Created`) go to a reply channel a waiting caller drains.
//!
//! ## Portability
//! Everything above is platform-neutral: the connection is a
//! [`transport::Conn`](crate::session::transport::Conn) — a `UnixStream` on unix, a
//! named-pipe `File` on Windows — and both are blocking, cloneable and bidirectional, so
//! the write-half-behind-a-mutex plus reader-thread design is shared verbatim. Only
//! [`transport`](crate::session::transport) is cfg'd.
//!
//! One knob is platform-shaped: [`VersionPolicy`]. The GUI→daemon link is `LockStep` (a
//! version mismatch means a stale daemon, which is handed over or torn down); the Windows
//! daemon→pty-host link is `Tolerant`, because the host is an *older build by design* — it
//! outlives daemon upgrades so the ConPTYs it owns are never touched.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use crate::session::build_id;
use crate::session::claims::ConnId;
use crate::session::proto::{read_frame, write_frame, ClientMsg, DaemonMsg, SpawnSpec, PROTO_VER};
use crate::session::replay::Replay;
use crate::session::transport::{self, Conn, Endpoint};
use crate::session_manager::{SessionEvent, SpawnOptions};

/// How long [`render_screen`](DaemonSessionManager::render_screen) waits for the daemon's
/// `Screen` reply before giving up (returning `None`). Generous — a screen serialize is
/// cheap daemon-side — but bounded so a wedged daemon can't hang a control read forever.
const SCREEN_TIMEOUT: Duration = Duration::from_secs(2);

/// Total time [`connect_or_spawn`] will keep retrying the connect after spawning the
/// daemon, before giving up. The daemon binds its socket within a few ms of launch, but a
/// cold `current_exe` start (plus the Tokio runtime build) can take longer, so we allow a
/// comfortable margin with exponential-ish backoff.
const SPAWN_CONNECT_BUDGET: Duration = Duration::from_secs(5);

/// Per-uid client-side shadow of a session's read-path state (the plan's "client shadow"):
/// what `has`/`uids`/`output_bytes`/`last_output_at`/cwd are answered from, plus the
/// `replay` mirror buffer. All maintained from the event stream so reads never touch I/O.
struct Shadow {
    /// Rolling mirror of recent output, grown by `Data` events and seeded once from the
    /// `Attach` reply — the local source for `replay()` (no round-trip).
    replay: Replay,
    /// Monotonic UTF-16 output cursor, mirroring `SessionRegistry::output_bytes`.
    output_bytes: u64,
    /// Epoch-ms of the last `Data` flush, or `None` if nothing seen yet.
    last_output_at: Option<u64>,
    /// Last sniffed cwd (from `Cwd` events), if any.
    cwd: Option<String>,
    /// What the daemon last reported running in this pane's foreground
    /// ([`SessionMeta::foreground`](crate::session::proto::SessionMeta::foreground)).
    /// `None` is "no answer yet", never "nothing is running".
    foreground: Option<String>,
    /// Where that foreground group is
    /// ([`SessionMeta::fg_cwd`](crate::session::proto::SessionMeta::fg_cwd)) — the kernel's
    /// live answer, kept apart from [`cwd`](Self::cwd), which is the last one a shell
    /// *reported*.
    fg_cwd: Option<String>,
    /// The pty's grid `(cols, rows)` as the daemon last reported it
    /// ([`SessionMeta::cols`](crate::session::proto::SessionMeta::cols)).
    ///
    /// Mirrored here because a re-attaching client has to render this session's *retained
    /// replay*, and that byte stream is full of cursor-positioning sequences the pty wrote
    /// for the width it had at the time. Replaying it into a grid of some other width lands
    /// those moves in the wrong columns, so the scrollback comes back mangled. The seed has
    /// to happen at the daemon's width, then reflow.
    ///
    /// `None` = unknown (a daemon predating `SessionMeta::cols`, or no snapshot seen yet):
    /// the caller keeps whatever size it would have used anyway.
    dims: Option<(u16, u16)>,
    /// **M7.** This shadow was inserted *locally* (by [`DaemonSessionManager::create`]) and
    /// the daemon has not yet confirmed it in a `SessionsChanged` snapshot. A snapshot may
    /// not delete a pending shadow: the create frame and the snapshot race on the wire, and
    /// a snapshot built before the spawn landed would otherwise erase a session we just
    /// asked for. Cleared the first time the uid appears in any snapshot.
    pending: bool,
}

impl Shadow {
    #[tracing::instrument(level = "debug")]
    fn new() -> Self {
        Self {
            replay: Replay::new(),
            output_bytes: 0,
            last_output_at: None,
            cwd: None,
            foreground: None,
            fg_cwd: None,
            dims: None,
            pending: false,
        }
    }

    /// A shadow inserted optimistically by a local `create`, before the daemon confirms it.
    #[tracing::instrument(level = "debug")]
    fn new_pending() -> Self {
        Shadow {
            pending: true,
            ..Shadow::new()
        }
    }
}

/// A daemon-backed [`SessionManager`](crate::session_manager::SessionManager): same API,
/// but the PTYs live in the [session daemon](crate::session::daemon) so they survive a GUI
/// crash. Owns one socket (a `Mutex`'d write half for requests) plus a reader thread that
/// maintains the shadow/mirror and forwards events to the GUI channel.
pub struct DaemonSessionManager {
    /// The write half of the socket, serialized so concurrent `write`/`resize`/… frames
    /// from different threads never interleave on the wire.
    write_half: Mutex<Conn>,
    /// The per-uid shadow + replay mirror — read by every hot-path accessor, written by the
    /// reader thread (events) and `create` (immediate insert).
    shadows: Arc<Mutex<HashMap<String, Shadow>>>,
    /// Reply channel for request/response messages (`Sessions`/`Replay`/`Screen`/`Hello`/
    /// `Pong`). Held behind a `Mutex` so the whole manager stays `Sync` (a bare
    /// `mpsc::Receiver` is `Send` but `!Sync`, and the axum control server shares the
    /// manager as `Arc<…>: Sync`). The lock doubles as the round-trip serializer — only one
    /// request/response is in flight at a time — which is fine: replies are all rare and off
    /// the hot path (only `render_screen` and the connect-time `ListSessions`/handshake).
    replies: Mutex<Receiver<DaemonMsg>>,
    /// **M7.** The daemon's claim table as last pushed to us: `uid -> owning connection`.
    /// Maintained wholesale from `DaemonMsg::Claims` snapshots by the reader thread, so a
    /// read is a lock and a lookup — no I/O, no round-trip, on the panel's paint path.
    claims: Arc<Mutex<HashMap<String, ConnId>>>,
    /// **M7.** *Our* connection id, as minted by the daemon and reported in its `Hello`.
    /// `0` until the handshake completes (and, against a pre-M7 daemon, forever) — the
    /// sentinel the registry never mints, so a `0` self-id makes every claim read as
    /// somebody else's. That is the safe direction: we decline to adopt rather than
    /// double-adopt.
    conn_id: AtomicU64,
    /// **M7 × the stale-daemon rule.** The `proto_ver` the daemon reported in its `Hello`,
    /// or `0` if it never answered. The client is allowed to be talking to an OLDER daemon
    /// than itself — [`stale_daemon_fallback`] deliberately keeps driving a stale daemon
    /// that holds live terminals rather than killing them for an upgrade — and a daemon
    /// below [`MIN_CLAIM_DAEMON_VER`] cannot deserialize `Claim`/`Release`/`ListClaims`.
    /// An unknown frame makes the daemon **drop the connection**, so sending claim traffic
    /// at one would take the user's terminals off screen. Every claim send is therefore
    /// gated on this; see [`claims_supported`](Self::claims_supported).
    daemon_ver: AtomicU64,
    /// Whether the socket to the daemon is still good. The reader thread is the only thing
    /// that ever learns the daemon has gone — it is the end that sees EOF — and before this
    /// flag existed it learned it and told nobody: it broke out of its loop and every
    /// accessor went on answering out of a shadow map that could no longer change. A pane
    /// stayed `running` forever and a `write` reported success into a closed socket.
    ///
    /// So the reader publishes what it saw. `false` is terminal: this manager owns exactly
    /// one connection and never reconnects, so once the daemon is gone the honest answer to
    /// "is this session live" and "did this input land" is `no` until a new manager is built.
    connected: Arc<AtomicBool>,
    _reader: std::thread::JoinHandle<()>,
}

/// Oldest daemon proto version that understands the M7 claim messages
/// (`Claim`/`Release`/`ListClaims`). Below it, claim traffic is suppressed entirely and the
/// panel degrades to its pre-M7 behaviour: every live session looks unclaimed, adoption is
/// always allowed, and adopting re-attaches rather than stealing (the daemon multiplexes
/// output, so a second viewer is harmless). Silence is the only safe option — an old daemon
/// answers an unknown frame by closing the socket.
const MIN_CLAIM_DAEMON_VER: u32 = 3;

impl DaemonSessionManager {
    /// Connect to the daemon serving `salt`, spawning it (detached) if none is listening,
    /// then start the reader thread and seed the shadow from `ListSessions`. Streamed
    /// events are forwarded to `events` (the GUI's existing channel). The salt is the
    /// user-data dir, exactly as the GUI's single-instance gate and the daemon's own
    /// discovery use it.
    ///
    /// **Build handshake:** the same round-trip also compares the daemon's `build_id`
    /// against ours. A daemon of a different build speaks our protocol perfectly well, so
    /// nothing is broken — but the user launched *this* binary and means its backend to
    /// serve, so we ask for the same live takeover and every session moves across intact.
    /// That is what makes upgrading (or rolling back) either side free of friction: no
    /// `--kill-daemon`, no lost terminals. Attempted at most ONCE per process, so two GUIs
    /// of different builds concede to each other instead of trading the salt forever.
    ///
    /// **Proto-version handshake (M3):** before building the manager, this does a bare
    /// `Hello` round-trip on the raw socket and compares the daemon's `proto_ver` against the
    /// client's [`PROTO_VER`]. On a MISMATCH the running daemon is a stale build of OUR binary
    /// (lock-step upgrades — no third-party compat burden), so we upgrade it in place:
    /// [`hand_over_stale_daemon`] spawns a successor that takes its sessions, descriptors and
    /// all. If that cannot happen, what we do next depends on what the incumbent is holding
    /// ([`stale_daemon_fallback`]) — an EMPTY daemon is torn down and replaced, but one with
    /// live terminals is driven as it is. It is never killed to force an upgrade.
    #[tracing::instrument(level = "debug")]
    pub fn new(events: UnboundedSender<SessionEvent>, salt: &str) -> io::Result<Self> {
        Self::new_with_policy(events, salt, VersionPolicy::LockStep)
    }

    /// Connect to the daemon serving `salt` under an explicit [`VersionPolicy`].
    ///
    /// [`VersionPolicy::LockStep`] is [`new`](Self::new): a version mismatch means a stale
    /// build of our own binary is holding the salt, and it is upgraded (or, failing that, torn
    /// down) before we proceed. [`VersionPolicy::Tolerant`] is the pty-host link on Windows,
    /// where an older peer is the *point* — see that variant's docs.
    #[tracing::instrument(level = "debug")]
    pub fn new_with_policy(
        events: UnboundedSender<SessionEvent>,
        salt: &str,
        policy: VersionPolicy,
    ) -> io::Result<Self> {
        let endpoint = transport::endpoint_for(salt);
        // Up to a couple of respawn rounds: a single mismatch should resolve in one
        // tear-down + respawn; more than that means something is wrong (e.g. two GUIs of
        // different versions fighting), and we just proceed with whatever answers last.
        // A build-driven takeover is attempted AT MOST ONCE per process. It always succeeds
        // against a cooperating incumbent, so a SECOND mismatch means someone else is
        // pulling the salt the other way — two GUIs of different builds, each correctly
        // wanting its own backend. Fighting that produces a takeover loop; conceding
        // produces a daemon of the wrong build, which still speaks our protocol and still
        // holds the user's terminals. We concede, and say so.
        let mut forced_build_upgrade = false;
        for attempt in 0..3 {
            let stream = connect_or_spawn(&endpoint, salt)?;
            match probe_daemon_identity(&stream)? {
                ProtoCheck::Match => return Self::from_stream(stream, events),
                ProtoCheck::BuildMismatch { daemon_build } if policy == VersionPolicy::Tolerant => {
                    // The pty-host being an older build is the whole point of `Tolerant`:
                    // it holds live ConPTYs that Windows gives us no way to move. A build
                    // difference there is expected, not stale.
                    tracing::info!("pty-host build skew (client {}, host {daemon_build}); proceeding — \
                         the host surface is version-stable by contract",
                        build_id::build_id());
                    return Self::from_stream(stream, events);
                }
                ProtoCheck::BuildMismatch { daemon_build } if forced_build_upgrade => {
                    tracing::info!("daemon is build {daemon_build}, not ours ({}), after we already \
                         handed it over once; another client wants it that way — driving \
                         it rather than starting a takeover fight",
                        build_id::build_id());
                    return Self::from_stream(stream, events);
                }
                ProtoCheck::BuildMismatch { daemon_build } => {
                    // Same protocol, different binary: a rebuild, a new install, or the
                    // other half of the dev/installed pair. The user launched THIS build and
                    // means its backend to serve. Take the sessions over rather than kill
                    // anything — the descriptors move, the shells never notice, and that is
                    // what makes upgrading (or rolling back) either side free.
                    tracing::info!("daemon build mismatch (client {}, daemon {daemon_build}); \
                         attempting live takeover (attempt {attempt})",
                        build_id::build_id());
                    forced_build_upgrade = true;
                    drop(stream);
                    if !hand_over_stale_daemon(salt, &endpoint) {
                        // Nothing to fall back to and nothing to fall back FROM: the
                        // protocol matches, so the incumbent is fully drivable. It keeps the
                        // salt and its terminals; we just work with the build that is there.
                        // (Unlike a proto mismatch, there is never a reason to tear this
                        // one down — an upgrade we merely prefer is not worth a session.)
                        tracing::info!("build takeover failed against daemon {daemon_build}; driving it \
                             as-is — the terminals matter more than the upgrade");
                        let stream = connect_or_spawn(&endpoint, salt)?;
                        return Self::from_stream(stream, events);
                    }
                }
                ProtoCheck::Mismatch { daemon_ver } if policy == VersionPolicy::Tolerant => {
                    // Deliberate: the peer is a pty-host from an older build, still holding
                    // live ConPTYs. Replacing it is exactly what we must NOT do — every
                    // terminal in it would die. The host surface is frozen (see
                    // `VersionPolicy::Tolerant`), so an older host is safe to drive.
                    tracing::info!("pty-host proto skew (client {PROTO_VER}, host {daemon_ver}); \
                         proceeding — the host surface is version-stable by contract");
                    return Self::from_stream(stream, events);
                }
                ProtoCheck::Mismatch { daemon_ver } => {
                    // The daemon is a stale version of our own binary. Prefer the LIVE UPGRADE
                    // (M1): spawn a daemon from the new binary and let it take the sessions —
                    // pty masters and all — off the incumbent, which then exits. Every terminal
                    // survives. Only if that does not produce a matching daemon (the incumbent
                    // predates takeover, or is wedged) do we fall back to the old tear-down,
                    // which kills every session.
                    tracing::info!("daemon proto-version mismatch (client {PROTO_VER}, daemon {daemon_ver}); \
                         attempting live takeover (attempt {attempt})");
                    drop(stream);
                    if !hand_over_stale_daemon(salt, &endpoint) {
                        // The takeover did not produce a matching daemon: the incumbent
                        // predates the protocol (proto 1 cannot parse `Takeover`) or is
                        // wedged. Historically we tore it down here — which KILLS every
                        // terminal it holds, the exact loss this milestone exists to
                        // prevent. A daemon with live sessions is never torn down to force
                        // an upgrade; we drive it as it is and leave the upgrade for the
                        // next time it is empty.
                        match stale_daemon_fallback(&endpoint, daemon_ver) {
                            StaleFallback::TearDown => {
                                tracing::info!("takeover failed on an EMPTY daemon; tearing down");
                                tear_down_stale_daemon(&endpoint, salt);
                            }
                            StaleFallback::Drive => {
                                tracing::info!("takeover failed against a daemon holding live sessions \
                                     (daemon {daemon_ver}, client {PROTO_VER}); driving it \
                                     as-is — the terminals matter more than the upgrade");
                                let stream = connect_or_spawn(&endpoint, salt)?;
                                return Self::from_stream(stream, events);
                            }
                            StaleFallback::Refuse => {
                                tracing::info!("daemon {daemon_ver} is below the drivable floor \
                                     {MIN_DRIVABLE_DAEMON_VER} and holds live sessions; \
                                     leaving it alone");
                                return Err(io::Error::new(
                                    io::ErrorKind::Unsupported,
                                    "a daemon too old to drive holds live sessions for this \
                                     salt; it was left running rather than killed",
                                ));
                            }
                        }
                    }
                    // Loop: connect_or_spawn will start a fresh daemon if none is now up.
                }
            }
        }
        // Last resort after exhausting respawns: connect and proceed regardless of version, so
        // a transient mismatch never hard-blocks launch (the GUI still falls back to in-process
        // upstream if even this errors).
        let stream = connect_or_spawn(&endpoint, salt)?;
        Self::from_stream(stream, events)
    }

    /// Build a manager over an already-connected socket — the seam tests use with an
    /// in-process daemon on a temp socket (no spawn/discovery). Sends the `Hello`
    /// handshake, starts the reader, and seeds the shadow from a `ListSessions`.
    #[tracing::instrument(level = "debug")]
    pub fn from_stream(stream: Conn, events: UnboundedSender<SessionEvent>) -> io::Result<Self> {
        let read_half = transport::try_clone(&stream)?;
        let write_half = stream;

        let shadows: Arc<Mutex<HashMap<String, Shadow>>> = Arc::default();
        let claims: Arc<Mutex<HashMap<String, ConnId>>> = Arc::default();
        let (reply_tx, replies) = std::sync::mpsc::channel::<DaemonMsg>();

        // Reader thread: demux inbound frames. Events maintain the shadow + mirror and are
        // forwarded to the GUI channel; replies go to the reply channel.
        let shadows_r = Arc::clone(&shadows);
        let claims_r = Arc::clone(&claims);
        let connected = Arc::new(AtomicBool::new(true));
        let connected_r = Arc::clone(&connected);
        let reader = std::thread::Builder::new()
            .name("hp-daemon-sm-reader".into())
            .spawn(move || {
                reader_loop(
                    read_half,
                    shadows_r,
                    claims_r,
                    events,
                    reply_tx,
                    connected_r,
                )
            })?;

        let mgr = DaemonSessionManager {
            write_half: Mutex::new(write_half),
            shadows,
            replies: Mutex::new(replies),
            claims,
            conn_id: AtomicU64::new(0),
            daemon_ver: AtomicU64::new(0),
            connected,
            _reader: reader,
        };

        // Handshake (M1 transports the version; M3 enforces it) — drains the `Hello` reply
        // so it doesn't sit in front of a later request/response.
        mgr.send(&ClientMsg::Hello {
            proto_ver: PROTO_VER,
        })?;
        // The second `Hello` is the one whose reply we read — and (M7) that reply carries
        // the connection id the daemon minted for this socket, which is how we later tell
        // our own claims apart from another process's. It also carries the daemon's own
        // proto version, which is NOT necessarily ours: the stale-daemon rule keeps us
        // driving an older daemon that holds live terminals, and claim traffic must stay off
        // the wire when it does (see `daemon_ver`).
        if let Some(DaemonMsg::Hello {
            conn_id, proto_ver, ..
        }) = mgr.request(
            ClientMsg::Hello {
                proto_ver: PROTO_VER,
            },
            |m| matches!(m, DaemonMsg::Hello { .. }),
        ) {
            mgr.conn_id.store(conn_id, Ordering::SeqCst);
            mgr.daemon_ver.store(proto_ver as u64, Ordering::SeqCst);
        }

        // Seed the shadow from the daemon's live session set (the "+ one `ListSessions` on
        // connect" half of the has/uids strategy) AND re-attach each survivor so its replay
        // mirror is re-seeded from the daemon's retained buffer (a fresh manager on the same
        // salt — e.g. after a GUI restart — picks the survivors back up). M2 drives the
        // visual re-host on top of this; here we just make the shadow + mirror correct.
        mgr.seed_from_daemon();
        Ok(mgr)
    }

    /// Send one request frame (fire-and-forget at this layer). Used directly for the
    /// no-reply mutators; [`request`](Self::request) wraps it for round-trips.
    #[tracing::instrument(level = "debug", skip_all)]
    fn send(&self, msg: &ClientMsg) -> io::Result<()> {
        let mut w = self.write_half.lock().unwrap();
        write_frame(&mut *w, msg)
    }

    /// Send a request and block (holding the reply-channel lock, which serializes
    /// round-trips) for the first reply matching `want`, up to [`SCREEN_TIMEOUT`]. Streamed
    /// events never reach this channel (the reader routes them elsewhere), so the only
    /// traffic here is replies; `want` still guards against an out-of-order reply from a
    /// prior timed-out round-trip whose answer arrived late.
    #[tracing::instrument(level = "debug", skip_all)]
    fn request(&self, msg: ClientMsg, want: impl Fn(&DaemonMsg) -> bool) -> Option<DaemonMsg> {
        // Holding the receiver lock for the whole round-trip both makes the channel a
        // single consumer at a time and serializes overlapping requests onto one wire turn.
        let replies = self.replies.lock().unwrap();
        // Drain any stale reply left from a prior, timed-out round-trip so it can't be
        // mistaken for this one's answer.
        while replies.try_recv().is_ok() {}
        if self.send(&msg).is_err() {
            return None;
        }
        let deadline = Instant::now() + SCREEN_TIMEOUT;
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            match replies.recv_timeout(remaining) {
                Ok(m) if want(&m) => return Some(m),
                Ok(_) => continue,     // not our reply kind — keep waiting
                Err(_) => return None, // timeout or disconnect
            }
        }
    }

    /// `ListSessions` → insert a shadow for every live uid (preserving any existing mirror),
    /// then `Attach` each so its replay mirror is (re)seeded from the daemon's buffer. Run
    /// once at connect; safe to call again (idempotent per uid).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn seed_from_daemon(&self) {
        let Some(DaemonMsg::Sessions(metas)) = self.request(ClientMsg::ListSessions, |m| {
            matches!(m, DaemonMsg::Sessions(_))
        }) else {
            return;
        };
        {
            let mut shadows = self.shadows.lock().unwrap();
            for meta in &metas {
                let shadow = shadows.entry(meta.uid.clone()).or_insert_with(Shadow::new);
                shadow.output_bytes = meta.output_bytes;
                shadow.last_output_at = meta.last_output_at;
                if meta.cwd.is_some() {
                    shadow.cwd = meta.cwd.clone();
                }
                shadow.foreground = meta.foreground.clone();
                shadow.fg_cwd = meta.fg_cwd.clone();
                shadow.dims = meta.cols.zip(meta.rows);
            }
        }
        // Attach each survivor to (a) subscribe this connection to its live events and (b)
        // seed its replay mirror from the `Attach` reply ONCE (the reader applies it).
        for meta in &metas {
            let _ = self.send(&ClientMsg::Attach {
                uid: meta.uid.clone(),
            });
        }
        // ...then BARRIER on a round-trip before returning. The `Replay` frames those
        // Attaches earn are applied by the reader thread, so without this `connect` returns
        // with every mirror still empty and the caller races it. The GUI's restore path is
        // exactly that caller: it reads `replay(uid)` synchronously to seed each re-attached
        // pane's grid, so a survivor came back blank — the process was still there, its
        // scrollback was not.
        //
        // A `Ping` works as the barrier because the daemon answers one connection's messages
        // in order and the reader applies frames in the order it reads them: by the time
        // `Pong` reaches the reply channel, every preceding `Replay` has already been folded
        // into its shadow. Cheap (one local round-trip, only when there is something to
        // seed) and bounded — a timeout just returns, leaving the old racy behaviour rather
        // than hanging startup.
        if !metas.is_empty() {
            let _ = self.request(ClientMsg::Ping, |m| matches!(m, DaemonMsg::Pong));
        }
    }

    // ---- the SessionManager surface (delegated to over the wire) ----

    /// Spawn a session for `opts`. PINS the GUI-chosen uid in the wire spec, inserts an
    /// empty shadow so `has`/`replay` answer immediately, and fires a `Create` (the daemon
    /// auto-attaches the creator, so every event from the session's birth streams back).
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn create(&self, opts: SpawnOptions) -> io::Result<()> {
        let uid = opts.uid.clone();
        // Insert the shadow up front so a `has(uid)`/`replay(uid)` immediately after create
        // (before any event arrives) is consistent with the in-process path.
        self.shadows
            .lock()
            .unwrap()
            .entry(uid.clone())
            .or_insert_with(Shadow::new_pending);
        let spec = spawn_spec_from(opts);
        self.send(&ClientMsg::Create(spec))?;
        Ok(())
    }

    /// The custom-pty-`factory` variant exists only for in-process tests (a closure can't
    /// cross the socket). The daemon owns real PTYs, so the daemon backend ignores the
    /// factory and spawns a normal session — preserving the public signature without a
    /// meaningless wire form. (No production caller uses `create_with`.)
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn create_with(
        &self,
        opts: SpawnOptions,
        _factory: crate::session_manager::SpawnFn,
    ) -> io::Result<()> {
        self.create(opts)
    }

    /// Whether this manager's socket to the daemon is still good. See the
    /// [`connected`](Self::connected) field: `false` is terminal for this manager.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Mark the connection dead. Called by the reader when it sees EOF, and by any send
    /// that fails — a socket write returning `BrokenPipe` is the same news arriving by the
    /// other end.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn note_disconnected(&self) {
        self.connected.store(false, Ordering::SeqCst);
    }

    /// The error every operation answers with once the daemon is gone. Named so the caller
    /// can hand it to a user unchanged.
    #[tracing::instrument(level = "debug", ret)]
    fn gone() -> io::Error {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "the session daemon is no longer running; its panes are gone",
        )
    }

    /// Whether a session with `uid` is live — answered from the shadow (no I/O).
    ///
    /// A dead daemon holds no sessions, and the shadow cannot know that on its own: nothing
    /// arrives to remove its entries, because the thing that would send the removal is what
    /// died. So the connection is checked first, and the frozen map is not consulted at all.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn has(&self, uid: &str) -> bool {
        self.is_connected() && self.shadows.lock().unwrap().contains_key(uid)
    }

    /// The uids of all live sessions — from the shadow (no I/O). Empty once the daemon is
    /// gone, for the reason on [`has`](Self::has).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn uids(&self) -> Vec<String> {
        if !self.is_connected() {
            return Vec::new();
        }
        self.shadows.lock().unwrap().keys().cloned().collect()
    }

    /// Recent output for a re-attaching view — the client mirror buffer (no round-trip).
    /// `None` for an unknown uid, matching the in-process `replay`.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn replay(&self, uid: &str) -> Option<String> {
        self.shadows
            .lock()
            .unwrap()
            .get(uid)
            .map(|s| s.replay.get().to_string())
    }

    /// The pty's grid `(cols, rows)` as of the daemon's last snapshot — from the shadow
    /// (no I/O). `None` when the daemon has not reported one. Seed a re-attaching grid at
    /// this size before feeding [`replay`](Self::replay); see [`Shadow::dims`].
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn dims(&self, uid: &str) -> Option<(u16, u16)> {
        self.shadows.lock().unwrap().get(uid).and_then(|s| s.dims)
    }

    /// Monotonic UTF-16 output cursor — from the shadow (no I/O).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn output_bytes(&self, uid: &str) -> Option<u64> {
        self.shadows
            .lock()
            .unwrap()
            .get(uid)
            .map(|s| s.output_bytes)
    }

    /// Replay + cursor as an atomic pair — one shadows lock, so the pair can't tear
    /// (`apply_event_to_shadow` updates both under the same lock).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn replay_with_cursor(&self, uid: &str) -> Option<(String, u64)> {
        self.shadows
            .lock()
            .unwrap()
            .get(uid)
            .map(|s| (s.replay.get().to_string(), s.output_bytes))
    }

    /// Epoch-ms of the last output flush — from the shadow (no I/O); `None` if nothing
    /// has flushed yet, mirroring the in-process accessor.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn last_output_at(&self, uid: &str) -> Option<u64> {
        self.shadows
            .lock()
            .unwrap()
            .get(uid)
            .and_then(|s| s.last_output_at)
    }

    /// What the daemon last reported running in this pane's foreground — from the shadow
    /// (no I/O). The daemon pushes a fresh snapshot whenever an answer changes, so this is
    /// current without anyone polling the socket.
    ///
    /// `None` is "no answer" (an unknown uid, a daemon that predates the field, a platform
    /// with no foreground group), never "nothing is running".
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn foreground_name(&self, uid: &str) -> Option<String> {
        self.shadows
            .lock()
            .unwrap()
            .get(uid)
            .and_then(|s| s.foreground.clone())
    }

    /// Where that foreground program is — same shadow, same freshness guarantee, and the
    /// same meaning for `None`. The daemon owns the pty, so it is the only process that can
    /// ask the kernel; this is how the GUI reads the answer.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn foreground_cwd(&self, uid: &str) -> Option<String> {
        self.shadows
            .lock()
            .unwrap()
            .get(uid)
            .and_then(|s| s.fg_cwd.clone())
    }

    /// Serialize the pane's current screen — a bounded `RenderScreen`/`Screen` round-trip
    /// (off the hot path). `None` on an unknown uid, a gone session, or a timeout.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn render_screen(&self, uid: &str) -> Option<String> {
        let want_uid = uid.to_string();
        let reply = self.request(
            ClientMsg::RenderScreen {
                uid: uid.to_string(),
            },
            move |m| matches!(m, DaemonMsg::Screen { uid: u, .. } if *u == want_uid),
        )?;
        match reply {
            DaemonMsg::Screen { text, .. } => text,
            _ => None,
        }
    }

    /// Write input to the pane's pty.
    ///
    /// This used to be fire-and-forget, and the `io::Error` a dead socket already produced
    /// was dropped on the floor — so a control-API caller typing into a pane whose daemon
    /// had exited got a `200 {"ok": true}` for keystrokes nobody received. The error is
    /// reported now.
    ///
    /// Two failure shapes, and only one of them is visible here. The daemon *gone* is: the
    /// reader saw EOF, or this send gets `BrokenPipe` — both end up as an `Err`. The daemon
    /// *wedged* — alive, not reading — is not: the bytes fit in the socket's send buffer and
    /// the write succeeds locally. Detecting that needs a round-trip, which does not belong
    /// on a per-keystroke path.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn write(&self, uid: &str, data: &str) -> io::Result<()> {
        if !self.is_connected() {
            return Err(Self::gone());
        }
        self.send(&ClientMsg::Write {
            uid: uid.to_string(),
            data: data.to_string(),
        })
        .inspect_err(|_| self.note_disconnected())
    }

    /// Resize the pane — fire-and-forget.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn resize(&self, uid: &str, cols: u16, rows: u16) {
        let _ = self.send(&ClientMsg::Resize {
            uid: uid.to_string(),
            cols,
            rows,
        });
    }

    /// Kill the pane — fire-and-forget — and forget its shadow locally (the daemon
    /// suppresses the natural-exit event for a deliberate kill, so no `Exit` will arrive to
    /// drop it; we drop it here to keep `has`/`uids` correct immediately, mirroring the
    /// in-process `kill` which removes the session synchronously).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn kill(&self, uid: &str) {
        self.shadows.lock().unwrap().remove(uid);
        let _ = self.send(&ClientMsg::Kill {
            uid: uid.to_string(),
        });
    }

    /// Kill every pane — fire-and-forget — and clear the local shadow.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn kill_all(&self) {
        self.shadows.lock().unwrap().clear();
        let _ = self.send(&ClientMsg::KillAll);
    }

    /// Ask the daemon to **shut down** (kill its sessions + exit): the quit-vs-keep-alive
    /// "OFF" branch and the `--kill-daemon` path. Fire-and-forget — the daemon exits without
    /// a reply frame, so the connection just drops (the EOF is the acknowledgement). Clears
    /// the local shadow so a subsequent accessor sees no sessions. No-op-safe: if the daemon
    /// is already gone the send simply fails and is ignored.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn shutdown_daemon(&self) {
        self.shadows.lock().unwrap().clear();
        let _ = self.send(&ClientMsg::Shutdown);
    }

    // ---- M7: the cross-process claim surface ----

    /// This connection's daemon-assigned [`ConnId`], or `0` if the handshake never produced
    /// one (a pre-M7 daemon). See the [`conn_id`](Self::conn_id) field.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn conn_id(&self) -> ConnId {
        self.conn_id.load(Ordering::SeqCst)
    }

    /// Whether the daemon on the other end understands the M7 claim messages at all.
    ///
    /// This is *not* a formality. A client of this build can legitimately be driving an
    /// OLDER daemon: [`stale_daemon_fallback`] refuses to kill a stale daemon that holds
    /// live terminals, so a proto-1 or proto-2 daemon full of the user's shells is driven
    /// as it is. Such a daemon cannot deserialize `Claim` — and the daemon's reader answers
    /// an undecodable frame by closing the connection, which would take every one of those
    /// terminals off screen to satisfy a bookkeeping message. So when this is false, every
    /// claim send is skipped and the panel falls back to pre-M7 behaviour (nothing looks
    /// claimed; adoption always allowed and harmless, since it re-attaches to a multiplexed
    /// session rather than stealing it).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn claims_supported(&self) -> bool {
        self.daemon_ver.load(Ordering::SeqCst) >= MIN_CLAIM_DAEMON_VER as u64
    }

    /// **Take responsibility for `uid`, or lose the race.** A blocking `Claim`/`ClaimResult`
    /// round-trip: `true` means the daemon's registry recorded *this* connection as the
    /// owner, `false` means somebody else already holds it (or the daemon did not answer).
    ///
    /// This is the no-double-adoption gate. Two windows racing to adopt one orphan both send
    /// `Claim`; the daemon's [`ClaimRegistry::claim`] is a compare-and-set under a mutex, so
    /// exactly one of them is told `granted`. A caller that gets `false` must not adopt.
    ///
    /// Failing closed on a timeout/dropped daemon is deliberate: declining to adopt is
    /// recoverable (click again), adopting a session another window is showing is not.
    ///
    /// [`ClaimRegistry::claim`]: crate::session::claims::ClaimRegistry::claim
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn claim(&self, uid: &str) -> bool {
        // Against a pre-M7 daemon there is no registry to arbitrate, and asking would drop
        // the connection. Answer `true`: that is the M5 behaviour this build replaces, and
        // it is the safe direction here — no registry means no other process's claim can be
        // violated, and an adopt is a re-attach to a multiplexed session, not a theft.
        if !self.claims_supported() {
            return true;
        }
        let want = uid.to_string();
        let reply = self.request(
            ClientMsg::Claim {
                uid: uid.to_string(),
            },
            move |m| matches!(m, DaemonMsg::ClaimResult { uid: u, .. } if *u == want),
        );
        match reply {
            Some(DaemonMsg::ClaimResult { granted, .. }) => granted,
            _ => false,
        }
    }

    /// Announce a claim on `uid` **without waiting for the answer**. For sessions this
    /// process just created, where the outcome is not in doubt and the caller is the GUI
    /// pump — a blocking round-trip there would put a wedged daemon on the frame path.
    /// Contested acquisition (adoption) must use [`claim`](Self::claim) instead and honour
    /// its answer.
    ///
    /// The daemon still replies with a `ClaimResult`; it lands in the reply channel and is
    /// skipped by the next round-trip's filter, which drains stale replies before waiting.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn announce_claim(&self, uid: &str) {
        if !self.claims_supported() {
            return; // see `claims_supported`: silence, not a dropped connection
        }
        let _ = self.send(&ClientMsg::Claim {
            uid: uid.to_string(),
        });
    }

    /// Give up our claim on `uid` — fire-and-forget; the daemon ignores a release from a
    /// connection that does not own it. Not required for correctness on exit (the daemon
    /// drops every claim of a connection when its socket closes), only for a window that
    /// stays alive after closing a pane.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn release(&self, uid: &str) {
        if !self.claims_supported() {
            return; // see `claims_supported`
        }
        let _ = self.send(&ClientMsg::Release {
            uid: uid.to_string(),
        });
    }

    /// The uids currently claimed by **some other connection** — i.e. panes that a different
    /// hyperpanes process (or a different window sharing this daemon connection's process,
    /// which cannot happen today) is responsible for. Read from the pushed claim snapshot:
    /// no I/O.
    ///
    /// Claims owned by *us* are excluded, so a window's own panes never show up as
    /// "somebody else's". If we never learned our own conn id (`0`), every claim counts as
    /// somebody else's — see the [`conn_id`](Self::conn_id) field for why that is the safe
    /// direction.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn claims_held_elsewhere(&self) -> HashSet<String> {
        let me = self.conn_id.load(Ordering::SeqCst);
        self.claims
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, &owner)| owner != me)
            .map(|(uid, _)| uid.clone())
            .collect()
    }

    /// Force a fresh `Claims` snapshot from the daemon. The daemon pushes one on every
    /// change already, so this is only for tests and for a caller that wants a synchronous
    /// barrier; the reply is intercepted by the reader thread, not returned here.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn request_claims(&self) {
        if !self.claims_supported() {
            return; // see `claims_supported`
        }
        let _ = self.send(&ClientMsg::ListClaims);
    }
}

/// The reader thread body: decode inbound frames forever, demuxing events (which update the
/// shadow/mirror and forward to the GUI channel) from replies (which go to the reply
/// channel). Exits on EOF, a socket error, or a dropped GUI channel.
#[tracing::instrument(level = "debug", skip_all)]
fn reader_loop(
    read_half: Conn,
    shadows: Arc<Mutex<HashMap<String, Shadow>>>,
    claims: Arc<Mutex<HashMap<String, ConnId>>>,
    events: UnboundedSender<SessionEvent>,
    replies: Sender<DaemonMsg>,
    connected: Arc<AtomicBool>,
) {
    let mut r = read_half;
    loop {
        match read_frame::<_, DaemonMsg>(&mut r) {
            Ok(Some(DaemonMsg::Event(ev))) => {
                apply_event_to_shadow(&shadows, &ev);
                // Forward verbatim to the renderer. A send error means the GUI dropped its
                // receiver (shutting down) — stop reading.
                if events.send(ev).is_err() {
                    break;
                }
            }
            // `cursor` is for a mirror-less client splicing the live stream onto the seed
            // (`session::attach`); the GUI has its own shadow and instead refuses to seed a
            // non-empty one, which covers the same overlap.
            Ok(Some(DaemonMsg::Replay { uid, data, cursor })) => {
                // The one-shot replay seed from an `Attach`: prime the mirror from the
                // daemon's retained buffer so a re-attaching view restores history. A `Data`
                // chunk can race ahead of this frame (the daemon attaches before it snapshots
                // the buffer), so the splice keeps whatever the mirror appended past
                // `cursor` and puts the replay in front of it.
                if !data.is_empty() {
                    let mut shadows = shadows.lock().unwrap();
                    let shadow = shadows.entry(uid).or_insert_with(Shadow::new);
                    splice_replay(shadow, cursor, &data);
                }
            }
            // **M7 push traffic.** `Claims` and `SessionsChanged` are *unsolicited* full
            // snapshots the daemon sends to every connection whenever the machine-wide
            // picture changes. They are folded into local state here and deliberately never
            // forwarded to `replies`: an unsolicited frame landing in the reply channel
            // would sit in front of the next round-trip's answer.
            Ok(Some(DaemonMsg::Claims(list))) => {
                let mut claims = claims.lock().unwrap();
                *claims = list.into_iter().map(|c| (c.uid, c.owner)).collect();
            }
            Ok(Some(DaemonMsg::SessionsChanged(metas))) => {
                reconcile_snapshot(&shadows, &metas);
            }
            // Other replies (Sessions/Screen/Hello/Pong/Created/ClaimResult) → the request
            // channel.
            Ok(Some(reply)) => {
                if replies.send(reply).is_err() {
                    break; // the manager was dropped
                }
            }
            // Clean EOF (daemon closed) or a malformed-frame/socket error → done, and this
            // is the ONLY place the process learns the daemon is gone. Publish it before
            // leaving: `has`, `uids` and `write` all read this flag, and without it they go
            // on answering out of a shadow map that nothing can ever update again.
            //
            // Deliberately not set on the two breaks above: those mean OUR end went away
            // (the GUI dropped the event receiver, the manager was dropped), which says
            // nothing about the daemon.
            Ok(None) => {
                tracing::warn!("session daemon connection closed");
                connected.store(false, Ordering::SeqCst);
                break;
            }
            Err(e) => {
                tracing::warn!(error = %e, "session daemon connection lost");
                connected.store(false, Ordering::SeqCst);
                break;
            }
        }
    }
}

/// Seed a shadow's mirror from an `Attach` replay whose retained text ends at output
/// position `cursor` (UTF-16 units). Any output the mirror already holds BEYOND `cursor`
/// arrived live, racing ahead of the replay frame, and must survive: the result is the
/// replay followed by exactly that tail. Output at or before `cursor` is covered by the
/// replay and is replaced, so a chunk that was mirrored twice never renders twice.
#[tracing::instrument(level = "debug", ret, skip(shadow))]
fn splice_replay(shadow: &mut Shadow, cursor: u64, data: &str) {
    let ahead = shadow.output_bytes.saturating_sub(cursor) as usize;
    let tail = if ahead == 0 {
        ""
    } else {
        utf16_tail(shadow.replay.get(), ahead)
    };
    let mut merged = Replay::new();
    merged.append(data);
    merged.append(tail);
    shadow.replay = merged;
    shadow.output_bytes = shadow.output_bytes.max(cursor);
}

/// The suffix of `s` holding at most `units` UTF-16 code units, cut on a char boundary (a
/// 2-unit char that straddles the cut is dropped, never split).
#[tracing::instrument(level = "debug", ret)]
fn utf16_tail(s: &str, units: usize) -> &str {
    let mut have = 0;
    let mut start = s.len();
    for (idx, ch) in s.char_indices().rev() {
        let u = ch.len_utf16();
        if have + u > units {
            break;
        }
        have += u;
        start = idx;
    }
    &s[start..]
}

/// Fold an unsolicited **full session snapshot** (`DaemonMsg::SessionsChanged`) into the
/// shadow map — the M7 fix for shadow staleness.
///
/// Before M7 the shadow was seeded once by `ListSessions` at connect and then maintained
/// only from the event stream plus local creates, so a session another client created after
/// we connected stayed invisible until we reconnected, and a session another client *killed*
/// left a ghost row (a deliberate `Kill` is suppressed on the event bus).
///
/// The daemon pushes a whole `Vec<SessionMeta>`, not a delta, so applying one is idempotent
/// and a dropped or reordered push cannot desync us. Reconciliation is:
///
/// * a uid in the snapshot we don't have → insert (and clear `pending` on one we do);
/// * a uid we have that the snapshot omits → remove, **unless** it is `pending` (a local
///   create still in flight — see [`Shadow::pending`]).
///
/// Metadata (`output_bytes`/`last_output_at`/`cwd`) is refreshed from the snapshot, but the
/// local replay mirror is never touched: it is grown by the event stream and is the only
/// copy of history this process has.
#[tracing::instrument(level = "debug", ret, skip(shadows))]
fn reconcile_snapshot(
    shadows: &Mutex<HashMap<String, Shadow>>,
    metas: &[crate::session::proto::SessionMeta],
) {
    let mut shadows = shadows.lock().unwrap();
    let live: HashSet<&str> = metas.iter().map(|m| m.uid.as_str()).collect();
    shadows.retain(|uid, sh| live.contains(uid.as_str()) || sh.pending);
    for meta in metas {
        let shadow = shadows.entry(meta.uid.clone()).or_insert_with(Shadow::new);
        shadow.pending = false;
        // A snapshot is built without the event bus's lock, so it can be a few chunks
        // behind the `Data` events already folded here. The cursor is monotonic, so take
        // the later of the two rather than letting a push walk it backwards.
        shadow.output_bytes = shadow.output_bytes.max(meta.output_bytes);
        if meta.last_output_at.is_some() {
            shadow.last_output_at = meta.last_output_at;
        }
        if meta.cwd.is_some() {
            shadow.cwd = meta.cwd.clone();
        }
        // Unconditional, unlike cwd: `None` here is the daemon's honest "no answer", and
        // a reader is required to treat it as such — carrying a stale name forward would
        // outlive the command that produced it.
        shadow.foreground = meta.foreground.clone();
        // Rides with `foreground`: same sample, same tick, same honest `None`.
        shadow.fg_cwd = meta.fg_cwd.clone();
        // Same rule as `foreground`, for the same reason: a daemon that cannot answer says
        // `None`, and a stale grid size is worse than no size — it would seed a replay at a
        // width the pty stopped using.
        shadow.dims = meta.cols.zip(meta.rows);
    }
}

/// Fold one streamed [`SessionEvent`] into the shadow: `Data` grows the mirror + counters,
/// `Cwd` updates the cached cwd, `Exit` drops the session (mirrors the in-process driver
/// removing a session from the map on terminal exit, so `has`/`uids` go false).
#[tracing::instrument(level = "debug", ret, skip(shadows))]
fn apply_event_to_shadow(shadows: &Mutex<HashMap<String, Shadow>>, ev: &SessionEvent) {
    let mut shadows = shadows.lock().unwrap();
    match ev {
        SessionEvent::Data { uid, data, .. } => {
            let shadow = shadows.entry(uid.clone()).or_insert_with(Shadow::new);
            shadow.replay.append(data);
            shadow.output_bytes += data.encode_utf16().count() as u64;
            shadow.last_output_at = Some(epoch_ms());
        }
        SessionEvent::Cwd { uid, cwd } => {
            let shadow = shadows.entry(uid.clone()).or_insert_with(Shadow::new);
            shadow.cwd = Some(cwd.clone());
        }
        SessionEvent::Exit { uid, .. } => {
            shadows.remove(uid);
        }
        // Phase-4 markers ride the proto but the client keeps no marker shadow yet
        // (the in-process backend owns the live liveness mirror). Ignored here; the
        // control server still receives the event stream verbatim for fan-out.
        SessionEvent::CommandStart { .. }
        | SessionEvent::CommandEnd { .. }
        | SessionEvent::PromptReady { .. }
        | SessionEvent::AgentState { .. } => {}
    }
}

/// Epoch-ms now — the client's own `last_output_at` stamp (the daemon's
/// `SessionEvent::Data` doesn't carry a timestamp, and the GUI compares against its own
/// wall clock anyway, exactly as the in-process `last_output_at` is a local stamp).
#[tracing::instrument(level = "debug", ret)]
fn epoch_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Build the wire [`SpawnSpec`] from [`SpawnOptions`]: PIN the uid (the GUI owns it),
/// flatten the integration into the spec's resolved `integration_args`/`integration_env`
/// (the daemon folds them back via [`SpawnSpec::into_options`]), and carry the rest.
#[tracing::instrument(level = "debug", skip_all)]
fn spawn_spec_from(opts: SpawnOptions) -> SpawnSpec {
    let (integration_args, integration_env) = match opts.integration {
        Some(i) => (i.args, i.env),
        None => (Vec::new(), Default::default()),
    };
    SpawnSpec {
        uid: Some(opts.uid),
        shell: opts.shell,
        command: opts.command,
        args: opts.args,
        cwd: opts.cwd,
        env: opts.env,
        cols: opts.cols,
        rows: opts.rows,
        pane_id: opts.pane_id,
        integration_args,
        integration_env,
        control_file: opts.control_file,
    }
}

/// How a client reacts to a daemon whose `proto_ver` is not ours.
///
/// Both peers are always builds of *our own* binary — there is no third-party compatibility
/// burden — so the default is lock-step: the newer side wins and the older one is upgraded in
/// place (or, if it predates the takeover protocol, torn down).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionPolicy {
    /// Version skew is resolved before use: take the incumbent's sessions over (M1) and, only
    /// if that fails, tear it down and respawn. The GUI → daemon link.
    LockStep,
    /// Version skew is accepted and driven as-is. Used for exactly one link — the Windows
    /// daemon → **pty-host** connection — where the peer is a *deliberately* un-upgraded
    /// process: it owns the ConPTYs, which cannot be handed to another process, so it must
    /// outlive daemon upgrades. That only works if the slice of the protocol the daemon uses
    /// against a host stays wire-compatible forever:
    ///
    /// > **Frozen host surface.** `Hello`, `ListSessions`, `Attach`, `Create`, `Write`,
    /// > `Resize`, `Kill`, `KillAll`, `RenderScreen`, `Ping`, `Shutdown` and the
    /// > `Hello`/`Sessions`/`Created`/`Replay`/`Screen`/`Event`/`Pong` replies may gain
    /// > *optional* fields, and `SessionEvent` may gain variants (unknown ones are dropped by
    /// > the receiver), but no existing field may change name, type or meaning.
    ///
    /// Break that and a Windows upgrade drops every terminal in the host — the one failure
    /// this whole design exists to prevent.
    Tolerant,
}

/// Result of the handshake probe ([`probe_daemon_identity`]).
enum ProtoCheck {
    /// The daemon is one we can simply use: its `proto_ver` equals the client's
    /// [`PROTO_VER`] *and* it reports our own build (or no build at all — an older daemon
    /// that predates the field, which never forces anything). Proceed.
    Match,
    /// Same protocol, different build: a rebuild, a fresh install, or the other of the
    /// dev/installed pair. Nothing is broken — we could talk to it — but the user launched
    /// *this* binary and expects its backend, so ask for a live takeover. Every session
    /// survives it, which is why this is worth doing on a merely-different build at all.
    BuildMismatch { daemon_build: String },
    /// The daemon speaks a different version — hand its sessions over + respawn (or, when
    /// that fails and it holds live terminals, drive it as it is).
    Mismatch { daemon_ver: u32 },
}

/// How long the version probe waits for the daemon's `Hello` before giving up.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Do a bare `Hello` round-trip on a freshly-connected (NOT yet manager-owned) connection and
/// compare the daemon's identity to ours: first its `proto_ver` against [`PROTO_VER`], then —
/// when those agree — its `build_id` against [`build_id`](crate::session::build_id). Used by
/// `new` BEFORE the manager + reader thread are built, so a mismatch can be resolved by
/// handing the stale daemon's sessions over and respawning — the lock-step-upgrade contract,
/// since the daemon is our own binary.
///
/// The two checks answer different questions. The protocol version asks *can* we talk to it;
/// the build id asks *is it the binary the user launched*. The second is the one that moves
/// on an ordinary rebuild or install, and it is safe to act on precisely because the answer
/// is a live takeover rather than a kill.
///
/// A handshake that doesn't answer in time is treated as a `Match` (proceed): the gate must
/// never hard-block launch over a slow or odd handshake — the worst case is talking to a
/// daemon we couldn't confirm, which is harmless.
#[tracing::instrument(level = "debug")]
fn probe_daemon_identity(stream: &Conn) -> io::Result<ProtoCheck> {
    let mut w = transport::try_clone(stream)?;
    let send = write_frame(
        &mut w,
        &ClientMsg::Hello {
            proto_ver: PROTO_VER,
        },
    );
    if send.is_err() {
        return Ok(ProtoCheck::Match);
    }
    // Read until the `Hello` reply, skipping frames that are not it. An M7 daemon answers a
    // `Hello` by *also* broadcasting its claim and session snapshots (see the daemon's Hello
    // arm), and those can legitimately overtake the reply on the wire — treating the first
    // frame as the answer would then silently report a version match against a daemon we
    // never actually heard from. `PROBE_FRAMES` bounds the skip so a chatty or wedged peer
    // can't hold launch open.
    const PROBE_FRAMES: usize = 8;
    for _ in 0..PROBE_FRAMES {
        match transport::read_frame_deadline::<DaemonMsg>(stream, PROBE_TIMEOUT) {
            Ok(Some(DaemonMsg::Hello {
                proto_ver,
                build_id: daemon_build,
                ..
            })) => {
                if proto_ver != PROTO_VER {
                    return Ok(ProtoCheck::Mismatch {
                        daemon_ver: proto_ver,
                    });
                }
                // Same protocol. The build id is the finer question: is this daemon the
                // binary the user just launched, or a different build of it? An empty id is
                // a daemon from before the field existed — unknown, and unknown is never a
                // reason to move anything.
                return Ok(if build_id::differs(&daemon_build) {
                    ProtoCheck::BuildMismatch { daemon_build }
                } else {
                    ProtoCheck::Match
                });
            }
            // Unsolicited push traffic (M7) or an unrelated reply — keep looking.
            Ok(Some(_)) => continue,
            // EOF or a (timed-out) read: don't block launch over an unconfirmed handshake —
            // proceed as a match. NB: `from_stream` re-runs its own Hello round-trip,
            // draining the daemon's (second) reply, so this probe's `Hello` reply does not
            // desync the stream the manager later owns.
            Ok(None) | Err(_) => break,
        }
    }
    Ok(ProtoCheck::Match)
}

/// How long to wait for a spawned successor to take the incumbent's sessions and start
/// serving. Covers the incumbent's handoff, its exit, and the successor's own cold start
/// (runtime build + adopting every pty), so it is looser than a plain spawn budget.
const TAKEOVER_BUDGET: Duration = Duration::from_secs(8);

/// Upgrade the daemon at `endpoint` **without killing its sessions**: spawn a daemon from the
/// current (new) binary, which finds the salt's endpoint already held, asks the incumbent to
/// hand every session over, and takes the endpoint once the incumbent exits (see
/// `daemon::take_over` / `daemon::windows::take_over`). Returns whether a daemon that is
/// *ours* — same [`PROTO_VER`] and same build id — is serving by the end of it. Both
/// triggers (proto skew and build skew) poll the same predicate, because both want the same
/// end state: the daemon is this binary.
///
/// `false` is the expected answer against an incumbent that predates the takeover protocol
/// (proto 1): it cannot parse the request and drops the connection, and the successor
/// declines to fight for the endpoint. The caller then asks [`stale_daemon_fallback`] what to
/// do, which tears the incumbent down ONLY if it holds no live session.
#[tracing::instrument(level = "debug", ret)]
fn hand_over_stale_daemon(salt: &str, endpoint: &Endpoint) -> bool {
    tracing::info!("spawning a successor daemon to take the sessions over");
    if let Err(e) = spawn_daemon_detached(salt) {
        tracing::warn!(error = %e, "could not spawn a successor daemon");
        return false;
    }
    let deadline = Instant::now() + TAKEOVER_BUDGET;
    let mut backoff = Duration::from_millis(20);
    loop {
        // Only a version MATCH proves the successor won: while the incumbent still holds the
        // endpoint, connecting succeeds and reports the stale version.
        if let Ok(stream) = transport::connect(endpoint) {
            if matches!(probe_daemon_identity(&stream), Ok(ProtoCheck::Match)) {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(200));
    }
}

/// Oldest daemon proto version a client of THIS build can still drive rather than replace.
///
/// Version 2 added `ClientMsg::Takeover` and nothing else: `DaemonMsg` is byte-identical to
/// version 1 and every request a GUI actually sends (`Attach`/`Create`/`Write`/`Resize`/
/// `Kill`/`RenderScreen`/`ListSessions`) is unchanged. So a v2 client can drive a v1 daemon
/// perfectly; it just cannot upgrade it in place. Raise this floor only when a bump breaks
/// the *base* surface — and note what it costs: below the floor, a daemon holding live
/// sessions is refused, not killed.
///
/// **Version 3 (M7) added `Claim`/`Release`/`ListClaims`, and the floor deliberately did NOT
/// move.** Those are not base surface: they are bookkeeping for the left panel's adoption
/// list, and the client suppresses all of them against a daemon below
/// [`MIN_CLAIM_DAEMON_VER`] (see [`DaemonSessionManager::claims_supported`]). Gating the new
/// traffic is the right trade — raising the floor instead would refuse the connect to a
/// proto-1/2 daemon full of the user's shells and take them off screen, which is the very
/// outcome the fallback exists to prevent.
const MIN_DRIVABLE_DAEMON_VER: u32 = 1;

/// What to do with a stale-version daemon that would not hand its sessions over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaleFallback {
    /// It holds live terminals and its surface is drivable: talk to it as it is. The upgrade
    /// is deferred, which is strictly better than the terminals dying for it.
    Drive,
    /// It holds nothing. Tearing it down costs no session, so take the clean upgrade.
    TearDown,
    /// It holds live terminals and is too old to drive. Leave it running and fail the
    /// connect — the GUI falls back to in-process sessions and the user's terminals live.
    Refuse,
}

/// Decide the fallback for a stale daemon that declined (or could not complete) the takeover.
///
/// The safety rule is one-directional: **we never kill sessions we did not create to force an
/// upgrade.** So "how many live sessions does it hold" is answered conservatively — an
/// unreadable or unanswered `ListSessions` counts as "it holds some", because guessing
/// "empty" wrongly destroys a user's work while guessing "occupied" wrongly only defers an
/// upgrade.
#[tracing::instrument(level = "debug", ret)]
fn stale_daemon_fallback(endpoint: &Endpoint, daemon_ver: u32) -> StaleFallback {
    match live_session_count(endpoint) {
        Some(0) => StaleFallback::TearDown,
        _ if daemon_ver >= MIN_DRIVABLE_DAEMON_VER => StaleFallback::Drive,
        _ => StaleFallback::Refuse,
    }
}

/// Live-session count reported by the daemon at `endpoint`, or `None` if it could not be
/// established (no connect, no `Sessions` reply in time, a reply we could not parse). Opens
/// its own short-lived connection so it never disturbs a stream the manager will own.
#[tracing::instrument(level = "debug", ret)]
fn live_session_count(endpoint: &Endpoint) -> Option<usize> {
    let stream = transport::connect(endpoint).ok()?;
    let mut w = transport::try_clone(&stream).ok()?;
    write_frame(
        &mut w,
        &ClientMsg::Hello {
            proto_ver: PROTO_VER,
        },
    )
    .ok()?;
    write_frame(&mut w, &ClientMsg::ListSessions).ok()?;
    // The `Hello` reply (and any event that races us) precedes the answer we want, so read
    // until `Sessions` shows up or the budget is spent.
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while Instant::now() < deadline {
        match transport::read_frame_deadline::<DaemonMsg>(&stream, PROBE_TIMEOUT) {
            Ok(Some(DaemonMsg::Sessions(metas))) => {
                return Some(metas.iter().filter(|m| m.alive).count())
            }
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
    None
}

/// Tear down a stale-version daemon at `endpoint`: connect, send `Shutdown`, then wait
/// (briefly) for it to stop answering. Best-effort — if the connect fails the daemon is
/// already gone, and if it lingers, the respawn loop in `new` still converges. `salt` is
/// unused today (the endpoint is enough) but kept for symmetry with the spawn side.
#[tracing::instrument(level = "debug", ret)]
fn tear_down_stale_daemon(endpoint: &Endpoint, _salt: &str) {
    if let Ok(stream) = transport::connect(endpoint) {
        tracing::info!("shutting down the stale session daemon");
        let mut w = stream;
        let _ = write_frame(&mut w, &ClientMsg::Shutdown);
        // Wait for the daemon to exit. Bounded so a wedged daemon doesn't hang launch; the
        // respawn loop tolerates a slow teardown.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if !transport::is_live(endpoint) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Connect to the daemon's endpoint; if none is listening, spawn the daemon detached and
/// retry-connect with backoff until [`SPAWN_CONNECT_BUDGET`]. A spawn race — another client
/// just launched the daemon, so OUR spawn loses the endpoint — is NOT an error: we simply
/// keep retrying the connect, since whoever won is now (or soon) listening.
#[tracing::instrument(level = "debug", ret)]
fn connect_or_spawn(endpoint: &Endpoint, salt: &str) -> io::Result<Conn> {
    // Fast path: a daemon is already up.
    if let Ok(s) = transport::connect(endpoint) {
        return Ok(s);
    }

    // None listening → spawn it detached. The daemon is a mode of THIS binary
    // (`current_exe --session-daemon <salt>`), launched so it outlives us and never touches
    // our console (the survival contract — see the plan's "Spawn" note).
    tracing::info!("no session daemon listening; spawning one");
    spawn_daemon_detached(salt)?;

    // Retry-connect with a short, growing backoff until the daemon binds (cold start +
    // runtime build can take a beat). A bind-side race in the daemon we just (maybe
    // redundantly) launched never surfaces here; on the CONNECT side we only ever see
    // "refused"/"not found" until the endpoint is live, which the retry rides out.
    let deadline = Instant::now() + SPAWN_CONNECT_BUDGET;
    let mut backoff = Duration::from_millis(10);
    loop {
        match transport::connect(endpoint) {
            Ok(s) => return Ok(s),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(200));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Path to `systemd-run` on a systemd host, else `None`.
#[cfg(unix)]
#[tracing::instrument(level = "debug", ret)]
fn systemd_run_path() -> Option<std::path::PathBuf> {
    for dir in ["/usr/bin", "/bin", "/usr/local/bin"] {
        let p = std::path::Path::new(dir).join("systemd-run");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Launch `current_exe --session-daemon <salt>` fully detached from the GUI's lifetime AND
/// cgroup. Cgroup independence matters: the GUI may itself run inside a systemd scope (it
/// does after any self-restart — the relauncher uses `systemd-run --scope`), and a daemon
/// merely `setsid`-detached would inherit that scope's cgroup and die when the scope is torn
/// down on the NEXT GUI restart (a chained-restart bug: gui-scope restart would silently
/// kill every pane). So prefer `systemd-run --user --scope` (own transient scope, owned by
/// systemd, survives any GUI teardown); fall back to a bare `setsid` spawn on non-systemd
/// hosts. Either way we drop the handle — the daemon is long-lived and re-parents to init.
#[cfg(unix)]
#[tracing::instrument(level = "debug", ret)]
fn spawn_daemon_detached(salt: &str) -> io::Result<()> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;
    if let Some(sr) = systemd_run_path() {
        let ok = Command::new(sr)
            .args(["--user", "--quiet", "--collect", "--scope", "--"])
            .arg(&exe)
            .arg("--session-daemon")
            .arg(salt)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok();
        if ok {
            return Ok(());
        }
        // systemd-run present but the spawn failed — fall through to the setsid path rather
        // than leaving the caller with no daemon.
    }
    spawn_daemon_setsid(&exe, salt)
}

/// Bare-`setsid` daemon spawn — the non-systemd fallback (and the safety net when
/// `systemd-run` is present but fails to launch). New session so a GUI crash/SIGHUP never
/// reaches it; null stdio; handle dropped so it re-parents to init.
#[cfg(unix)]
#[tracing::instrument(level = "debug", ret)]
fn spawn_daemon_setsid(exe: &std::path::Path, salt: &str) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(exe);
    cmd.arg("--session-daemon")
        .arg(salt)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // `setsid(2)` after fork detaches the child into its own session/process group. Declared
    // inline as a raw libc extern to avoid a direct `libc` dependency for one call — it's an
    // async-signal-safe syscall wrapper with no allocation, which is the bar for `pre_exec`.
    extern "C" {
        fn setsid() -> i32;
    }
    // SAFETY: `setsid` is async-signal-safe (no allocation, no locks); the closure runs in
    // the forked child between `fork` and `exec`, which is exactly where `pre_exec` allows
    // only such calls. A failure just leaves us in the parent's session — harmless.
    unsafe {
        cmd.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
    cmd.spawn().map(|_child| ())
}

/// Windows: launch `current_exe --session-daemon <salt>` detached from this process.
///
/// `DETACHED_PROCESS` gives the daemon no console at all (the unix analog of `setsid` +
/// null stdio: nothing the GUI's console teardown can reach), `CREATE_NEW_PROCESS_GROUP`
/// keeps a console Ctrl-C in the GUI's group from reaching it, and `CREATE_NO_WINDOW` makes
/// sure no window flashes even if a future build gains a console. Windows has no parent-death
/// signal and no cgroup to inherit, so unlike unix there is nothing further to escape: the
/// spawned process is already independent once we drop its handle.
#[cfg(windows)]
#[tracing::instrument(level = "debug", ret)]
fn spawn_daemon_detached(salt: &str) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let exe = std::env::current_exe()?;
    Command::new(exe)
        .arg("--session-daemon")
        .arg(salt)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn()
        .map(|_child| ())
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[tracing::instrument(level = "debug", skip_all)]
    fn env(pairs: &[(&str, &str)]) -> crate::session::spawn::EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn spawn_spec_from_pins_uid_and_flattens_integration() {
        let opts = SpawnOptions {
            uid: "pane-9".into(),
            shell: Some("/bin/zsh".into()),
            command: Some("ls".into()),
            cwd: Some("/tmp".into()),
            integration: Some(crate::session_manager::Integration {
                args: vec!["-i".into()],
                env: env(&[("HP", "1")]),
            }),
            control_file: Some("/c.json".into()),
            ..Default::default()
        };
        let spec = spawn_spec_from(opts);
        assert_eq!(
            spec.uid.as_deref(),
            Some("pane-9"),
            "the GUI's uid is pinned on the wire"
        );
        assert_eq!(spec.command.as_deref(), Some("ls"));
        assert_eq!(spec.integration_args, vec!["-i".to_string()]);
        assert_eq!(
            spec.integration_env.get("HP").map(String::as_str),
            Some("1")
        );
        assert_eq!(spec.control_file.as_deref(), Some("/c.json"));
        // Round-trips back through into_options to the same uid (the daemon honors it).
        assert_eq!(spec.into_options("pane-9".into()).uid, "pane-9");
    }

    #[test]
    fn spawn_spec_from_no_integration_is_a_plain_shell() {
        let spec = spawn_spec_from(SpawnOptions {
            uid: "p1".into(),
            ..Default::default()
        });
        assert!(spec.integration_args.is_empty());
        assert!(spec.integration_env.is_empty());
        assert!(spec.into_options("p1".into()).integration.is_none());
    }

    // ---- shadow folding (no socket needed) ----

    #[tracing::instrument(level = "debug")]
    fn shadows() -> Arc<Mutex<HashMap<String, Shadow>>> {
        Arc::default()
    }

    #[tracing::instrument(level = "debug", ret)]
    fn meta(uid: &str, foreground: Option<&str>) -> crate::session::proto::SessionMeta {
        crate::session::proto::SessionMeta {
            uid: uid.to_string(),
            cwd: None,
            output_bytes: 0,
            last_output_at: None,
            alive: true,
            cols: None,
            rows: None,
            foreground: foreground.map(str::to_string),
            fg_cwd: None,
        }
    }

    /// The daemon owns the pty, so its snapshot is the only place a client can learn what
    /// a pane is running. Folding it must be plain assignment — including back to `None`,
    /// which is how the daemon says "the shell has the terminal again".
    #[test]
    fn a_snapshot_carries_the_foreground_name_in_both_directions() {
        let s = shadows();
        reconcile_snapshot(&s, &[meta("u1", Some("claude"))]);
        assert_eq!(
            s.lock().unwrap().get("u1").unwrap().foreground.as_deref(),
            Some("claude")
        );

        reconcile_snapshot(&s, &[meta("u1", None)]);
        assert_eq!(
            s.lock().unwrap().get("u1").unwrap().foreground,
            None,
            "a stale name would outlive the command that produced it"
        );
    }

    /// The sniffed directory folds the same way, and — the point of it being its own field
    /// — without disturbing the *reported* one. A pane inside `ssh` has both at once and
    /// they name different machines; letting a snapshot's `fg_cwd` overwrite `cwd` would
    /// corrupt what resume metadata is written from.
    #[test]
    fn a_snapshot_carries_the_sniffed_directory_without_touching_the_reported_one() {
        let s = shadows();
        let mut reported = meta("u1", Some("ssh"));
        reported.cwd = Some("/on/the/far/host".into());
        reconcile_snapshot(&s, &[reported]);

        let mut sniffed = meta("u1", Some("ssh"));
        sniffed.fg_cwd = Some("/here".into());
        reconcile_snapshot(&s, &[sniffed]);
        let shadows = s.lock().unwrap();
        let shadow = shadows.get("u1").unwrap();
        assert_eq!(shadow.fg_cwd.as_deref(), Some("/here"));
        assert_eq!(
            shadow.cwd.as_deref(),
            Some("/on/the/far/host"),
            "the sniffed directory must not overwrite the reported one"
        );
    }

    /// A snapshot is built without the event bus's lock, so it can lag the `Data` events
    /// already folded here. The output cursor is monotonic; a late push must not rewind it
    /// (a rewound cursor re-sends bytes the terminal has already drawn).
    #[test]
    fn a_late_snapshot_never_walks_the_output_cursor_backwards() {
        let s = shadows();
        apply_event_to_shadow(
            &s,
            &SessionEvent::Data {
                uid: "u1".into(),
                data: "abcd".into(),
                cursor: 4,
            },
        );
        let mut stale = meta("u1", None);
        stale.output_bytes = 2;
        reconcile_snapshot(&s, &[stale]);
        assert_eq!(s.lock().unwrap().get("u1").unwrap().output_bytes, 4);
    }

    /// A `Data` chunk that races ahead of the `Replay` frame must not be thrown away, and
    /// the part of the mirror the replay already covers must not be doubled.
    #[test]
    fn replay_splice_keeps_the_live_tail_past_the_cursor() {
        let mut sh = Shadow::new();
        // Live output mirrored before the replay landed: "cd" at positions 2..4.
        sh.output_bytes = 2;
        sh.replay.append("cd");
        sh.output_bytes = 4;
        // The daemon's retained buffer through position 3: "abc".
        splice_replay(&mut sh, 3, "abc");
        assert_eq!(sh.replay.get(), "abcd");
        assert_eq!(sh.output_bytes, 4);
    }

    #[test]
    fn replay_splice_replaces_a_fully_covered_mirror() {
        let mut sh = Shadow::new();
        sh.replay.append("bc");
        sh.output_bytes = 3;
        // The replay reaches past everything mirrored so far.
        splice_replay(&mut sh, 5, "abcde");
        assert_eq!(sh.replay.get(), "abcde");
        assert_eq!(sh.output_bytes, 5, "the cursor advances to the replay's end");
    }

    #[test]
    fn replay_splice_seeds_an_empty_mirror() {
        let mut sh = Shadow::new();
        splice_replay(&mut sh, 3, "abc");
        assert_eq!(sh.replay.get(), "abc");
        assert_eq!(sh.output_bytes, 3);
    }

    #[test]
    fn utf16_tail_never_splits_a_surrogate_pair() {
        assert_eq!(utf16_tail("ab😀", 2), "😀");
        assert_eq!(utf16_tail("ab😀", 1), "", "one unit cannot hold a 2-unit char");
        assert_eq!(utf16_tail("ab😀", 3), "b😀");
        assert_eq!(utf16_tail("abc", 10), "abc");
        assert_eq!(utf16_tail("abc", 0), "");
    }

    #[test]
    fn data_event_grows_mirror_and_counters() {
        let s = shadows();
        apply_event_to_shadow(
            &s,
            &SessionEvent::Data {
                uid: "u1".into(),
                data: "ab".into(),
                cursor: 2,
            },
        );
        apply_event_to_shadow(
            &s,
            &SessionEvent::Data {
                uid: "u1".into(),
                data: "😀".into(),
                cursor: 4,
            },
        );
        let g = s.lock().unwrap();
        let sh = g.get("u1").unwrap();
        assert_eq!(sh.replay.get(), "ab😀");
        assert_eq!(sh.output_bytes, 4, "ab=2 + emoji=2 UTF-16 units");
        assert!(sh.last_output_at.is_some());
    }

    #[test]
    fn cwd_event_updates_shadow_cwd() {
        let s = shadows();
        apply_event_to_shadow(
            &s,
            &SessionEvent::Cwd {
                uid: "u1".into(),
                cwd: "/tmp".into(),
            },
        );
        assert_eq!(
            s.lock().unwrap().get("u1").unwrap().cwd.as_deref(),
            Some("/tmp")
        );
    }

    #[test]
    fn exit_event_drops_the_shadow() {
        let s = shadows();
        apply_event_to_shadow(
            &s,
            &SessionEvent::Data {
                uid: "u1".into(),
                data: "x".into(),
                cursor: 1,
            },
        );
        assert!(s.lock().unwrap().contains_key("u1"));
        apply_event_to_shadow(
            &s,
            &SessionEvent::Exit {
                uid: "u1".into(),
                code: 0,
            },
        );
        assert!(
            !s.lock().unwrap().contains_key("u1"),
            "Exit drops the session shadow"
        );
    }

    // ---- end-to-end: DaemonSessionManager against a REAL in-process daemon ----
    //
    // These reuse M0's loopback harness (`session::daemon::spawn_in_process`, on a temp
    // socket with the daemon's own runtime) and drive the M1 client over it: create →
    // observe Data/Exit on the GUI channel, replay() returns the mirror, render_screen()
    // round-trips, kill works, and a fresh manager on the same socket re-seeds from Attach.

    use crate::session::daemon::spawn_in_process;
    use std::time::Duration as Dur;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    // A unique temp socket path per test AND per run (pid + thread id) — never collides.
    #[tracing::instrument(level = "debug", ret)]
    fn temp_socket(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hp-m1-{tag}-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    // Block (on a helper thread / spin) until an event channel yields one matching `pred`,
    // or the deadline passes. Drains intervening events. The channel is the GUI's
    // `UnboundedReceiver<SessionEvent>` that the manager's reader thread feeds.
    #[tracing::instrument(level = "debug", skip_all)]
    fn recv_event_until(
        rx: &mut UnboundedReceiver<SessionEvent>,
        timeout: Dur,
        mut pred: impl FnMut(&SessionEvent) -> bool,
    ) -> Option<SessionEvent> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ev) if pred(&ev) => return Some(ev),
                Ok(_) => continue,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Dur::from_millis(5));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
            }
        }
        None
    }

    // Spin until `cond` is true or the deadline passes (for shadow propagation, which lands
    // a beat after the event since the reader thread applies it asynchronously).
    #[tracing::instrument(level = "debug", skip_all)]
    fn wait_until(timeout: Dur, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Dur::from_millis(5));
        }
        cond()
    }

    #[tracing::instrument(level = "debug")]
    fn connect_manager(socket: &Path) -> (DaemonSessionManager, UnboundedReceiver<SessionEvent>) {
        let stream = std::os::unix::net::UnixStream::connect(socket).expect("connect");
        let (etx, erx) = unbounded_channel::<SessionEvent>();
        let mgr = DaemonSessionManager::from_stream(stream, etx).expect("manager");
        (mgr, erx)
    }

    // create → write → observe Data on the GUI channel; replay() mirrors; the shadow
    // accumulates output_bytes/last_output_at; kill() drops it synchronously. Uses a
    // long-lived interactive shell so the session stays alive while we assert the (live)
    // mirror — `create_short_command_streams_data_then_exit` covers the exit path.
    #[test]
    fn create_write_streams_data_replay_mirrors_and_kill() {
        let socket = temp_socket("create");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");
        let (mgr, mut rx) = connect_manager(&socket);

        mgr.create(SpawnOptions {
            uid: "p1".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");

        // has()/uids() reflect the session immediately (shadow inserted on create).
        assert!(mgr.has("p1"), "has() true right after create");
        assert!(mgr.uids().contains(&"p1".to_string()));

        // Drive a marker; its echo streams back as Data on the GUI channel.
        mgr.write("p1", "echo HELLO_MARKER\n")
            .expect("the marker reached the session");
        let data = recv_event_until(
            &mut rx,
            Dur::from_secs(10),
            |e| matches!(e, SessionEvent::Data { uid, data, .. } if uid == "p1" && data.contains("HELLO_MARKER")),
        );
        assert!(
            data.is_some(),
            "expected Data{{HELLO_MARKER}} on the GUI channel"
        );

        // replay() returns the client mirror (no round-trip) and includes the output.
        assert!(
            wait_until(Dur::from_secs(2), || {
                mgr.replay("p1").is_some_and(|r| r.contains("HELLO_MARKER"))
            }),
            "replay() mirror should hold the streamed output, got {:?}",
            mgr.replay("p1")
        );
        // output_bytes / last_output_at shadow advanced.
        assert!(
            mgr.output_bytes("p1").unwrap_or(0) > 0,
            "output_bytes shadow advanced"
        );
        assert!(
            mgr.last_output_at("p1").is_some(),
            "last_output_at shadow set"
        );

        // kill() drops the shadow synchronously (deliberate kill is silent — no Exit event).
        mgr.kill("p1");
        assert!(!mgr.has("p1"), "kill drops the shadow synchronously");
    }

    // A short-lived command streams its Data AND a natural Exit{0} to the GUI channel, and
    // the natural exit drops the shadow (has() → false) — mirroring the in-process driver
    // removing a session from the map on terminal exit.
    #[test]
    fn create_short_command_streams_data_then_exit() {
        let socket = temp_socket("shortcmd");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");
        let (mgr, mut rx) = connect_manager(&socket);

        // The 0.3s sleep holds output back until the daemon-side auto-attach (on Create)
        // has registered us, so the Data + Exit stream live and deterministically.
        mgr.create(SpawnOptions {
            uid: "q1".into(),
            command: Some("/bin/sh".into()),
            args: Some(vec!["-c".into(), "sleep 0.3; echo hi".into()]),
            ..Default::default()
        })
        .expect("create");

        let data = recv_event_until(
            &mut rx,
            Dur::from_secs(10),
            |e| matches!(e, SessionEvent::Data { uid, data, .. } if uid == "q1" && data.contains("hi")),
        );
        assert!(data.is_some(), "expected Data{{hi}} on the GUI channel");

        let exit = recv_event_until(
            &mut rx,
            Dur::from_secs(10),
            |e| matches!(e, SessionEvent::Exit { uid, code } if uid == "q1" && *code == 0),
        );
        assert!(exit.is_some(), "expected Exit{{0}} on the GUI channel");
        assert!(
            wait_until(Dur::from_secs(2), || !mgr.has("q1")),
            "natural exit drops the shadow"
        );
    }

    // render_screen() round-trips to the daemon (a bounded request/response).
    #[test]
    fn render_screen_round_trips() {
        let socket = temp_socket("screen");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");
        let (mgr, mut rx) = connect_manager(&socket);

        mgr.create(SpawnOptions {
            uid: "p1".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");

        // Drive a marker and wait for it to stream so the daemon's screen has content.
        mgr.write("p1", "echo SCREEN_MARKER\n")
            .expect("the marker reached the session");
        let saw = recv_event_until(
            &mut rx,
            Dur::from_secs(10),
            |e| matches!(e, SessionEvent::Data { uid, data, .. } if uid == "p1" && data.contains("SCREEN_MARKER")),
        );
        assert!(saw.is_some(), "marker should stream");

        // render_screen() returns the serialized screen (a real round-trip), containing it.
        let screen = wait_until(Dur::from_secs(3), || {
            mgr.render_screen("p1")
                .is_some_and(|s| s.contains("SCREEN_MARKER"))
        });
        assert!(
            screen,
            "render_screen should round-trip the screen incl. the marker"
        );

        // An unknown uid renders to None (gone session / never existed).
        assert_eq!(mgr.render_screen("nope"), None);

        mgr.kill("p1");
        assert!(!mgr.has("p1"), "kill drops the shadow synchronously");
    }

    // RECONNECT: drop the client, make a NEW manager on the same socket; uids() shows the
    // survivor and replay() re-seeds from the Attach reply (the M2 payoff, at the client).
    #[test]
    fn reconnect_shows_survivor_and_reseeds_replay_from_attach() {
        let socket = temp_socket("reconnect");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");

        // First manager: create a long-lived shell and drive a marker into it.
        let (mgr1, mut rx1) = connect_manager(&socket);
        mgr1.create(SpawnOptions {
            uid: "surv".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");
        mgr1.write("surv", "echo SURVIVOR_MARKER\n")
            .expect("the marker reached the session");
        assert!(
            recv_event_until(&mut rx1, Dur::from_secs(10), |e| {
                matches!(e, SessionEvent::Data { uid, data, .. } if uid == "surv" && data.contains("SURVIVOR_MARKER"))
            })
            .is_some(),
            "marker should stream to the first manager"
        );
        // The marker is now in the daemon's retained replay buffer for this live session.

        // Drop the first manager (simulating a GUI crash) — the daemon + session survive.
        drop(mgr1);
        drop(rx1);

        // A FRESH manager on the same socket: ListSessions (on connect) shows the survivor,
        // and the Attach it issues re-seeds the replay mirror from the daemon's buffer.
        let (mgr2, _rx2) = connect_manager(&socket);
        assert!(
            wait_until(Dur::from_secs(2), || mgr2
                .uids()
                .contains(&"surv".to_string())),
            "reconnect: uids() should show the survivor, got {:?}",
            mgr2.uids()
        );
        assert!(
            wait_until(Dur::from_secs(3), || {
                mgr2.replay("surv")
                    .is_some_and(|r| r.contains("SURVIVOR_MARKER"))
            }),
            "reconnect: replay() should re-seed from the Attach reply, got {:?}",
            mgr2.replay("surv")
        );

        mgr2.kill("surv");
    }

    // ...and the seed is there the INSTANT `connect` returns, not eventually. The test above
    // polls, which is fair for a mirror the event stream also grows — but the GUI does not
    // poll: `make_pane_from_spec` reads `replay(uid)` once, synchronously, to prime a
    // re-attached pane's grid. Before the connect barrier that read landed ahead of the
    // `Replay` frames and every survivor came back showing a bare prompt, its scrollback
    // gone even though the process had never died.
    #[test]
    fn the_replay_seed_is_ready_when_connect_returns() {
        let socket = temp_socket("seed-sync");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");

        let (mgr1, mut rx1) = connect_manager(&socket);
        mgr1.create(SpawnOptions {
            uid: "surv".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");
        mgr1.write("surv", "echo SEED_SYNC_MARKER\n")
            .expect("the marker reached the session");
        assert!(
            recv_event_until(&mut rx1, Dur::from_secs(10), |e| {
                matches!(e, SessionEvent::Data { uid, data, .. } if uid == "surv" && data.contains("SEED_SYNC_MARKER"))
            })
            .is_some(),
            "marker should reach the daemon's replay buffer"
        );
        drop(mgr1);
        drop(rx1);

        let (mgr2, _rx2) = connect_manager(&socket);
        // No `wait_until`: this is the read the GUI actually performs.
        let seeded = mgr2.replay("surv");
        assert!(
            seeded
                .as_deref()
                .is_some_and(|r| r.contains("SEED_SYNC_MARKER")),
            "connect must not return before the Attach replay is folded in, got {seeded:?}"
        );

        mgr2.kill("surv");
    }

    // The width the replay was WRITTEN at travels with it. A survivor's retained replay is raw
    // pty output — cursor-positioning escapes included — so the re-attaching GUI has to build
    // its grid at the daemon's grid size before feeding it. That size is only knowable through
    // the shadow, and `SessionManager::dims` used to answer a hardcoded `None` for every
    // daemon-backed pane, which is why a restored pane rendered its scrollback at 80 columns
    // whatever width it actually had.
    #[test]
    fn a_survivors_grid_size_is_known_after_reconnect() {
        let socket = temp_socket("dims-mirror");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");

        let (mgr1, mut rx1) = connect_manager(&socket);
        mgr1.create(SpawnOptions {
            uid: "surv".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            cols: Some(113),
            rows: Some(37),
            ..Default::default()
        })
        .expect("create");
        // `has` answers from the optimistic local shadow, so it says yes before the daemon
        // has spawned anything — wait for real output instead, which only the pty can produce.
        mgr1.write("surv", "echo DIMS_READY\n")
            .expect("the marker reached the session");
        assert!(
            recv_event_until(&mut rx1, Dur::from_secs(10), |e| {
                matches!(e, SessionEvent::Data { uid, data, .. } if uid == "surv" && data.contains("DIMS_READY"))
            })
            .is_some(),
            "session should come up"
        );
        drop(mgr1);
        drop(rx1);

        let (mgr2, _rx2) = connect_manager(&socket);
        assert_eq!(
            mgr2.dims("surv"),
            Some((113, 37)),
            "reconnect must learn the pty grid, not leave the caller guessing 80x24"
        );

        mgr2.kill("surv");
    }

    // ---- M2 re-attach: the SessionManager-level decision the GUI restore branches on ----
    //
    // `state.rs::make_pane_from_spec` decides RE-ATTACH vs RE-SPAWN with exactly this
    // predicate: `mgr.is_daemon() && spec.uid.map(|u| mgr.has(u)).unwrap_or(false)`. These
    // tests stand up a SessionManager::Daemon over a REAL (in-process) daemon and prove that
    // predicate end-to-end — a SURVIVING uid re-attaches (the session was NOT re-spawned;
    // `replay` carries the prior output), an UNKNOWN/dead uid does not — plus the
    // uid-stability invariant the whole scheme rests on.

    use crate::session_manager::SessionManager;

    // Wrap a real connected daemon socket in a SessionManager::Daemon — the same backend the
    // GUI holds (`new_daemon` builds this; here we connect to a temp-socket in-process daemon
    // instead of spawning `current_exe`, which a test binary can't do).
    #[tracing::instrument(level = "debug")]
    fn daemon_manager(socket: &Path) -> (SessionManager, UnboundedReceiver<SessionEvent>) {
        let stream = std::os::unix::net::UnixStream::connect(socket).expect("connect");
        let (etx, erx) = unbounded_channel::<SessionEvent>();
        let mgr = DaemonSessionManager::from_stream(stream, etx).expect("manager");
        (SessionManager::Daemon(Arc::new(mgr)), erx)
    }

    // The crux: a snapshot uid that is STILL LIVE in the daemon re-attaches (no re-spawn),
    // and a dead/unknown uid does not — so the GUI restore would re-spawn it instead.
    #[test]
    fn surviving_uid_reattaches_unknown_uid_does_not() {
        let socket = temp_socket("reattach-decide");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");

        // --- "previous run": create a long-lived session under a recorded uid + drive output.
        let recorded_uid = {
            let (mgr1, mut rx1) = daemon_manager(&socket);
            assert!(mgr1.is_daemon(), "the daemon backend reports is_daemon()");
            // A GUI pane would mint this via `mgr.fresh_uid()` (a UUID on the daemon backend);
            // a literal uid is fine for the test — the point is it's PINNED + recorded.
            let uid = mgr1.fresh_uid();
            assert!(
                uid.starts_with("pane-"),
                "daemon fresh_uid is a pane-<uuid>, got {uid}"
            );
            mgr1.create(SpawnOptions {
                uid: uid.clone(),
                shell: Some("/bin/sh".into()),
                args: Some(vec!["-i".into()]),
                ..Default::default()
            })
            .expect("create");
            mgr1.write(&uid, "echo REATTACH_MARKER\n")
                .expect("the marker reached the session");
            assert!(
                recv_event_until(&mut rx1, Dur::from_secs(10), |e| {
                    matches!(e, SessionEvent::Data { uid: u, data, .. } if *u == uid && data.contains("REATTACH_MARKER"))
                })
                .is_some(),
                "marker should stream into the live session"
            );
            // Snapshot would record `uid` (to_session_file does). Drop the manager = GUI crash.
            uid
        };

        // --- "next launch": a FRESH manager on the same daemon. The restore predicate:
        let (mgr2, _rx2) = daemon_manager(&socket);

        // (a) The surviving uid: is_daemon && has(uid) → RE-ATTACH. The session was NOT
        //     re-spawned (it's the very same one), and replay carries its prior output.
        assert!(
            wait_until(Dur::from_secs(2), || mgr2.has(&recorded_uid)),
            "the survivor's uid is live in the daemon after reconnect, got uids {:?}",
            mgr2.uids()
        );
        let reattach_survivor = mgr2.is_daemon() && mgr2.has(&recorded_uid);
        assert!(
            reattach_survivor,
            "restore would RE-ATTACH the surviving uid (no re-spawn)"
        );
        // The same decision through the named API the GUI actually calls (M6): the survivor
        // re-attaches UNDER ITS RECORDED UID (a different uid would spawn a second session).
        assert_eq!(
            mgr2.pane_load(Some(&recorded_uid)),
            crate::session_manager::PaneLoad::Reattach(recorded_uid.clone()),
            "pane_load re-attaches the survivor under its recorded uid"
        );
        assert!(
            wait_until(Dur::from_secs(3), || {
                mgr2.replay(&recorded_uid)
                    .is_some_and(|r| r.contains("REATTACH_MARKER"))
            }),
            "re-attach seeds the fresh grid from the survivor's replay, got {:?}",
            mgr2.replay(&recorded_uid)
        );

        // (b) An unknown/dead uid (the program had exited last run): has() is false → the GUI
        //     falls back to a fresh spawn from spec.command/args/shell.
        let dead_uid = "pane-00000000-dead-dead-dead-000000000000";
        let reattach_dead = mgr2.is_daemon() && mgr2.has(dead_uid);
        assert!(
            !reattach_dead,
            "an unknown/dead uid does NOT re-attach → restore re-spawns it"
        );
        let dead_load = mgr2.pane_load(Some(dead_uid));
        assert!(
            !dead_load.is_reattach(),
            "pane_load spawns for a dead uid, got {dead_load:?}"
        );
        assert_ne!(
            dead_load.uid(),
            dead_uid,
            "the re-spawn mints a fresh uid rather than re-using the dead one"
        );

        mgr2.kill(&recorded_uid);
    }

    // M6 COMPATIBILITY, against a real daemon: a workspace file written by an OLD build
    // records no pane uids at all. Loading it must degrade to SPAWN-EVERYTHING — even with
    // live sessions sitting in the daemon — and must never adopt one of them by accident.
    // The mixed case is the same run: the one pane whose recorded uid is live re-attaches to
    // exactly that session, every other pane spawns, and the daemon gains no duplicate.
    #[test]
    fn a_legacy_uidless_workspace_spawns_everything_even_beside_live_sessions() {
        use crate::workspace::model::{GroupSpec, PaneSpec, WorkspaceFile};

        let socket = temp_socket("legacy-spawn");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");
        let (mgr, _rx) = daemon_manager(&socket);

        // A live session, as if left behind by a previous run.
        let live = mgr.fresh_uid();
        mgr.create(SpawnOptions {
            uid: live.clone(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");
        assert!(
            wait_until(Dur::from_secs(2), || mgr.has(&live)),
            "the live session registers, uids {:?}",
            mgr.uids()
        );
        let sessions_before = mgr.uids().len();

        // (a) A legacy file: two panes, NO uid on either. Every pane spawns fresh.
        let legacy = WorkspaceFile {
            groups: Some(vec![GroupSpec {
                panes: vec![
                    PaneSpec {
                        command: Some("claude".into()),
                        ..Default::default()
                    },
                    PaneSpec {
                        command: Some("htop".into()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }]),
            ..Default::default()
        };
        for (i, spec) in legacy
            .groups
            .iter()
            .flatten()
            .flat_map(|g| &g.panes)
            .enumerate()
        {
            let load = mgr.pane_load(spec.uid.as_deref());
            assert!(
                !load.is_reattach(),
                "legacy pane {i} must spawn even on a daemon backend, got {load:?}"
            );
            assert_ne!(
                load.uid(),
                live.as_str(),
                "a uid-less pane must never adopt an unrelated live session"
            );
        }

        // (b) A new-format file whose FIRST pane's uid is live and whose second is dead: the
        //     survivor re-attaches under its own uid, the dead one spawns fresh.
        let modern = [Some(live.as_str()), Some("pane-0000-dead"), None];
        let decisions: Vec<_> = modern.iter().map(|u| mgr.pane_load(*u)).collect();
        assert_eq!(
            decisions[0],
            crate::session_manager::PaneLoad::Reattach(live.clone()),
            "the live uid re-attaches"
        );
        assert!(!decisions[1].is_reattach(), "a dead uid spawns");
        assert!(!decisions[2].is_reattach(), "a uid-less pane spawns");
        assert_ne!(decisions[1].uid(), decisions[2].uid());

        // Deciding never spawned anything: exactly one re-attach target, no duplicate.
        assert_eq!(
            mgr.uids().len(),
            sessions_before,
            "reattach-or-spawn creates no sessions by itself, uids {:?}",
            mgr.uids()
        );
        assert_eq!(
            mgr.uids().iter().filter(|u| **u == live).count(),
            1,
            "the survivor is adopted once, not duplicated"
        );

        mgr.kill(&live);
    }

    // uid-stability invariant (the plan's "uid stability"): the daemon backend's fresh_uid is
    // UNIQUE across independent "runs" (manager instances) — a new run's freshly minted uid
    // can never collide with (and thus silently adopt) a survivor from a prior run. A literal
    // process-local counter would re-issue `pane-0` every run and alias the survivor.
    #[test]
    fn daemon_fresh_uid_is_unique_across_runs() {
        let socket = temp_socket("uid-stability");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");

        // Two independent managers (two GUI runs against the same daemon) each mint a batch
        // of fresh uids; the two batches must be disjoint (no cross-run collision).
        let (runa, _ra) = daemon_manager(&socket);
        let (runb, _rb) = daemon_manager(&socket);

        let batch1: Vec<String> = (0..8).map(|_| runa.fresh_uid()).collect();
        let batch2: Vec<String> = (0..8).map(|_| runb.fresh_uid()).collect();

        // Unique within each run AND across the two runs.
        let mut all = batch1.clone();
        all.extend(batch2.clone());
        all.sort();
        all.dedup();
        assert_eq!(
            all.len(),
            batch1.len() + batch2.len(),
            "fresh_uid never collides across runs"
        );
        drop((runa, runb, _ra, _rb));

        // And the same uid a run minted is exactly the one a survivor would be re-attached by:
        // create under a minted uid, drop, reconnect, has(uid) true (the round-trip the GUI
        // snapshot→reattach relies on).
        let (run_a, mut rx_a) = daemon_manager(&socket);
        let surv = run_a.fresh_uid();
        run_a
            .create(SpawnOptions {
                uid: surv.clone(),
                shell: Some("/bin/sh".into()),
                args: Some(vec!["-i".into()]),
                ..Default::default()
            })
            .expect("create");
        // Drive a marker and wait for its echo so the session is CONFIRMED registered + live
        // daemon-side before we drop (mirrors the reconnect test — without this the daemon may
        // not have finished spawning the pty when ListSessions runs on the next connect).
        run_a
            .write(&surv, "echo STABLE_SURVIVOR\n")
            .expect("the marker reached the session");
        assert!(
            recv_event_until(&mut rx_a, Dur::from_secs(10), |e| {
                matches!(e, SessionEvent::Data { uid: u, data, .. } if *u == surv && data.contains("STABLE_SURVIVOR"))
            })
            .is_some(),
            "the minted-uid session is live daemon-side before we drop"
        );
        drop(run_a);
        drop(rx_a);

        let (run3, _r3) = daemon_manager(&socket);
        assert!(
            wait_until(Dur::from_secs(2), || run3.has(&surv)),
            "the minted uid round-trips: a later run re-attaches the survivor by that exact uid"
        );
        run3.kill(&surv);
    }

    // ---- keystroke→echo micro-bench: daemon vs in-process ----
    //
    // The plan's latency risk: the daemon adds a local UDS hop per keystroke/output chunk.
    // This measures keystroke→echoed-Data round-trip latency on BOTH backends and prints
    // both numbers, to confirm the daemon overhead is negligible (the design's hypothesis).
    //
    // Ignored by default (it spawns real shells and takes a couple seconds); run with:
    //   cargo test -p hyperpanes-core keystroke_echo_latency_bench -- --ignored --nocapture
    #[test]
    #[ignore = "micro-bench: run with --ignored --nocapture"]
    fn keystroke_echo_latency_bench() {
        const ITERS: usize = 60;
        const WARMUP: usize = 5;

        // In-process backend: a real SessionManager (no daemon, no socket).
        let inproc = {
            let (etx, mut erx) = unbounded_channel::<SessionEvent>();
            let rt = tokio::runtime::Runtime::new().expect("rt");
            let _g = rt.enter();
            let mgr = crate::session_manager::SessionManager::new(etx);
            mgr.create(SpawnOptions {
                uid: "ip".into(),
                shell: Some("/bin/sh".into()),
                args: Some(vec!["-i".into()]),
                ..Default::default()
            })
            .expect("create inproc");
            // Drain the shell's startup banner before timing.
            std::thread::sleep(Dur::from_millis(300));
            while erx.try_recv().is_ok() {}
            let lat = bench_echo("ip", &mgr, &mut erx, ITERS, WARMUP);
            mgr.kill("ip");
            // Keep the runtime alive for the duration (drivers run on it); drop after.
            drop(rt);
            lat
        };

        // Daemon backend: a DaemonSessionManager over an in-process daemon (a real socket).
        let daemon = {
            let socket = temp_socket("bench");
            let _d = spawn_in_process(&socket).expect("daemon binds");
            let (mgr, mut rx) = connect_manager(&socket);
            mgr.create(SpawnOptions {
                uid: "dm".into(),
                shell: Some("/bin/sh".into()),
                args: Some(vec!["-i".into()]),
                ..Default::default()
            })
            .expect("create daemon");
            std::thread::sleep(Dur::from_millis(300));
            while rx.try_recv().is_ok() {}
            let lat = bench_echo("dm", &mgr, &mut rx, ITERS, WARMUP);
            mgr.kill("dm");
            lat
        };

        println!("\n=== keystroke->echo latency ({ITERS} iters, {WARMUP} warmup) ===");
        println!(
            "  in-process : mean {:>7.1}us  p50 {:>7.1}us  max {:>7.1}us",
            inproc.0, inproc.1, inproc.2
        );
        println!(
            "  daemon     : mean {:>7.1}us  p50 {:>7.1}us  max {:>7.1}us",
            daemon.0, daemon.1, daemon.2
        );
        println!("  daemon overhead (mean): {:+.1}us\n", daemon.0 - inproc.0);
    }

    // A backend-agnostic echo timer: write a unique marker line, time until its echoed Data
    // arrives on `rx`, repeated. Returns (mean_us, p50_us, max_us). Works for any type with
    // `write(&str)` and a paired `UnboundedReceiver<SessionEvent>` — i.e. both backends.
    trait WriteToBackend {
        fn write(&self, uid: &str, data: &str);
    }
    impl WriteToBackend for crate::session_manager::SessionManager {
        #[tracing::instrument(level = "debug", ret, skip(self))]
        fn write(&self, uid: &str, data: &str) {
            // A dropped write here would not fail the benchmark, it would silently bias it:
            // the echo it was waiting for never comes and the sample is a timeout.
            self.write(uid, data)
                .expect("benchmark write reached the session");
        }
    }
    impl WriteToBackend for DaemonSessionManager {
        #[tracing::instrument(level = "debug", ret, skip(self))]
        fn write(&self, uid: &str, data: &str) {
            self.write(uid, data)
                .expect("benchmark write reached the session");
        }
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn bench_echo(
        uid: &str,
        mgr: &impl WriteToBackend,
        rx: &mut UnboundedReceiver<SessionEvent>,
        iters: usize,
        warmup: usize,
    ) -> (f64, f64, f64) {
        let mut samples = Vec::with_capacity(iters);
        for i in 0..iters {
            let marker = format!("M{i}Z");
            let t0 = Instant::now();
            mgr.write(uid, &format!("echo {marker}\n"));
            // Wait for the echoed marker to come back as Data.
            let got = recv_event_until(
                rx,
                Dur::from_secs(5),
                |e| matches!(e, SessionEvent::Data { uid: u, data, .. } if u == uid && data.contains(&marker)),
            );
            let dt = t0.elapsed();
            assert!(got.is_some(), "echo {marker} timed out");
            if i >= warmup {
                samples.push(dt.as_secs_f64() * 1e6); // microseconds
            }
            // Drain any trailing chunks (prompt redraw) before the next iteration.
            while rx.try_recv().is_ok() {}
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        let p50 = samples[samples.len() / 2];
        let max = *samples.last().unwrap();
        (mean, p50, max)
    }

    // M0 follow-up: a create whose pty spawn FAILS surfaces as an Exit event (instead of a
    // silently-hung blank pane). Force a failure with a non-existent shell binary.
    #[test]
    fn create_spawn_failure_surfaces_as_exit() {
        let socket = temp_socket("spawnfail");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");
        let (mgr, mut rx) = connect_manager(&socket);

        mgr.create(SpawnOptions {
            uid: "bad".into(),
            // A direct spawn of a binary that does not exist → the pty spawn errors.
            command: Some("/nonexistent/definitely-not-a-real-binary-xyz".into()),
            args: Some(vec!["/nonexistent/definitely-not-a-real-binary-xyz".into()]),
            ..Default::default()
        })
        .expect("create request sends");

        // The daemon injects an Exit for the uid on spawn failure; the client reflects it.
        let exit = recv_event_until(
            &mut rx,
            Dur::from_secs(5),
            |e| matches!(e, SessionEvent::Exit { uid, .. } if uid == "bad"),
        );
        assert!(
            exit.is_some(),
            "a spawn failure should surface as an Exit, not a hang"
        );
        assert!(
            wait_until(Dur::from_secs(2), || !mgr.has("bad")),
            "the failed session is dropped"
        );
    }

    // ====================== M3 proto-version handshake + shutdown ======================

    // probe_daemon_identity against a REAL daemon (same PROTO_VER) returns Match, AND the
    // manager built on that same socket afterwards still works — i.e. the probe's Hello
    // round-trip did NOT desync the stream nor leave a read timeout on the shared fd (a
    // regression guard for the SO_RCVTIMEO-is-shared subtlety).
    #[test]
    fn version_probe_matches_real_daemon_and_manager_works_after() {
        let socket = temp_socket("proto-match");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");

        let stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
        assert!(
            matches!(
                probe_daemon_identity(&stream).expect("probe"),
                ProtoCheck::Match
            ),
            "a same-version daemon must match"
        );
        // Build the manager on the SAME stream (as `new` does after a Match) and drive it —
        // proves no desync + blocking reads restored (the reader thread must not time out).
        let (etx, mut rx) = unbounded_channel::<SessionEvent>();
        let mgr = DaemonSessionManager::from_stream(stream, etx).expect("manager after probe");
        mgr.create(SpawnOptions {
            uid: "pm".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");
        mgr.write("pm", "echo PROBE_OK\n")
            .expect("the marker reached the session");
        assert!(
            recv_event_until(&mut rx, Dur::from_secs(10), |e| {
                matches!(e, SessionEvent::Data { uid, data, .. } if uid == "pm" && data.contains("PROBE_OK"))
            })
            .is_some(),
            "the manager must stream normally after the version probe"
        );
        mgr.kill("pm");
    }

    // A daemon that reports a DIFFERENT proto_ver makes the probe return Mismatch — the
    // signal `new` uses to tear down + respawn. Stand up a one-shot fake listener that
    // answers the client's Hello with a bumped version.
    #[test]
    fn version_probe_detects_a_mismatched_daemon() {
        let socket = temp_socket("proto-mismatch");
        let _ = std::fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("fake bind");

        // Fake daemon: accept one connection, read the client's Hello, reply with a version
        // ONE GREATER than ours (a "newer daemon" a stale client would meet).
        let server = std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let _ = read_frame::<_, ClientMsg>(&mut conn); // the client's Hello
                let _ = write_frame(
                    &mut conn,
                    &DaemonMsg::Hello {
                        proto_ver: PROTO_VER + 1,
                        daemon_pid: 4242,
                        conn_id: 1,
                        build_id: build_id::build_id().to_string(),
                    },
                );
                // Keep the connection open briefly so the client reads the reply.
                std::thread::sleep(Dur::from_millis(200));
            }
        });

        let stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect fake");
        let check = probe_daemon_identity(&stream).expect("probe");
        assert!(
            matches!(check, ProtoCheck::Mismatch { daemon_ver } if daemon_ver == PROTO_VER + 1),
            "a different-version daemon must be a Mismatch carrying the daemon's version"
        );
        drop(stream);
        let _ = server.join();
    }

    // Same protocol, DIFFERENT build: the case `PROTO_VER` alone can never see. The probe must
    // report it separately from a proto mismatch, because the two get different treatment —
    // a build mismatch is only ever worth a live takeover, never a teardown.
    #[test]
    fn version_probe_detects_a_daemon_of_another_build() {
        let socket = temp_socket("build-mismatch");
        let _ = std::fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("fake bind");

        // Fake daemon: our protocol exactly, but a build id that is plainly not ours.
        let server = std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let _ = read_frame::<_, ClientMsg>(&mut conn); // the client's Hello
                let _ = write_frame(
                    &mut conn,
                    &DaemonMsg::Hello {
                        proto_ver: PROTO_VER,
                        daemon_pid: 4243,
                        conn_id: 1,
                        build_id: "0.0.0+deadbeefdeadbeef".into(),
                    },
                );
                std::thread::sleep(Dur::from_millis(200));
            }
        });

        let stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect fake");
        let check = probe_daemon_identity(&stream).expect("probe");
        assert!(
            matches!(&check, ProtoCheck::BuildMismatch { daemon_build } if daemon_build == "0.0.0+deadbeefdeadbeef"),
            "a same-proto, other-build daemon must be a BuildMismatch carrying its build id"
        );
        drop(stream);
        let _ = server.join();
    }

    // The other half of the additive-field promise: a daemon built BEFORE `build_id` existed
    // answers without it, and must still read as a plain `Match`. Anything else would take
    // over every older daemon on sight — the exact "killed my terminals" failure this whole
    // mechanism exists to avoid.
    #[test]
    fn a_daemon_that_reports_no_build_is_still_a_match() {
        let socket = temp_socket("build-unknown");
        let _ = std::fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("fake bind");

        let server = std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let _ = read_frame::<_, ClientMsg>(&mut conn);
                let _ = write_frame(
                    &mut conn,
                    &DaemonMsg::Hello {
                        proto_ver: PROTO_VER,
                        daemon_pid: 4244,
                        conn_id: 1,
                        // What `#[serde(default)]` yields for a pre-build-id daemon's reply.
                        build_id: String::new(),
                    },
                );
                std::thread::sleep(Dur::from_millis(200));
            }
        });

        let stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect fake");
        assert!(
            matches!(
                probe_daemon_identity(&stream).expect("probe"),
                ProtoCheck::Match
            ),
            "an unknown build must never look like a build worth upgrading to"
        );
        drop(stream);
        let _ = server.join();
    }

    // ============== stale-daemon fallback: terminals outrank the upgrade ==============

    // The regression this file exists for. On 2026-08-29 a proto-1 daemon holding eight live
    // shells met a proto-2 client: the takeover was impossible (v1 cannot parse `Takeover`),
    // the client fell back to `tear_down_stale_daemon`, and all eight terminals died to make
    // room for the new build. A daemon holding live sessions must now be DRIVEN, never
    // killed — the upgrade waits until it is empty.
    #[test]
    fn a_stale_daemon_holding_live_sessions_is_driven_not_torn_down() {
        let socket = temp_socket("stale-occupied");
        let daemon = spawn_in_process(&socket).expect("daemon binds");
        let (mgr, mut rx) = connect_manager(&socket);
        mgr.create(SpawnOptions {
            uid: "keepme".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");
        assert!(
            recv_event_until(&mut rx, Dur::from_secs(10), |e| {
                matches!(e, SessionEvent::Data { uid, .. } if uid == "keepme")
            })
            .is_some(),
            "the session is live before the upgrade decision"
        );

        let endpoint = Endpoint::new(socket.to_string_lossy());
        assert_eq!(live_session_count(&endpoint), Some(1), "one live session");
        assert_eq!(
            stale_daemon_fallback(&endpoint, PROTO_VER - 1),
            StaleFallback::Drive,
            "a stale daemon with a live terminal must be driven, not killed"
        );
        assert!(
            !daemon.is_shutting_down(),
            "deciding the fallback must not touch the daemon"
        );
        assert!(mgr.has("keepme"), "the terminal survives the decision");
        mgr.kill("keepme");
    }

    // The other side of the rule: an EMPTY stale daemon costs nothing to replace, so the
    // clean upgrade still happens — the fix must not strand every old daemon forever.
    #[test]
    fn an_empty_stale_daemon_is_still_torn_down_and_replaced() {
        let socket = temp_socket("stale-empty");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");
        let endpoint = Endpoint::new(socket.to_string_lossy());

        assert_eq!(live_session_count(&endpoint), Some(0), "no sessions");
        assert_eq!(
            stale_daemon_fallback(&endpoint, PROTO_VER - 1),
            StaleFallback::TearDown,
            "an empty stale daemon is safe to replace"
        );
    }

    // "How many sessions?" is answered conservatively: a daemon we cannot reach or that does
    // not answer counts as OCCUPIED. Guessing "empty" wrongly destroys terminals; guessing
    // "occupied" wrongly only defers an upgrade.
    #[test]
    fn an_unreadable_session_count_counts_as_occupied() {
        let socket = temp_socket("stale-silent");
        let _ = std::fs::remove_file(&socket);
        let endpoint = Endpoint::new(socket.to_string_lossy());

        assert_eq!(live_session_count(&endpoint), None, "nothing listening");
        assert_ne!(
            stale_daemon_fallback(&endpoint, PROTO_VER - 1),
            StaleFallback::TearDown,
            "an unconfirmed session count must never authorise a tear-down"
        );
    }

    // The seam where M7 meets the stale-daemon rule. `StaleFallback::Drive` means this
    // build's client can be driving a proto-2 daemon that is holding the user's terminals —
    // and proto 2 cannot deserialize `Claim`, which the daemon's reader answers by CLOSING
    // the connection. Sending claim bookkeeping there would take every one of those
    // terminals off screen. So: not one claim frame may reach a pre-3 daemon, and `claim`
    // answers `true` locally (the pre-M7 behaviour: adoption allowed, and harmless because
    // it re-attaches to a multiplexed session rather than stealing it).
    #[test]
    fn no_claim_traffic_is_ever_sent_to_a_pre_m7_daemon() {
        let socket = temp_socket("claims-gate");
        let _ = std::fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("fake bind");

        // A fake proto-2 daemon: it answers `Hello` (reporting version 2) and `ListSessions`
        // and records every frame it is sent, so the test can assert on the whole stream. It
        // stops on a `Ping`, which the test sends last as a barrier — the manager's reader
        // thread keeps its own dup of the socket alive past `drop(mgr)`, so waiting for EOF
        // here would wait forever.
        let seen: Arc<Mutex<Vec<ClientMsg>>> = Arc::default();
        let seen_srv = Arc::clone(&seen);
        let server = std::thread::spawn(move || {
            let Ok((conn, _)) = listener.accept() else {
                return;
            };
            let mut r = conn.try_clone().expect("clone");
            let mut w = conn;
            while let Ok(Some(msg)) = read_frame::<_, ClientMsg>(&mut r) {
                seen_srv.lock().unwrap().push(msg.clone());
                match msg {
                    ClientMsg::Hello { .. } => {
                        let _ = write_frame(
                            &mut w,
                            &DaemonMsg::Hello {
                                proto_ver: MIN_CLAIM_DAEMON_VER - 1,
                                daemon_pid: 4242,
                                conn_id: 0,
                                build_id: build_id::build_id().to_string(),
                            },
                        );
                    }
                    ClientMsg::ListSessions => {
                        let _ = write_frame(&mut w, &DaemonMsg::Sessions(Vec::new()));
                    }
                    ClientMsg::Ping => break, // the test's end-of-stream barrier
                    _ => {}
                }
            }
        });

        let stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect fake");
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let mgr = DaemonSessionManager::from_stream(stream, etx).expect("manager");

        assert!(
            !mgr.claims_supported(),
            "a proto-{} daemon must be recognised as claim-blind",
            MIN_CLAIM_DAEMON_VER - 1
        );
        assert!(
            mgr.claim("orphan"),
            "with no registry to arbitrate, adoption stays allowed"
        );
        mgr.announce_claim("orphan");
        mgr.release("orphan");
        mgr.request_claims();
        assert!(
            mgr.claims_held_elsewhere().is_empty(),
            "no snapshots arrive, so nothing reads as somebody else's"
        );

        // The barrier: every frame the manager was going to send is already queued behind
        // it, so once the fake has read this one it has read them all.
        mgr.send(&ClientMsg::Ping).expect("barrier ping");
        let _ = server.join();
        drop(mgr);
        let seen = seen.lock().unwrap();
        assert!(
            !seen.iter().any(|m| matches!(
                m,
                ClientMsg::Claim { .. } | ClientMsg::Release { .. } | ClientMsg::ListClaims
            )),
            "a pre-M7 daemon must never be sent a claim frame; got {seen:?}"
        );
        assert!(
            seen.iter().any(|m| matches!(m, ClientMsg::Hello { .. })),
            "the fake daemon really was driven (the handshake reached it)"
        );
    }

    // The other direction: against a daemon of our own version the gate is open, so the
    // claim surface is live and the registry actually arbitrates.
    #[test]
    fn a_current_daemon_enables_the_claim_surface() {
        let socket = temp_socket("claims-open");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");
        let (mgr, _rx) = connect_manager(&socket);
        assert!(
            mgr.claims_supported(),
            "a daemon of this build speaks the claim protocol"
        );
        assert_ne!(mgr.conn_id(), 0, "and it minted us a connection id");
        assert!(mgr.claim("orphan"), "an unheld uid is granted");
    }

    // tear_down_stale_daemon actually brings a running daemon down: after it returns, the
    // socket is unlinked (the daemon exited) — the mechanism `new` relies on to clear the
    // stale daemon before respawning a fresh one.
    #[test]
    fn tear_down_stale_daemon_shuts_a_running_daemon_down() {
        let socket = temp_socket("teardown");
        let daemon = spawn_in_process(&socket).expect("daemon binds");
        assert!(socket.exists() && !daemon.is_shutting_down());

        tear_down_stale_daemon(&Endpoint::new(socket.to_string_lossy()), "salt-unused");

        assert!(
            wait_until(Dur::from_secs(3), || daemon.is_shutting_down()),
            "tear_down_stale_daemon should shut the daemon down"
        );
        assert!(
            wait_until(Dur::from_secs(1), || !socket.exists()),
            "the torn-down daemon unlinks its socket"
        );
    }

    // The manager-level shutdown_daemon() sends Shutdown and clears the local shadow, and the
    // daemon tears down (the quit-vs-keep-alive "OFF" branch at the client surface).
    /// A daemon that has gone takes its sessions with it, and the manager still holding the
    /// socket has to say so.
    ///
    /// The reader thread is the only thing in the process that can learn this: it is the end
    /// that sees EOF. Before `connected` it learned it and told nobody — it broke out of its
    /// loop, and `has`/`uids`/`write` went on answering out of a shadow map that nothing could
    /// ever update again. A pane stayed `running` forever and a write reported success into a
    /// closed socket.
    ///
    /// The daemon here is in-process and deliberately does NOT exit the test binary, so the
    /// socket is closed directly instead: a second descriptor on the same socket, shut down.
    /// `shutdown(2)` acts on the socket rather than the descriptor, so the manager's reader gets
    /// exactly the EOF a departed daemon delivers — with the shadow left fully populated, which
    /// is the state that used to lie.
    #[test]
    fn a_dead_socket_empties_has_and_uids_and_fails_writes() {
        let socket = temp_socket("gone");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");
        let stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
        let handle = stream
            .try_clone()
            .expect("a second descriptor on the same socket");
        let (etx, mut rx) = unbounded_channel::<SessionEvent>();
        let mgr = DaemonSessionManager::from_stream(stream, etx).expect("manager");

        mgr.create(SpawnOptions {
            uid: "g".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");
        // Drive a marker and wait for its echo, so the session is CONFIRMED live daemon-side and
        // the shadow is fully populated before the socket goes.
        mgr.write("g", "echo GONE_READY\n")
            .expect("a live daemon accepts input");
        assert!(
            recv_event_until(&mut rx, Dur::from_secs(10), |e| {
                matches!(e, SessionEvent::Data { uid, data, .. } if uid == "g" && data.contains("GONE_READY"))
            })
            .is_some(),
            "the session is live daemon-side before the socket dies"
        );
        assert!(mgr.has("g"));
        assert!(mgr.is_connected());

        handle
            .shutdown(std::net::Shutdown::Both)
            .expect("close the socket under the manager");

        assert!(
            wait_until(Dur::from_secs(5), || !mgr.is_connected()),
            "the reader publishes the socket's death"
        );
        assert!(
            !mgr.has("g"),
            "a dead daemon holds no sessions, whatever the frozen shadow still says"
        );
        assert!(mgr.uids().is_empty(), "and no uids either");
        let err = mgr
            .write("g", "x")
            .expect_err("a write to a gone daemon is an error, not a silent success");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn manager_shutdown_daemon_tears_the_daemon_down() {
        let socket = temp_socket("mgr-shutdown");
        let daemon = spawn_in_process(&socket).expect("daemon binds");
        let (mgr, _rx) = connect_manager(&socket);

        mgr.create(SpawnOptions {
            uid: "s".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");
        assert!(mgr.has("s"), "session present before shutdown");

        mgr.shutdown_daemon();
        assert!(!mgr.has("s"), "shutdown_daemon clears the local shadow");
        assert!(
            wait_until(Dur::from_secs(3), || daemon.is_shutting_down()),
            "shutdown_daemon brings the daemon down"
        );
    }
}
