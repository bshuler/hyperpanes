//! **Windows named-pipe daemon transport** — the Windows peer of the unix UDS daemon in
//! [`session::daemon`](crate::session::daemon).
//!
//! Everything above the transport is shared: the same [`session::proto`](crate::session::proto)
//! framing, the same [`SessionRegistry`], the same lifecycle (idle-exit, `Shutdown`), the same
//! client ([`daemon_client`](crate::session::daemon_client), which reaches this pipe through
//! [`session::transport`](crate::session::transport)). Only three things differ, and they are
//! all here:
//!
//! * [`pipe_name`] — the salted `\\.\pipe\hyperpanesd.<hash>` name, derived exactly like the
//!   unix socket path (FNV-1a of the salt → 16-hex token), so client and daemon agree on one
//!   endpoint per salt without re-hashing.
//! * [`bind_first_instance`] — the one-daemon-per-salt gate. On unix that is an `flock` on a
//!   sibling `.lock` file; here it is `first_pipe_instance(true)`, which the OS grants to
//!   exactly one server per pipe name and denies (`ERROR_ACCESS_DENIED`) to every other. That
//!   makes it a *race-free* detector in its own right — the create either wins the name or
//!   tells us an incumbent holds it — so no separate named mutex is needed. (The
//!   `single_instance` module needs its mutex because it detects by *connecting*, which races;
//!   we detect by *binding*, which does not.)
//! * [`serve`](Daemon::serve) — an async accept loop (the tokio named-pipe API is async, unlike
//!   `std::os::unix::net`), pre-arming the next instance before handing the current one to a
//!   task so a connect arriving mid-handoff is never refused.
//!
//! ## Async vs blocking
//! The UDS daemon serves each connection on a blocking OS thread. The tokio named-pipe API is
//! async, so this serve loop is `async` and runs on the daemon's Tokio runtime — the pty
//! drivers already need that runtime, so there is no extra cost. The proto's
//! `read_frame`/`write_frame` want synchronous `Read`/`Write`, so frames are decoded out of an
//! accumulating buffer instead — see [`PipeConn`].
//!
//! ## The live upgrade (M1)
//! `Takeover` is answered here, but NOT the way unix answers it. Unix passes the pty master
//! fds to the successor over `SCM_RIGHTS`. Windows has no equivalent for a ConPTY: an `HPCON`
//! is an opaque heap pointer owned by the process that called `CreatePseudoConsole`, and when
//! that process exits, conhost sees the signal pipe break and kills the attached shell. So the
//! ConPTYs live in a **pty-host** process instead — a daemon like this one, but launched with a
//! salt carrying the [`PTY_HOST_MARKER`] suffix (see [`host_salt`]) — the daemon proxies to it,
//! and a takeover is just "the successor re-attaches to the same pty-host" — no handle ever
//! crosses a process boundary. Because the host outlives daemon upgrades it is an *older build*
//! by design, so that link runs
//! [`VersionPolicy::Tolerant`](crate::session::daemon_client::VersionPolicy) against a frozen
//! host surface.

#![cfg(windows)]

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

use crate::session::proto::{ClientMsg, DaemonMsg, SessionMeta, MAX_FRAME_LEN, PROTO_VER};
use crate::session_manager::{SessionEvent, SessionManager};

// Re-use the same idle grace knob as the unix side (the env override is platform-agnostic).
const DEFAULT_IDLE_GRACE_MS: u64 = 30_000;

fn idle_grace() -> Duration {
    let ms = std::env::var("HYPERPANES_DAEMON_IDLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_IDLE_GRACE_MS);
    Duration::from_millis(ms)
}

/// FNV-1a (64-bit) — identical to the unix `daemon_names` hash (and the single-instance
/// gate's), so a Windows daemon and client derive the same salted pipe.
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The salted named-pipe path a daemon for `salt` binds — the Windows analog of the unix
/// `socket_path_for`. One endpoint per salt; the client connects to the SAME name.
pub fn pipe_name(salt: &str) -> String {
    let h = format!("{:016x}", fnv1a64(salt));
    format!(r"\\.\pipe\hyperpanesd.{h}")
}

/// Suffix that turns a salt into its **pty-host** salt.
///
/// The pty-host is not a new kind of process: it is a daemon like any other, run as
/// `hyperpanes --session-daemon <salt><marker>`, and it is what actually owns the ConPTYs. A
/// daemon whose salt carries this marker serves an in-process
/// [`SessionRegistry`](crate::session_manager::SessionRegistry); one whose salt does not
/// proxies to the host. So the marker is the whole of the mode switch, and [`pipe_name`] gives
/// the two roles distinct endpoints for free (it hashes the salt).
///
/// `\u{1}` cannot occur in the user-data path a salt is derived from, so no real salt can be
/// mistaken for a host salt.
const PTY_HOST_MARKER: &str = "\u{1}pty-host";

/// The salt of the pty-host serving `salt`'s terminals.
pub fn host_salt(salt: &str) -> String {
    format!("{salt}{PTY_HOST_MARKER}")
}

/// Whether `salt` names a pty-host rather than a user-facing daemon.
pub fn is_host_salt(salt: &str) -> bool {
    salt.ends_with(PTY_HOST_MARKER)
}

/// Shared M3 lifecycle (mirror of the unix `Lifecycle`): connection counter + shutdown latch.
/// On Windows there is no socket FILE to unlink (a named pipe vanishes when its last instance
/// closes), so teardown is just "stop accepting + exit".
struct Lifecycle {
    active_conns: AtomicU64,
    shutting_down: AtomicBool,
}

impl Lifecycle {
    fn new() -> Self {
        Lifecycle {
            active_conns: AtomicU64::new(0),
            shutting_down: AtomicBool::new(false),
        }
    }
    fn conn_opened(&self) {
        self.active_conns.fetch_add(1, Ordering::SeqCst);
    }
    fn conn_closed(&self) {
        self.active_conns.fetch_sub(1, Ordering::SeqCst);
    }
    fn conn_count(&self) -> u64 {
        self.active_conns.load(Ordering::SeqCst)
    }
    fn begin_shutdown(&self) -> bool {
        !self.shutting_down.swap(true, Ordering::SeqCst)
    }
    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }
}

/// Run the Windows daemon for `salt`, blocking until exit — the `#[cfg(windows)]` body behind
/// `hyperpanes --session-daemon <salt>`. Builds a Tokio runtime (pty drivers need it), claims
/// the salt's pipe as its **first instance** (the one-daemon-per-salt gate), arms the idle
/// monitor, then serves forever.
///
/// Binding happens BEFORE anything else so a losing race costs nothing: `AddrInUse` means an
/// incumbent already serves this salt, and the caller (a client that spawned us) simply keeps
/// connecting to whoever won.
pub fn run(salt: &str) -> io::Result<()> {
    let pipe = pipe_name(salt);
    // A pty-host never takes another host's place: it owns live ConPTYs, so an incumbent host
    // is precisely what we must leave alone. Losing the bind means one is already there, and
    // that is a success for whoever spawned us.
    let host_mode = is_host_salt(salt);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let server = match bind_first_instance(&pipe) {
            Ok(server) => server,
            // An incumbent daemon holds this salt. If it is an older build of us, take its
            // place WITHOUT disturbing the pty-host underneath — that is the whole Windows
            // live upgrade. (These few calls block the runtime; nothing else is running on it
            // yet, and they are bounded by `TAKEOVER_BUDGET`.)
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && !host_mode => {
                request_takeover(&pipe)?;
                bind_when_released(&pipe, TAKEOVER_BUDGET)?
            }
            Err(e) => return Err(e),
        };
        let daemon = Daemon::new(salt)?;
        daemon.start_idle_monitor(idle_grace());
        daemon.serve(&pipe, server).await
    })
}

/// How long a successor waits for the incumbent to answer the takeover and release the pipe.
/// Covers the incumbent's ack, its exit, and the OS reclaiming the name.
const TAKEOVER_BUDGET: Duration = Duration::from_secs(5);

/// How long to wait for the incumbent's takeover ack before giving up on it.
const TAKEOVER_RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Ask the daemon currently serving `pipe` to stand down and leave the pty-host running.
///
/// The Windows peer of the unix `daemon::take_over` — but where unix must ship the pty master
/// descriptors across the connection (`SCM_RIGHTS`), here **nothing is transferred**: the
/// terminals live in the pty-host, which both the incumbent and we connect to. The ack is only
/// a receipt (the incumbent's session list, for the log) proving the request was understood;
/// an incumbent that closes without answering leaves the caller to fall back on the
/// session-destroying tear-down, exactly as on unix.
fn request_takeover(pipe: &str) -> io::Result<()> {
    use crate::session::transport::{self, Endpoint};

    let endpoint = Endpoint::new(pipe);
    let mut conn = transport::connect(&endpoint)?;
    crate::session::proto::write_frame(&mut conn, &ClientMsg::Takeover)?;
    match transport::read_frame_deadline::<DaemonMsg>(&conn, TAKEOVER_RECV_TIMEOUT)? {
        Some(DaemonMsg::Sessions(sessions)) => {
            crate::session::daemon_client::dbg(&format!(
                "takeover: incumbent stood down, {} session(s) stay in the pty-host",
                sessions.len()
            ));
            Ok(())
        }
        _ => Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "the daemon holding this salt did not answer the takeover",
        )),
    }
}

/// Poll for the pipe name until it is ours or `budget` elapses — the peer of the unix
/// `acquire_when_released`. The name frees when the incumbent's last handle closes, which is
/// when it exits, so winning here is also the proof it is gone.
fn bind_when_released(pipe: &str, budget: Duration) -> io::Result<NamedPipeServer> {
    let deadline = Instant::now() + budget;
    loop {
        match bind_first_instance(pipe) {
            Ok(server) => return Ok(server),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
}

/// `ERROR_ACCESS_DENIED` — what `CreateNamedPipe` returns for a `first_pipe_instance(true)`
/// create when some other process already owns the name.
const ERROR_ACCESS_DENIED: i32 = 5;

/// Claim `pipe` as its first (and therefore only) server — the Windows one-daemon-per-salt
/// gate, the peer of the unix `flock`. Reports an incumbent as [`io::ErrorKind::AddrInUse`],
/// which is exactly what the unix side reports when the lock is already held, so callers need
/// no cfg to tell "I lost the race" from "the bind genuinely failed".
///
/// Must be called from inside a Tokio runtime context (the pipe registers with the reactor).
fn bind_first_instance(pipe: &str) -> io::Result<NamedPipeServer> {
    match ServerOptions::new().first_pipe_instance(true).create(pipe) {
        Ok(server) => Ok(server),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("a hyperpanes daemon already serves {pipe}"),
        )),
        Err(e) => Err(e),
    }
}

/// Kill the running Windows daemon for `salt` (the `--kill-daemon` path): connect + send
/// `Shutdown`. No-op if the pipe isn't there. Mirrors the unix `kill_daemon`.
pub fn kill_daemon(salt: &str) -> io::Result<bool> {
    let pipe = pipe_name(salt);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        match ClientOptions::new().open(&pipe) {
            Ok(mut client) => {
                let bytes = frame_bytes(&ClientMsg::Shutdown)?;
                client.write_all(&bytes).await?;
                client.flush().await?;
                let _ = client.shutdown().await;
                Ok(true)
            }
            // No server listening → nothing to kill.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    })
}

/// The running Windows daemon — the same shape as the unix `Daemon` (a `SessionRegistry`, an
/// event broadcast bus, a cwd cache, the lifecycle), adapted to the async pipe transport.
#[derive(Clone)]
struct Daemon {
    /// Where the terminals actually are. In a **pty-host** this is an in-process
    /// [`SessionRegistry`](crate::session_manager::SessionRegistry) owning the ConPTYs; in a
    /// user-facing daemon it is a client of that host. Same API either way, so everything
    /// below is written once.
    sessions: SessionManager,
    bus: tokio::sync::broadcast::Sender<SessionEvent>,
    cwds: Arc<Mutex<std::collections::HashMap<String, String>>>,
    lifecycle: Arc<Lifecycle>,
    /// Wakes the accept loop when a control path (`Shutdown`/`Takeover`) ends the daemon, so
    /// it stops waiting on `connect()` and returns — which drops every pipe instance and frees
    /// the name for a successor. The alternative, `process::exit` from inside a connection
    /// task, would also work in production but makes the stand-down path untestable.
    stopped: Arc<tokio::sync::Notify>,
}

impl Daemon {
    /// Build the daemon for `salt`, wiring its backend according to the role the salt names.
    ///
    /// A **pty-host** salt gets an in-process registry: it is the process that calls
    /// `CreatePseudoConsole`, and it must outlive us. Any other salt gets a *tolerant* client
    /// of that host (spawning it if it isn't up yet) — tolerant because after an upgrade the
    /// host is, by design, an older build than we are. That is the whole Windows answer to
    /// "upgrade without dropping terminals": an `HPCON` cannot cross a process boundary, so
    /// instead the process holding it never has to.
    fn new(salt: &str) -> io::Result<Self> {
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let sessions = if is_host_salt(salt) {
            SessionManager::new(etx)
        } else {
            SessionManager::new_daemon_tolerant(etx, &host_salt(salt))?
        };
        let (bus, _) = tokio::sync::broadcast::channel::<SessionEvent>(4096);
        let cwds: Arc<Mutex<std::collections::HashMap<String, String>>> = Arc::default();

        // Event pump: backend mpsc → cwd cache + broadcast bus (identical to the unix pump).
        let bus_tx = bus.clone();
        let cwds_pump = Arc::clone(&cwds);
        tokio::spawn(async move {
            while let Some(ev) = erx.recv().await {
                if let SessionEvent::Cwd { uid, cwd } = &ev {
                    cwds_pump.lock().unwrap().insert(uid.clone(), cwd.clone());
                }
                let _ = bus_tx.send(ev);
            }
        });

        Ok(Daemon {
            sessions,
            bus,
            cwds,
            lifecycle: Arc::new(Lifecycle::new()),
            stopped: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Idle-exit monitor (mirror of the unix one): 0 sessions AND 0 clients through the grace
    /// → exit. On Windows we just `process::exit(0)` (no socket file to unlink).
    fn start_idle_monitor(&self, grace: Duration) {
        let lifecycle = Arc::clone(&self.lifecycle);
        let sessions = self.sessions.clone();
        std::thread::spawn(move || {
            let mut armed: Option<Instant> = None;
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let idle = sessions.uids().is_empty() && lifecycle.conn_count() == 0;
                if !idle {
                    armed = None;
                    continue;
                }
                match armed {
                    None => armed = Some(Instant::now()),
                    Some(since) if since.elapsed() >= grace => {
                        if lifecycle.begin_shutdown() {
                            sessions.kill_all();
                            // Take the pty-host with us: idle means there are no terminals
                            // anywhere below us either, so leaving it up would strand a
                            // process. (A no-op in a host, which has no daemon of its own.)
                            sessions.shutdown_daemon();
                            std::process::exit(0);
                        }
                        break;
                    }
                    Some(_) => {}
                }
            }
        });
    }

    /// Accept connections forever over the named pipe, serving each as its own task. Takes the
    /// already-claimed first instance from [`bind_first_instance`] (the gate has to happen
    /// before the daemon is built, so the loser exits without having spawned pty machinery),
    /// and re-arms the next instance before handing the current one off — the
    /// `single_instance::windows::run_server` pattern — so a connect arriving during a handoff
    /// is never refused.
    async fn serve(&self, pipe: &str, mut server: NamedPipeServer) -> io::Result<()> {
        loop {
            if self.lifecycle.is_shutting_down() {
                return Ok(());
            }
            // Wait for a client — or for a control path to end us, which must not have to
            // wait for one more connection to arrive.
            let accepted = tokio::select! {
                r = server.connect() => r,
                _ = self.stopped.notified() => return Ok(()),
            };
            if accepted.is_err() {
                server = ServerOptions::new().create(pipe)?;
                continue;
            }
            let connected = std::mem::replace(
                &mut server,
                ServerOptions::new().create(pipe)?, // pre-arm the next instance
            );
            let daemon = self.clone();
            tokio::spawn(async move {
                daemon.handle_connection(connected).await;
            });
        }
    }

    /// Serve one client over a connected pipe instance. Mirrors the unix `handle_connection`:
    /// read+dispatch `ClientMsg`s on this task while a sibling task forwards broadcast events
    /// for the uids this connection attached to. (Single-task simplification vs the unix
    /// two-thread split — the async pipe is full-duplex, so we can `tokio::select!` between the
    /// inbound frames and the bus on one task; the buffering shim feeds `read_frame`.)
    async fn handle_connection(&self, conn: NamedPipeServer) {
        self.lifecycle.conn_opened();
        let mut pc = PipeConn::new(conn);
        let mut bus_rx = self.bus.subscribe();
        let mut attached: std::collections::HashSet<String> = Default::default();

        loop {
            tokio::select! {
                // Inbound client request.
                msg = pc.read_msg() => {
                    match msg {
                        Ok(Some(m)) => {
                            if !self.dispatch(m, &mut pc, &mut attached).await {
                                break; // a control path (Shutdown) asked to close
                            }
                        }
                        Ok(None) | Err(_) => break, // EOF / error → connection done
                    }
                }
                // Outbound broadcast event for an attached uid.
                ev = bus_rx.recv() => {
                    if let Ok(ev) = ev {
                        if attached.contains(event_uid(&ev))
                            && pc.write_msg(&DaemonMsg::Event(ev)).await.is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
        self.lifecycle.conn_closed();
    }

    /// Dispatch one `ClientMsg` (mirror of the unix `dispatch`). Returns `false` to close.
    async fn dispatch(
        &self,
        msg: ClientMsg,
        pc: &mut PipeConn,
        attached: &mut std::collections::HashSet<String>,
    ) -> bool {
        match msg {
            ClientMsg::Hello { .. } => {
                let _ = pc
                    .write_msg(&DaemonMsg::Hello {
                        proto_ver: PROTO_VER,
                        daemon_pid: std::process::id(),
                    })
                    .await;
            }
            ClientMsg::ListSessions => {
                let _ = pc
                    .write_msg(&DaemonMsg::Sessions(self.list_sessions()))
                    .await;
            }
            ClientMsg::Attach { uid } => {
                // Subscribe first, then snapshot the buffer AND the output cursor together
                // (`replay_with_cursor` reads both under the replay lock) so a client can
                // splice the live stream onto the seed without painting the overlap twice —
                // see the unix daemon's `Attach` arm and `DaemonMsg::Replay::cursor`.
                attached.insert(uid.clone());
                let (data, cursor) = self.sessions.replay_with_cursor(&uid).unwrap_or_default();
                let _ = pc.write_msg(&DaemonMsg::Replay { uid, data, cursor }).await;
            }
            ClientMsg::Create(spec) => {
                let uid = spec
                    .uid
                    .clone()
                    .unwrap_or_else(|| self.sessions.fresh_uid());
                attached.insert(uid.clone());
                let opts = spec.into_options(uid.clone());
                let created = self.sessions.create(opts);
                let _ = pc.write_msg(&DaemonMsg::Created { uid: uid.clone() }).await;
                if created.is_err() {
                    let _ = self.bus.send(SessionEvent::Exit { uid, code: -1 });
                }
            }
            ClientMsg::Write { uid, data } => self.sessions.write(&uid, &data),
            ClientMsg::Resize { uid, cols, rows } => self.sessions.resize(&uid, cols, rows),
            ClientMsg::Kill { uid } => {
                self.sessions.kill(&uid);
                self.cwds.lock().unwrap().remove(&uid);
            }
            ClientMsg::KillAll => {
                self.sessions.kill_all();
                self.cwds.lock().unwrap().clear();
            }
            ClientMsg::RenderScreen { uid } => {
                let text = self.sessions.render_screen(&uid);
                let _ = pc.write_msg(&DaemonMsg::Screen { uid, text }).await;
            }
            ClientMsg::Ping => {
                let _ = pc.write_msg(&DaemonMsg::Pong).await;
            }
            ClientMsg::Takeover => {
                // Stand down for a successor built from a newer binary. Where unix ships its
                // pty master descriptors over `SCM_RIGHTS` at this point, Windows transfers
                // nothing: the terminals are ConPTYs in the pty-host, which stays up and which
                // the successor connects to itself. So the ack is just a receipt, and the one
                // thing that matters is what we DON'T do on the way out — no `kill_all`, no
                // `shutdown_daemon`. Either would destroy exactly the terminals this path
                // exists to save.
                let sessions = self.list_sessions();
                let _ = pc.write_msg(&DaemonMsg::Sessions(sessions)).await;
                if self.lifecycle.begin_shutdown() {
                    self.stopped.notify_waiters();
                }
                // Closing this connection releases our pipe instances; the successor is
                // polling for the name and takes it the moment the process goes.
                return false;
            }
            ClientMsg::Shutdown => {
                if self.lifecycle.begin_shutdown() {
                    self.sessions.kill_all();
                    // An explicit shutdown means "stop everything" — unlike `Takeover`, take
                    // the pty-host down with us. There is no socket file to unlink on Windows
                    // (a named pipe vanishes with its last instance), so ending the accept
                    // loop is the whole teardown.
                    self.sessions.shutdown_daemon();
                    self.stopped.notify_waiters();
                }
                return false;
            }
        }
        true
    }

    fn list_sessions(&self) -> Vec<SessionMeta> {
        let cwds = self.cwds.lock().unwrap();
        self.sessions
            .uids()
            .into_iter()
            .map(|uid| {
                // The grid rides along so an attach client can letterbox at the desktop's
                // size instead of reflowing the pane (mux-backend-plan M2).
                let (cols, rows) = match self.sessions.dims(&uid) {
                    Some((c, r)) => (Some(c), Some(r)),
                    None => (None, None),
                };
                SessionMeta {
                    cwd: cwds.get(&uid).cloned(),
                    output_bytes: self.sessions.output_bytes(&uid).unwrap_or(0),
                    last_output_at: self.sessions.last_output_at(&uid),
                    alive: true,
                    cols,
                    rows,
                    uid,
                }
            })
            .collect()
    }
}

fn event_uid(ev: &SessionEvent) -> &str {
    match ev {
        SessionEvent::Data { uid, .. }
        | SessionEvent::Cwd { uid, .. }
        | SessionEvent::Exit { uid, .. }
        | SessionEvent::CommandStart { uid }
        | SessionEvent::CommandEnd { uid, .. }
        | SessionEvent::PromptReady { uid }
        | SessionEvent::AgentState { uid, .. } => uid,
    }
}

/// A buffering shim that decodes the proto's length-prefixed frames out of an async named
/// pipe. The proto's `read_frame`/`write_frame` want synchronous `Read`/`Write`; rather than
/// fight that, we re-implement the (tiny, identical) framing directly over the async pipe.
///
/// Only the SERVER side needs this. The client opens the pipe as an ordinary blocking
/// [`File`](std::fs::File) (see [`session::transport`](crate::session::transport)) and shares
/// the proto's synchronous `read_frame`/`write_frame` verbatim; it is the async server that
/// cannot.
struct PipeConn {
    pipe: NamedPipeServer,
    buf: Vec<u8>,
}

impl PipeConn {
    fn new(pipe: NamedPipeServer) -> Self {
        PipeConn {
            pipe,
            buf: Vec::with_capacity(8192),
        }
    }

    /// Read the next framed `ClientMsg`, accumulating bytes until a whole frame is buffered.
    /// `Ok(None)` on a clean EOF between frames.
    async fn read_msg(&mut self) -> io::Result<Option<ClientMsg>> {
        loop {
            // Do we already have a complete frame buffered?
            if let Some(msg) = self.try_decode()? {
                return Ok(Some(msg));
            }
            let mut tmp = [0u8; 4096];
            match self.pipe.read(&mut tmp).await {
                Ok(0) => {
                    return if self.buf.is_empty() {
                        Ok(None) // clean EOF between frames
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "EOF mid-frame",
                        ))
                    };
                }
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// Try to pull one complete frame out of `buf` (a `u32` LE length then that many JSON
    /// bytes). Returns `Ok(None)` if not enough is buffered yet.
    fn try_decode(&mut self) -> io::Result<Option<ClientMsg>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        if len > MAX_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame exceeds MAX_FRAME_LEN",
            ));
        }
        let total = 4 + len as usize;
        if self.buf.len() < total {
            return Ok(None);
        }
        let body = self.buf[4..total].to_vec();
        self.buf.drain(..total);
        let msg = serde_json::from_slice(&body).map_err(io::Error::other)?;
        Ok(Some(msg))
    }

    /// Write one framed `DaemonMsg`.
    async fn write_msg(&mut self, msg: &DaemonMsg) -> io::Result<()> {
        let bytes = frame_bytes(msg)?;
        self.pipe.write_all(&bytes).await?;
        self.pipe.flush().await
    }
}

/// Serialize a message into a length-prefixed frame (the same wire shape as
/// `proto::write_frame`, materialized as bytes for the async writers here).
fn frame_bytes(msg: &impl serde::Serialize) -> io::Result<Vec<u8>> {
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
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::proto::{write_frame, SpawnSpec};
    use crate::session::transport::{self, Conn, Endpoint};

    /// A salt unique per test AND per run (pid + thread id), so parallel and repeated runs
    /// never collide on a pipe name.
    fn test_salt(tag: &str) -> String {
        format!(
            "hp-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )
    }

    /// Start a daemon for `salt` on the current runtime and hand back a handle to it. The
    /// production path (`run`) owns its own runtime and blocks; tests need the accept loop as
    /// a task so they can drive a client from the same thread.
    async fn start(salt: &str) -> Daemon {
        let pipe = pipe_name(salt);
        let server = bind_first_instance(&pipe).expect("first instance");
        let daemon = Daemon::new(salt).expect("daemon");
        let serving = daemon.clone();
        tokio::spawn(async move {
            let _ = serving.serve(&pipe, server).await;
        });
        daemon
    }

    /// Blocking client round-trip helpers. The client side is the shared `transport` +
    /// `proto` code — the same bytes `DaemonSessionManager` sends — so these exercise the real
    /// wire, not a test-only shim.
    fn client(salt: &str) -> Conn {
        transport::connect(&Endpoint::new(pipe_name(salt))).expect("connect")
    }

    fn send(conn: &mut Conn, msg: &ClientMsg) {
        write_frame(conn, msg).expect("write frame");
    }

    /// Read frames until one satisfies `want`, or the budget runs out. Events stream in
    /// alongside replies, so a caller looking for a reply has to skip past them.
    fn recv_until(
        conn: &Conn,
        budget: Duration,
        want: impl Fn(&DaemonMsg) -> bool,
    ) -> Option<DaemonMsg> {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            match transport::read_frame_deadline::<DaemonMsg>(conn, Duration::from_millis(250)) {
                Ok(Some(msg)) if want(&msg) => return Some(msg),
                Ok(Some(_)) => continue,
                Ok(None) => continue,
                Err(_) => return None,
            }
        }
        None
    }

    /// Accumulate `Data` payloads until the combined output contains `needle`. A ConPTY is
    /// free to split its output across writes, so a single event is not guaranteed to hold a
    /// whole word — asserting on one would be flaky.
    fn output_contains(conn: &Conn, budget: Duration, needle: &str) -> bool {
        let deadline = Instant::now() + budget;
        let mut seen = String::new();
        while Instant::now() < deadline {
            match transport::read_frame_deadline::<DaemonMsg>(conn, Duration::from_millis(250)) {
                Ok(Some(DaemonMsg::Event(SessionEvent::Data { data, .. }))) => {
                    seen.push_str(&data);
                    if seen.contains(needle) {
                        return true;
                    }
                }
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
        false
    }

    /// Poll `ListSessions` until `uid` is gone. `Kill` is fire-and-forget, so the reply to a
    /// list sent right behind it may still describe the pre-kill world.
    fn wait_until_gone(conn: &mut Conn, uid: &str, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            send(conn, &ClientMsg::ListSessions);
            let listed = recv_until(conn, Duration::from_secs(2), |m| {
                matches!(m, DaemonMsg::Sessions(_))
            });
            if let Some(DaemonMsg::Sessions(s)) = listed {
                if s.iter().all(|m| m.uid != uid) {
                    return true;
                }
            }
        }
        false
    }

    // A host salt is recognizable, round-trips, and lands on a DIFFERENT pipe than the daemon
    // it serves — the two roles must never contend for one name.
    #[test]
    fn host_salt_is_distinguishable_and_separately_addressed() {
        let salt = "C:\\Users\\x\\AppData\\Roaming\\hyperpanes";
        let host = host_salt(salt);

        assert!(!is_host_salt(salt), "a plain salt is not a host salt");
        assert!(is_host_salt(&host), "a host salt is recognized");
        assert!(host.starts_with(salt), "the host salt extends the salt");
        assert_ne!(
            pipe_name(salt),
            pipe_name(&host),
            "daemon and pty-host must bind different pipes"
        );
        assert!(
            !is_host_salt(&pipe_name(&host)),
            "the marker lives in the salt, not in the derived pipe name"
        );
    }

    // The one-daemon-per-salt gate: `first_pipe_instance` grants the name to exactly one
    // server and reports every other as AddrInUse — the Windows peer of the unix flock, and
    // what tells a spawned successor it must take over rather than bind.
    #[tokio::test(flavor = "multi_thread")]
    async fn first_instance_gate_admits_one_daemon_per_salt() {
        let pipe = pipe_name(&test_salt("gate"));
        let first = bind_first_instance(&pipe).expect("first server claims the name");

        let second = bind_first_instance(&pipe);
        assert_eq!(
            second.err().map(|e| e.kind()),
            Some(io::ErrorKind::AddrInUse),
            "a second server on the same name is refused as AddrInUse"
        );

        // Released with the incumbent's last handle — which is how a successor gets in.
        drop(first);
        assert!(
            bind_first_instance(&pipe).is_ok(),
            "the name is claimable again once the incumbent lets it go"
        );
    }

    // End-to-end over the real pipe: handshake, spawn a ConPTY, drive it, see its output
    // stream back, and kill it. This is the whole daemon contract on Windows.
    #[tokio::test(flavor = "multi_thread")]
    async fn daemon_spawns_a_conpty_and_streams_its_output() {
        let salt = test_salt("e2e");
        // A host salt: this daemon owns the ConPTYs itself, with no pty-host to spawn.
        let salt = host_salt(&salt);
        let _daemon = start(&salt).await;

        let mut conn = client(&salt);
        send(
            &mut conn,
            &ClientMsg::Hello {
                proto_ver: PROTO_VER,
            },
        );
        let hello = recv_until(&conn, Duration::from_secs(5), |m| {
            matches!(m, DaemonMsg::Hello { .. })
        })
        .expect("daemon answers the handshake");
        match hello {
            DaemonMsg::Hello { proto_ver, .. } => assert_eq!(proto_ver, PROTO_VER),
            other => panic!("expected Hello, got {other:?}"),
        }

        // `cmd /c echo` is the smallest thing that proves a real console was allocated and
        // its output made it back through the ConPTY, the registry, the bus and the pipe.
        send(
            &mut conn,
            &ClientMsg::Create(SpawnSpec {
                uid: Some("pane-e2e".into()),
                shell: Some("cmd.exe".into()),
                args: Some(vec!["/c".into(), "echo hyperpanes-ok".into()]),
                cols: Some(80),
                rows: Some(24),
                ..Default::default()
            }),
        );
        let created = recv_until(&conn, Duration::from_secs(5), |m| {
            matches!(m, DaemonMsg::Created { .. })
        })
        .expect("daemon acks the create");
        assert!(matches!(created, DaemonMsg::Created { uid } if uid == "pane-e2e"));

        assert!(
            output_contains(&conn, Duration::from_secs(10), "hyperpanes-ok"),
            "the ConPTY's output should stream back as Data events"
        );

        send(
            &mut conn,
            &ClientMsg::Kill {
                uid: "pane-e2e".into(),
            },
        );
        assert!(
            wait_until_gone(&mut conn, "pane-e2e", Duration::from_secs(10)),
            "a killed session drops out of the session list"
        );
    }

    // The Windows live upgrade (M1), in miniature. A daemon proxying to a pty-host stands
    // down on `Takeover` — and the sessions, which live in the HOST, are untouched. That is
    // the whole reason the ConPTYs are not in the daemon: nothing has to be handed over,
    // because nothing moves.
    #[tokio::test(flavor = "multi_thread")]
    async fn takeover_stands_the_daemon_down_and_leaves_the_terminals_running() {
        let salt = test_salt("takeover");
        // Start the pty-host FIRST so the daemon's `connect_or_spawn` finds it live and never
        // tries to spawn `current_exe` (which, in a test binary, is not the app).
        let host = host_salt(&salt);
        let _pty_host = start(&host).await;
        let daemon = start(&salt).await;

        // Create a terminal through the daemon; it is really created in the host.
        let mut conn = client(&salt);
        send(
            &mut conn,
            &ClientMsg::Create(SpawnSpec {
                uid: Some("pane-survivor".into()),
                shell: Some("cmd.exe".into()),
                cols: Some(80),
                rows: Some(24),
                ..Default::default()
            }),
        );
        assert!(
            recv_until(&conn, Duration::from_secs(5), |m| matches!(
                m,
                DaemonMsg::Created { .. }
            ))
            .is_some(),
            "the daemon creates the session in the pty-host"
        );

        // A successor asks the incumbent to stand down.
        let mut upgrade = client(&salt);
        send(&mut upgrade, &ClientMsg::Takeover);
        let ack = recv_until(&upgrade, Duration::from_secs(5), |m| {
            matches!(m, DaemonMsg::Sessions(_))
        })
        .expect("the incumbent acknowledges the takeover");
        assert!(
            matches!(&ack, DaemonMsg::Sessions(s) if s.iter().any(|m| m.uid == "pane-survivor")),
            "the ack reports what the successor is inheriting: {ack:?}"
        );
        assert!(
            daemon.lifecycle.is_shutting_down(),
            "the incumbent stands down after acknowledging"
        );

        // The crux: ask the PTY-HOST directly. The terminal is still there.
        let mut host_conn = client(&host);
        send(&mut host_conn, &ClientMsg::ListSessions);
        let listed = recv_until(&host_conn, Duration::from_secs(5), |m| {
            matches!(m, DaemonMsg::Sessions(_))
        })
        .expect("the pty-host answers");
        assert!(
            matches!(listed, DaemonMsg::Sessions(s) if s.iter().any(|m| m.uid == "pane-survivor")),
            "the takeover must NOT touch the terminals living in the pty-host"
        );

        // And the successor can now claim the name the incumbent gave up.
        drop(conn);
        drop(upgrade);
        assert!(
            bind_when_released(&pipe_name(&salt), Duration::from_secs(5)).is_ok(),
            "the stood-down daemon releases its pipe for the successor"
        );
    }
}
