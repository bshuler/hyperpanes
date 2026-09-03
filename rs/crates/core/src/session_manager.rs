//! Port of `src/main/session.ts` + `src/main/session-manager.ts` — the CENTRAL owner
//! of live sessions (`Map<uid, Session>`): create / get / write / resize / kill /
//! killAll. In one Rust process all PTYs live here and windows/panes just reference
//! `uid`s (no Electron broadcast-to-all-windows model — this is what simplifies
//! multi-window re-attach).
//!
//! A `Session` ties pty → cwd-sniff (`session::cwd`, on the RAW chunk pre-batch) →
//! `batcher` → `replay` (+ a live `screen` for `mode:"screen"` reads), emitting
//! Data / Cwd / Exit, and tracks `last_output_at` + a monotonic `output_bytes` cursor
//! (UTF-16 units) for the control read-path.
//!
//! # Wave-2 contract (the control server consumes this)
//! * [`SessionManager::create`] spawns a pty and starts its driver task; events arrive
//!   on the [`SessionEvent`] channel passed to [`SessionManager::new`].
//! * Read-path accessors are synchronous and cheap: [`SessionManager::replay`],
//!   [`SessionManager::output_bytes`] (UTF-16 monotonic cursor — pair with
//!   `control::output::sliceSince`), [`SessionManager::last_output_at`] (epoch ms, for
//!   `control::output::waitDecision`), and [`SessionManager::render_screen`].
//! * Mutators: [`write`](SessionManager::write) / [`resize`](SessionManager::resize) /
//!   [`kill`](SessionManager::kill) / [`kill_all`](SessionManager::kill_all).
//!
//! Must be called inside a Tokio runtime — `create` spawns the per-session driver task.
//!
//! ## Design note: clocks
//! The batcher's 16 ms timer runs on a **monotonic** clock (driver-local), while
//! `last_output_at` is an **epoch-ms** stamp so a control server can compare it against
//! its own wall clock exactly as the TS used `Date.now()`. The two never mix.
//!
//! ## Design note: shell integration
//! The injection side of shell integration lives in another track (`shell_integration`,
//! still a stub). To stay decoupled, `create` takes the resolved [`Integration`]
//! (extra args + env) as an *input* rather than calling that module — the wiring layer
//! supplies it. When absent, a plain interactive shell is spawned (additive no-op).

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::session::batcher::DataBatcher;
use crate::session::cwd::parse_osc_cwd;
use crate::session::pty::{spawn_pty, Pty, PtyEvent, PtySpec};
use crate::session::replay::Replay;
use crate::session::screen::Screen;
use crate::session::spawn::{
    build_env, default_shell, resolve_control_file, resolve_spawn, resolve_windows_command,
    EnvInputs, EnvMap,
};

/// Process-global counter for the in-process backend's `pane-N` uids (see
/// [`SessionManager::fresh_uid`]). Must be process-global (not per-manager) so two windows
/// sharing the one in-process `SessionManager` never mint the same `pane-0` — the historical
/// collision the GUI's own `state.rs` counter was hardened against; minting here keeps that
/// invariant for the daemon scheme too.
static NEXT_INPROC_UID: AtomicU64 = AtomicU64::new(0);

/// `pane-0`, `pane-1`, … — the in-process uid scheme (PTYs die with the GUI, so per-run
/// uniqueness suffices). The daemon scheme is a UUID for cross-run uniqueness; see
/// [`SessionManager::fresh_uid`].
fn next_inproc_uid() -> String {
    format!("pane-{}", NEXT_INPROC_UID.fetch_add(1, Ordering::Relaxed))
}

/// An event emitted by a live session, delivered on the manager's event channel.
/// Mirrors the TS `SessionHandlers` callbacks (`onData` / `onCwd` / `onExit`).
///
/// `Serialize`/`Deserialize` so the session daemon (`session::proto`) can carry the
/// event verbatim to attached clients (`DaemonMsg::Event`) — the enum is a flat,
/// owned-data shape with no GUI/runtime types, so the wire form is the in-process form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEvent {
    /// A flushed (batched) output chunk. The renderer/control writes this to its
    /// terminal; it is also what the replay buffer and `output_bytes` accumulate.
    /// `cursor` is the monotonic `output_bytes` value AFTER this chunk (UTF-16 code
    /// units), computed atomically at flush time — remote clients use it to splice a
    /// live stream onto a `GET /output` snapshot without gaps or duplicates.
    /// `#[serde(default)]` keeps the daemon proto readable from pre-cursor peers.
    Data {
        uid: String,
        data: String,
        #[serde(default)]
        cursor: u64,
    },
    /// The pane's working directory changed (from an OSC 7 / OSC 9;9 sniff). De-duped:
    /// fires only on an actual change.
    Cwd { uid: String, cwd: String },
    /// The child exited with this code. Emitted on a *natural* exit only — a manual
    /// `kill` / `kill_all` is silent (mirrors TS `destroy()` gating `onExit`).
    Exit { uid: String, code: i32 },
    /// Phase-4 semantic markers (sniffed off the raw stream like cwd, then stripped).
    /// Additive — they ride the daemon proto verbatim via serde, and consumers that only
    /// care about output/cwd/exit ignore them.
    ///
    /// `133;C` — a command's output begins (a command is now running).
    CommandStart { uid: String },
    /// `133;D` / `133;D;<code>` — a command finished, optionally with its exit code.
    CommandEnd { uid: String, code: Option<i32> },
    /// `133;A` / `133;B` — the shell is at / drawing a prompt → ready for input.
    PromptReady { uid: String },
    /// `9;hp;state=…` — the program self-reports its liveness.
    AgentState {
        uid: String,
        state: AgentLiveness,
        code: Option<i32>,
    },
}

/// Serializable mirror of [`crate::session::osc133::AgentLiveness`] so the event can ride
/// the daemon proto. Kept in lockstep with the parser's enum via [`From`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentLiveness {
    Busy,
    AwaitingInput,
    Done,
    Error,
}

impl From<crate::session::osc133::AgentLiveness> for AgentLiveness {
    fn from(a: crate::session::osc133::AgentLiveness) -> Self {
        use crate::session::osc133::AgentLiveness as P;
        match a {
            P::Busy => AgentLiveness::Busy,
            P::AwaitingInput => AgentLiveness::AwaitingInput,
            P::Done => AgentLiveness::Done,
            P::Error => AgentLiveness::Error,
        }
    }
}

/// One live session's **transferable state** — everything a successor process needs to
/// re-create the session around a pty it did not spawn. The payload of the daemon live
/// upgrade (`docs/mux-backend-plan.md`, M1).
///
/// The pty itself cannot be serialized: its master descriptor travels *beside* this struct
/// as `SCM_RIGHTS` ancillary data (see [`session::handoff`](crate::session::handoff)), and
/// [`fd_index`](Self::fd_index) says which descriptor of that message belongs to this
/// session.
///
/// Serde-clean and owned, like [`SessionEvent`] — the wire form is the in-process form.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub uid: String,
    /// The rolling replay buffer verbatim. A client that re-attaches after the upgrade must
    /// see the same scrollback it would have seen without one.
    #[serde(default)]
    pub replay: String,
    /// The monotonic UTF-16 output cursor. This MUST carry across: a client holding a
    /// `since` cursor minted before the upgrade would otherwise be handed a rewound stream
    /// and would redraw output it has already drawn.
    #[serde(default)]
    pub cursor: u64,
    pub cols: u16,
    pub rows: u16,
    /// Last sniffed cwd, when the sender tracks one. The registry does not (it accumulates
    /// counters, not cwds — the daemon keeps that cache), so [`SessionRegistry::hand_off`]
    /// leaves this `None` for its caller to fill in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Index into the descriptor array of the message that carried this snapshot.
    #[serde(default)]
    pub fd_index: usize,
    /// The child's process-group leader at handoff time, when the platform reports one. The
    /// successor cannot `waitpid` an adopted child (it is reparented to init) but it can
    /// still signal this group. Advisory — see `session::adopt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgrp: Option<i32>,
}

/// Resolved shell-integration inputs for an interactive spawn: extra leading args and
/// env. Supplied by the wiring layer (the `shell_integration` track owns *producing*
/// these). Empty/`None` → a plain interactive shell.
#[derive(Debug, Clone, Default)]
pub struct Integration {
    pub args: Vec<String>,
    pub env: EnvMap,
}

/// Options to spawn a session — the port of TS `SpawnOptions`.
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    pub uid: String,
    /// Shell to launch; defaults to `session::spawn::default_shell()`.
    pub shell: Option<String>,
    /// Program argv. With `command` → the verbatim argv for a DIRECT (no-shell) spawn
    /// (P4a). Without `command` → args handed to the interactive shell.
    pub args: Option<Vec<String>>,
    /// A command to run (shell-wrapped unless `args` is also given). `None` → an
    /// interactive shell.
    pub command: Option<String>,
    pub cwd: Option<String>,
    /// Per-pane env override.
    pub env: Option<EnvMap>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    /// The owning pane's stable id → injected as `HYPERPANES_PANE_ID`.
    pub pane_id: Option<String>,
    /// Shell integration (interactive branch only). See [`Integration`].
    pub integration: Option<Integration>,
    /// Path to `control.json` (→ `HYPERPANES_CONTROL_FILE`, unless a scoped token is
    /// present). Supplied by the persistence/control wiring. `None` → not injected.
    pub control_file: Option<String>,
}

// Read-side state shared between the driver task (writer) and control reads (readers).
// Replay + screen are mutex-guarded; the counters are atomics.
struct Shared {
    replay: Mutex<Replay>,
    screen: Mutex<Screen>,
    /// Output flushed since the screen mirror was last brought up to date, buffered
    /// for a LAZY `Screen::advance`. The screen is the headless VTE mirror that only
    /// feeds `mode:"screen"` control reads (and the `awaitingInput` heuristic) — which
    /// are infrequent and on-demand. Parsing the full pty stream into it on EVERY flush
    /// (as the eager design did) double-parses the same bytes the GUI grid already
    /// parses, pure wasted CPU when no control client is reading the screen. Instead we
    /// stash flushed bytes here and drain them into `screen` only when a screen read
    /// actually happens (`sync_screen`). Correctness is identical — the screen is brought
    /// fully current at read time — but the hot path does zero VTE work for the mirror.
    screen_pending: Mutex<Vec<u8>>,
    /// Monotonic count of ALL output UTF-16 code units ever flushed (the `since`
    /// cursor basis). Never decreases.
    output_bytes: AtomicU64,
    /// Epoch-ms of the last flush, or 0 if no output yet.
    last_output_at: AtomicU64,
    /// Set by a manual kill so the natural-exit `Exit` event is suppressed.
    killed: AtomicBool,
    /// Phase-4 liveness mirror, fed by the OSC-133 / OSC-9;hp sniff so the *pull-based*
    /// activity ticker can read a pane's precise state cheaply (no per-tick scan). Gated
    /// on `marker_seen`: until a marker is ever seen, the legacy silence heuristic owns
    /// the activity, so an un-instrumented pane is byte-for-byte unchanged.
    ///
    /// `prompt_ready` — true after `133;A`/`133;D` or agent awaiting-input/done.
    prompt_ready: AtomicBool,
    /// `command_running` — true after `133;C` or agent busy; cleared by `133;D`/prompt.
    command_running: AtomicBool,
    /// Last `133;D` exit code (`i32::MIN` = none yet).
    last_exit_code: AtomicI32,
    /// Have we EVER seen a marker? Gates the fallback to the silence heuristic.
    marker_seen: AtomicBool,
}

/// A cheap snapshot of a session's phase-4 liveness mirror (the pull side of the marker
/// channel). `marker_seen == false` ⇒ the caller must fall back to the silence heuristic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Liveness {
    pub prompt_ready: bool,
    pub command_running: bool,
    pub last_exit_code: Option<i32>,
    pub marker_seen: bool,
}

impl Shared {
    /// A blank read-state for a `cols`x`rows` grid — the starting point for both a freshly
    /// spawned session and an adopted one (which then seeds replay/cursor on top).
    fn fresh(cols: u16, rows: u16) -> Arc<Self> {
        Arc::new(Shared {
            replay: Mutex::new(Replay::new()),
            screen: Mutex::new(Screen::new(cols, rows)),
            screen_pending: Mutex::new(Vec::new()),
            output_bytes: AtomicU64::new(0),
            last_output_at: AtomicU64::new(0),
            killed: AtomicBool::new(false),
            prompt_ready: AtomicBool::new(false),
            command_running: AtomicBool::new(false),
            last_exit_code: AtomicI32::new(i32::MIN),
            marker_seen: AtomicBool::new(false),
        })
    }

    /// Drain any buffered output into the screen mirror so a subsequent `screen.render()`
    /// reflects all flushed bytes. Cheap no-op when nothing is pending. Called on the
    /// read path (lazy) instead of on every flush (eager) — see `screen_pending`.
    fn sync_screen(&self) {
        // Take the pending buffer under its own lock first to minimize contention with
        // the driver thread's appends, then parse it into the screen.
        let pending = {
            let mut p = self.screen_pending.lock().unwrap();
            if p.is_empty() {
                return;
            }
            std::mem::take(&mut *p)
        };
        self.screen.lock().unwrap().advance(&pending);
    }

    /// Fold one parsed [`Marker`](crate::session::osc133::Marker) into the liveness mirror.
    /// Every marker flips `marker_seen`, which is what hands authority from the silence
    /// heuristic to the precise state for this pane (gate in `control::server::activity_for`).
    fn apply_marker(&self, m: &crate::session::osc133::Marker) {
        use crate::session::osc133::{AgentLiveness as A, Marker};
        self.marker_seen.store(true, Ordering::Relaxed);
        match m {
            Marker::CommandStart => {
                self.command_running.store(true, Ordering::Relaxed);
                self.prompt_ready.store(false, Ordering::Relaxed);
            }
            Marker::CommandEnd { code } => {
                self.command_running.store(false, Ordering::Relaxed);
                self.prompt_ready.store(true, Ordering::Relaxed);
                if let Some(c) = code {
                    self.last_exit_code.store(*c, Ordering::Relaxed);
                }
            }
            Marker::PromptReady => {
                self.command_running.store(false, Ordering::Relaxed);
                self.prompt_ready.store(true, Ordering::Relaxed);
            }
            Marker::Agent { state, code } => match state {
                A::Busy => {
                    self.command_running.store(true, Ordering::Relaxed);
                    self.prompt_ready.store(false, Ordering::Relaxed);
                }
                A::AwaitingInput | A::Done => {
                    self.command_running.store(false, Ordering::Relaxed);
                    self.prompt_ready.store(true, Ordering::Relaxed);
                }
                A::Error => {
                    self.command_running.store(false, Ordering::Relaxed);
                    self.prompt_ready.store(true, Ordering::Relaxed);
                    if let Some(c) = code {
                        self.last_exit_code.store(*c, Ordering::Relaxed);
                    }
                }
            },
        }
    }

    /// Snapshot the liveness mirror for the activity ticker.
    fn liveness(&self) -> Liveness {
        let raw = self.last_exit_code.load(Ordering::Relaxed);
        Liveness {
            prompt_ready: self.prompt_ready.load(Ordering::Relaxed),
            command_running: self.command_running.load(Ordering::Relaxed),
            last_exit_code: (raw != i32::MIN).then_some(raw),
            marker_seen: self.marker_seen.load(Ordering::Relaxed),
        }
    }

    /// Input was just sent → optimistically clear `prompt_ready` so the busy edge is
    /// reported without waiting for the next marker (tightens latency, never lies for
    /// long — a real prompt re-asserts `prompt_ready` on its next `133;A`).
    fn note_write(&self) {
        if self.marker_seen.load(Ordering::Relaxed) {
            self.prompt_ready.store(false, Ordering::Relaxed);
        }
    }
}

/// A sink the pty reader thread calls with each [`PtyEvent`]. `Arc`-wrapped so the
/// real spawn and the test mock can both hold and invoke it.
pub type EventSink = Arc<dyn Fn(PtyEvent) + Send + Sync>;

/// A factory that turns a spec + sink into a live pty. The default uses `spawn_pty`;
/// tests inject a mock so the async pipeline is exercised without ConPTY. `pub(crate)` so
/// the daemon backend ([`session::daemon_client`](crate::session::daemon_client)) can name
/// it in its mirroring `create_with` signature (it ignores the factory — a closure can't
/// cross a socket; the daemon owns real PTYs).
pub(crate) type SpawnFn = Box<dyn FnOnce(&PtySpec, EventSink) -> io::Result<Box<dyn Pty>> + Send>;

/// One live session: the pty handle plus the shared read state. The driver task runs
/// detached; dropping the `Session` drops the pty (closing its handles).
struct Session {
    /// `Arc` rather than `Box` so `write`/`resize` can clone the handle out of the map and
    /// release the registry lock BEFORE touching the pty: a blocking pty write must not
    /// stall every other session's reads and events behind it.
    pty: Arc<dyn Pty>,
    shared: Arc<Shared>,
}

/// The reusable, transport-agnostic per-uid session store + operations — the heart of
/// what was historically `SessionManager`, factored out so the **session daemon**
/// (`session::daemon`) can own the very same registry the in-process GUI owns through
/// [`SessionManager`]. Cheap to clone-share via `Arc`; `Clone` shares the same session
/// map + event sender + uid counter (a handle, not a copy), so spawn work can move onto
/// a worker thread (`Spawn` on Windows ConPTY can block ~1s for some shells — see
/// `docs/conpty-passthrough-investigation.md`).
///
/// Every method here is the literal body of the corresponding old `SessionManager`
/// method; `SessionManager` now delegates verbatim so its public API is unchanged.
///
/// ## Daemon-assignable uids
/// The daemon must be the source of truth for uids across GUI restarts (a re-attaching
/// pane references a session by uid — see the plan's "uid stability" note). [`mint_uid`]
/// hands out a process-unique `s{n}` token from a per-registry counter so the daemon can
/// allocate the uid itself when a client's [`proto::SpawnSpec`] left it blank. The GUI
/// path keeps minting uids exactly as before (it passes its own `uid` in `SpawnOptions`),
/// so this is purely additive.
#[derive(Clone)]
pub struct SessionRegistry {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    events: UnboundedSender<SessionEvent>,
    /// Monotonic uid source for daemon-assigned sessions (see [`mint_uid`]).
    next_uid: Arc<AtomicU64>,
}

impl SessionRegistry {
    /// Create a registry that emits [`SessionEvent`]s on `events`.
    pub fn new(events: UnboundedSender<SessionEvent>) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            events,
            next_uid: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Allocate a fresh process-unique uid (`s1`, `s2`, …). The daemon calls this when a
    /// client did not pin a uid, making the daemon the authoritative uid source.
    pub fn mint_uid(&self) -> String {
        format!("s{}", self.next_uid.fetch_add(1, Ordering::Relaxed))
    }

    /// Spawn a real pty session for `opts`. Returns once the pty is live and its driver
    /// task is running. Errors if the pty fails to spawn.
    pub fn create(&self, opts: SpawnOptions) -> io::Result<()> {
        let factory: SpawnFn = Box::new(|spec, sink| spawn_pty(spec, move |ev| sink(ev)));
        self.create_with(opts, factory)
    }

    /// Spawn a session using a custom pty `factory` (tests inject a mock). The resolved
    /// [`PtySpec`] is built from `opts` exactly as the production path does.
    pub fn create_with(&self, opts: SpawnOptions, factory: SpawnFn) -> io::Result<()> {
        let spec = build_spec(&opts);

        let (ptx, prx) = unbounded_channel::<PtyEvent>();
        let sink: EventSink = Arc::new(move |ev| {
            let _ = ptx.send(ev);
        });
        let pty = match factory(&spec, sink) {
            Ok(pty) => pty,
            Err(e) => {
                tracing::warn!(uid = %opts.uid, command = %spec.file, error = %e, "session spawn failed");
                return Err(e);
            }
        };
        tracing::info!(uid = %opts.uid, command = %spec.file, "session created");

        self.install(opts.uid, Shared::fresh(spec.cols, spec.rows), pty, prx);
        Ok(())
    }

    /// Install `pty` under `uid` with the read-state `shared`, starting its driver task.
    /// The one place a session enters the map — [`create_with`](Self::create_with) and
    /// [`adopt`](Self::adopt) differ only in where the pty and the state come from.
    fn install(
        &self,
        uid: String,
        shared: Arc<Shared>,
        pty: Box<dyn Pty>,
        prx: UnboundedReceiver<PtyEvent>,
    ) {
        let pipeline = SessionPipeline::new(uid.clone(), Arc::clone(&shared));
        let sessions = Arc::clone(&self.sessions);
        let events = self.events.clone();
        tokio::spawn(drive_session(pipeline, prx, events, sessions, uid.clone()));

        self.sessions
            .lock()
            .unwrap()
            .insert(uid, Session { pty: Arc::from(pty), shared });
    }

    /// Re-create a session around a pty **inherited from a predecessor process** — the
    /// receiving half of the daemon live upgrade. `build` is handed the event sink and
    /// returns the pty wrapping the adopted descriptor
    /// (`session::adopt::adopt_pty` in production, a mock in tests).
    ///
    /// The restored session is an ordinary one in every respect: same driver task, same
    /// events, same accessors. What carries across is exactly what a client can observe —
    /// the replay buffer, the output cursor and the grid. The screen mirror is rebuilt
    /// lazily by replaying the buffer through it, so a `mode:"screen"` read after an
    /// upgrade sees the pane as it was rather than an empty grid.
    ///
    /// What does NOT carry: the phase-4 liveness mirror (it re-learns itself from the next
    /// marker the shell emits) and `last_output_at` (nothing has been flushed *by us* yet;
    /// reporting a stale timestamp would misreport the pane as recently active).
    pub fn adopt(
        &self,
        snap: &SessionSnapshot,
        build: impl FnOnce(EventSink) -> io::Result<Box<dyn Pty>>,
    ) -> io::Result<()> {
        let (ptx, prx) = unbounded_channel::<PtyEvent>();
        let sink: EventSink = Arc::new(move |ev| {
            let _ = ptx.send(ev);
        });
        let pty = build(sink)?;

        // Keep the uid source ahead of anything we inherit. A successor's counter starts at 1,
        // so without this it would re-mint `s1` while an adopted `s1` was live and collide two
        // sessions on one uid.
        if let Some(n) = snap
            .uid
            .strip_prefix('s')
            .and_then(|n| n.parse::<u64>().ok())
        {
            self.next_uid.fetch_max(n + 1, Ordering::Relaxed);
        }

        let shared = Shared::fresh(snap.cols.max(1), snap.rows.max(1));
        if !snap.replay.is_empty() {
            shared.replay.lock().unwrap().append(&snap.replay);
            // Feed the same bytes to the LAZY screen path rather than parsing them now:
            // an adopted session may never get a screen read, and `sync_screen` will bring
            // the mirror up to date if one comes.
            shared
                .screen_pending
                .lock()
                .unwrap()
                .extend_from_slice(snap.replay.as_bytes());
        }
        shared.output_bytes.store(snap.cursor, Ordering::Relaxed);

        self.install(snap.uid.clone(), shared, pty, prx);
        Ok(())
    }

    /// Surrender **every** live session for a daemon live upgrade: snapshot each one's
    /// transferable state and return it beside the master descriptor a successor must adopt.
    /// The registry is empty afterwards.
    ///
    /// Each pty is released with [`Pty::relinquish`], NOT dropped — `portable-pty`'s master
    /// writer transmits `\n` + EOT from its destructor, so dropping these would type Ctrl-D
    /// into every shell the upgrade exists to preserve.
    ///
    /// A session whose pty cannot be handed over (`handoff_info() == None` — a mock, or a
    /// backend that exposes no descriptor) is **killed** instead: leaving it behind would
    /// strand a pty in a process that is about to exit.
    ///
    /// Descriptors come back in `fd_index` order, so the caller can pass them straight to
    /// `handoff::send_with_fds` in `Vec` order.
    ///
    /// **The returned descriptors are borrowed from ptys that are now deliberately leaked.**
    /// They stay open until the process image goes away, which is the whole contract: this
    /// is only ever called on the way out. The per-session reader threads are also still
    /// running, and until this process exits they keep consuming bytes the successor will
    /// never see — the same brief window nginx accepts across a live binary upgrade, and the
    /// reason the caller must exit immediately rather than linger.
    #[cfg(unix)]
    pub fn hand_off(&self) -> Vec<(SessionSnapshot, std::os::fd::RawFd)> {
        let drained: Vec<(String, Session)> = {
            let mut map = self.sessions.lock().unwrap();
            map.drain().collect()
        };

        let mut out = Vec::new();
        for (uid, session) in drained {
            let Some(info) = session.pty.handoff_info() else {
                session.shared.killed.store(true, Ordering::SeqCst);
                let _ = session.pty.kill();
                continue;
            };
            let (replay, cursor) = {
                let replay = session.shared.replay.lock().unwrap();
                let cursor = session.shared.output_bytes.load(Ordering::Relaxed);
                (replay.get().to_string(), cursor)
            };
            let (cols, rows) = session.shared.screen.lock().unwrap().dims();
            out.push((
                SessionSnapshot {
                    uid,
                    replay,
                    cursor,
                    cols,
                    rows,
                    cwd: None,
                    fd_index: out.len(),
                    pgrp: info.pgrp,
                },
                info.master_fd,
            ));
            // Last: the descriptor above is only valid while the pty lives, so the handle is
            // leaked rather than closed — the same move as [`Pty::relinquish`], done on the
            // `Arc` since the shared handle cannot be unwrapped back into a `Box`. Forgetting
            // this strong count guarantees the pty's destructor never runs (and never types
            // EOT into the shell), even if a concurrent `write` still holds a clone.
            std::mem::forget(session.pty);
        }
        tracing::info!(sessions = out.len(), "sessions handed off");
        out
    }

    /// Whether a session with `uid` is currently live.
    pub fn has(&self, uid: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(uid)
    }

    /// The uids of all live sessions.
    pub fn uids(&self) -> Vec<String> {
        self.sessions.lock().unwrap().keys().cloned().collect()
    }

    /// What is running in `uid`'s foreground *right now*, asked of the kernel
    /// ([`tools::foreground`](crate::tools::foreground)).
    ///
    /// `None` means the question could not be asked — an unknown uid, a pty that exposes
    /// no descriptor (a mock), or a platform with no foreground process group — never
    /// "nothing is running". A caller that treats the two alike would erase a pane's
    /// identity on every platform that cannot answer.
    ///
    /// The descriptor is copied out and the map lock released before the syscalls, so a
    /// probe of one session never blocks writes to another.
    pub fn foreground_name(&self, uid: &str) -> Option<String> {
        #[cfg(unix)]
        {
            // `handoff_info` is the one accessor that already exposes the master; reading
            // it does not disturb the pty (the handoff is `relinquish`, a separate step).
            let fd = {
                let map = self.sessions.lock().unwrap();
                map.get(uid)?.pty.handoff_info()?.master_fd
            };
            crate::tools::foreground::foreground_name(fd)
        }
        #[cfg(not(unix))]
        {
            let _ = uid;
            None
        }
    }

    /// The working directory of this session's foreground process group, asked of the
    /// kernel — see [`tools::foreground::foreground_cwd`](crate::tools::foreground::foreground_cwd).
    ///
    /// Sampled rather than reported, and that is the point: a pane's cwd otherwise only
    /// ever arrives as a `SessionEvent::Cwd` parsed out of OSC 7, which a plain `zsh` never
    /// emits — so a pane spawned without an explicit directory had no cwd at all. `None`
    /// keeps the same meaning as its neighbour above: no answer, never "no directory".
    pub fn foreground_cwd(&self, uid: &str) -> Option<String> {
        #[cfg(unix)]
        {
            let fd = {
                let map = self.sessions.lock().unwrap();
                map.get(uid)?.pty.handoff_info()?.master_fd
            };
            crate::tools::foreground::foreground_cwd(fd)
        }
        #[cfg(not(unix))]
        {
            let _ = uid;
            None
        }
    }

    /// Recent output for a re-attaching view (the rolling replay buffer).
    pub fn replay(&self, uid: &str) -> Option<String> {
        let map = self.sessions.lock().unwrap();
        map.get(uid)
            .map(|s| s.shared.replay.lock().unwrap().get().to_string())
    }

    /// Monotonic count of all output UTF-16 code units ever emitted (the `since`
    /// cursor; pair with `control::output::sliceSince`).
    pub fn output_bytes(&self, uid: &str) -> Option<u64> {
        let map = self.sessions.lock().unwrap();
        map.get(uid)
            .map(|s| s.shared.output_bytes.load(Ordering::Relaxed))
    }

    /// Epoch-ms of the last output flush, or `None` if the pane has produced nothing
    /// yet (feeds `control::output::waitDecision`).
    pub fn last_output_at(&self, uid: &str) -> Option<u64> {
        let map = self.sessions.lock().unwrap();
        map.get(uid)
            .and_then(|s| match s.shared.last_output_at.load(Ordering::Relaxed) {
                0 => None,
                ms => Some(ms),
            })
    }

    /// Serialize the pane's current screen to clean text (for `mode:"screen"` reads).
    /// Brings the lazily-fed screen mirror fully up to date first (see `screen_pending`).
    pub fn render_screen(&self, uid: &str) -> Option<String> {
        let map = self.sessions.lock().unwrap();
        map.get(uid).map(|s| {
            s.shared.sync_screen();
            s.shared.screen.lock().unwrap().render()
        })
    }

    /// Current pty grid `(cols, rows)` — the width a remote client must emulate at.
    pub fn dims(&self, uid: &str) -> Option<(u16, u16)> {
        let map = self.sessions.lock().unwrap();
        map.get(uid).map(|s| s.shared.screen.lock().unwrap().dims())
    }

    /// The replay buffer + the byte cursor as an ATOMIC pair (both read under the
    /// replay lock, which `flush_into` also holds while bumping the cursor). Remote
    /// clients splice a live `output` frame stream onto this snapshot; a torn pair
    /// would drop or duplicate bytes at the seam.
    pub fn replay_with_cursor(&self, uid: &str) -> Option<(String, u64)> {
        let map = self.sessions.lock().unwrap();
        map.get(uid).map(|s| {
            let replay = s.shared.replay.lock().unwrap();
            let cursor = s.shared.output_bytes.load(Ordering::Relaxed);
            (replay.get().to_string(), cursor)
        })
    }

    /// Write input to the pane's pty.
    ///
    /// An unknown uid is an error, not a no-op: it is what a caller sees when the pane it
    /// was told about is gone, and answering nothing let the control API report success for
    /// input that reached no process at all.
    pub fn write(&self, uid: &str, data: &str) -> io::Result<()> {
        // Clone the handles out and drop the registry guard before the (possibly blocking)
        // pty write, so a wedged pty never stalls every other session behind the map lock.
        let Some((pty, shared)) = self.handles(uid) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no live session {uid}"),
            ));
        };
        // Phase 4: input was just sent → optimistically clear `prompt_ready` so the
        // busy edge is reported immediately (a real prompt re-asserts it on its `133;A`).
        shared.note_write();
        pty.write(data.as_bytes()).map(|_| ())
    }

    /// The pty handle and shared state for `uid`, cloned out from under the registry lock.
    fn handles(&self, uid: &str) -> Option<(Arc<dyn Pty>, Arc<Shared>)> {
        let map = self.sessions.lock().unwrap();
        map.get(uid)
            .map(|s| (Arc::clone(&s.pty), Arc::clone(&s.shared)))
    }

    /// Snapshot a session's phase-4 liveness mirror, or `None` if the uid is unknown.
    pub fn liveness(&self, uid: &str) -> Option<Liveness> {
        let map = self.sessions.lock().unwrap();
        map.get(uid).map(|s| s.shared.liveness())
    }

    /// Resize the pane (≥1×1) — both the pty grid and the live screen model.
    pub fn resize(&self, uid: &str, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        // Same discipline as `write`: never hold the registry lock across a pty call.
        if let Some((pty, shared)) = self.handles(uid) {
            let _ = pty.resize(cols, rows);
            // Apply any buffered output to the screen BEFORE reflowing, so the resize
            // reflows the real content rather than reflowing an empty grid and then
            // advancing post-resize (which would wrap at the new width inconsistently).
            shared.sync_screen();
            shared.screen.lock().unwrap().resize(cols, rows);
        }
    }

    /// Kill the pane's pty and forget it. The natural-exit `Exit` event is suppressed
    /// (mirrors TS `destroy()`), so a deliberate kill is silent.
    pub fn kill(&self, uid: &str) {
        let removed = self.sessions.lock().unwrap().remove(uid);
        if let Some(s) = removed {
            s.shared.killed.store(true, Ordering::SeqCst);
            tracing::info!(uid, "session killed");
            let _ = s.pty.kill();
        } else {
            tracing::debug!(uid, "kill of unknown session ignored");
        }
    }

    /// Kill every live pane and clear the map.
    pub fn kill_all(&self) {
        let drained: Vec<Session> = {
            let mut map = self.sessions.lock().unwrap();
            map.drain().map(|(_, s)| s).collect()
        };
        tracing::info!(sessions = drained.len(), "killing all sessions");
        for s in drained {
            s.shared.killed.store(true, Ordering::SeqCst);
            let _ = s.pty.kill();
        }
    }
}

/// How a pane described by a spec should come up: adopt a surviving daemon session under its
/// durable uid, or spawn a fresh one. See [`SessionManager::pane_load`]. Either way the
/// variant carries the uid the pane must use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneLoad {
    /// The recorded uid names a live daemon session — adopt it (no new shell; the pane's
    /// prior output is replayed into the fresh grid).
    Reattach(String),
    /// No survivor — spawn from the spec under this freshly minted uid.
    Spawn(String),
}

impl PaneLoad {
    /// The uid the pane must be created/adopted under.
    pub fn uid(&self) -> &str {
        match self {
            PaneLoad::Reattach(u) | PaneLoad::Spawn(u) => u,
        }
    }

    /// Whether this is a re-attach (as opposed to a fresh spawn).
    pub fn is_reattach(&self) -> bool {
        matches!(self, PaneLoad::Reattach(_))
    }
}

/// Owns every live pty session, keyed by uid — the GUI's single handle to sessions. The
/// GUI holds an `Arc<SessionManager>` and calls this exact API; the **backend** behind it
/// is chosen once at construction (`docs/session-daemon-plan.md` M1):
///
/// * [`Backend::InProcess`] — the historical path: the PTYs are children of the GUI process
///   and live in a [`SessionRegistry`] right here. This is the default
///   ([`SessionManager::new`]) and what CI / `--no-daemon` use.
/// * [`Backend::Daemon`] — a [`DaemonSessionManager`] talking to the PTY-owning
///   [session daemon](crate::session::daemon) over a UDS, so the PTYs survive a GUI crash
///   (selected by [`SessionManager::new_daemon`], wired to `HYPERPANES_SESSION_DAEMON=1`).
///
/// Every public method dispatches to the active backend with an **identical signature**, so
/// the GUI's call sites are untouched — the whole point of M1: the backend swap is invisible
/// above this type. The daemon backend honors the plan's non-blocking-API contract (shadow
/// state + a mirror buffer; only `render_screen` does a bounded round-trip).
///
/// `SessionManager` stays **`Clone`** exactly as the historical in-process one was — the
/// in-process variant clones the cheap [`SessionRegistry`] handle (a shared map + sender),
/// and the daemon variant is an `Arc<DaemonSessionManager>` so a clone shares the one
/// socket + reader thread (the daemon backend is single-connection; a clone is another
/// handle, not another connection). Preserving `Clone` keeps GUI code that moves an owned
/// `mgr.clone()` onto a worker thread (`state.rs::spawn_session_async`) untouched.
#[derive(Clone)]
pub enum SessionManager {
    /// The PTYs live in this process (the default, pre-daemon path).
    InProcess(SessionRegistry),
    /// The PTYs live in the session daemon; this talks to it over a socket. `Arc` so the
    /// non-`Clone` socket/reader inside is shared across manager clones.
    Daemon(Arc<crate::session::daemon_client::DaemonSessionManager>),
}

impl SessionManager {
    /// Create an **in-process** manager that emits [`SessionEvent`]s on `events` (the
    /// default backend — PTYs are children of this process, as before the daemon existed).
    pub fn new(events: UnboundedSender<SessionEvent>) -> Self {
        SessionManager::InProcess(SessionRegistry::new(events))
    }

    /// Create a **daemon-backed** manager: connect to (spawning if needed) the session
    /// daemon for `salt` and forward its events to `events`. Errors only if the daemon
    /// can't be reached/spawned — `main` falls back to [`new`](Self::new) on `Err` so a
    /// daemon failure never blocks launch. `salt` is the user-data dir (same key the GUI's
    /// single-instance gate and the daemon's discovery use).
    pub fn new_daemon(events: UnboundedSender<SessionEvent>, salt: &str) -> io::Result<Self> {
        Ok(SessionManager::Daemon(Arc::new(
            crate::session::daemon_client::DaemonSessionManager::new(events, salt)?,
        )))
    }

    /// Daemon-backed like [`new_daemon`](Self::new_daemon), but **tolerating protocol skew**
    /// with the peer. Exactly one caller: the Windows daemon connecting to its **pty-host**,
    /// which owns the ConPTYs and is deliberately never upgraded under a running terminal. See
    /// [`VersionPolicy::Tolerant`](crate::session::daemon_client::VersionPolicy::Tolerant) for
    /// the frozen-surface contract that makes it safe.
    pub fn new_daemon_tolerant(
        events: UnboundedSender<SessionEvent>,
        salt: &str,
    ) -> io::Result<Self> {
        use crate::session::daemon_client::{DaemonSessionManager, VersionPolicy};
        Ok(SessionManager::Daemon(Arc::new(
            DaemonSessionManager::new_with_policy(events, salt, VersionPolicy::Tolerant)?,
        )))
    }

    /// The underlying [`SessionRegistry`] for the in-process backend, or `None` when this
    /// manager is daemon-backed (the registry then lives in the daemon, not here). Unused by
    /// the GUI today; kept for in-process tooling that wants the registry directly.
    pub fn registry(&self) -> Option<&SessionRegistry> {
        match self {
            SessionManager::InProcess(r) => Some(r),
            SessionManager::Daemon(_) => None,
        }
    }

    /// Whether this manager is backed by the (crash-surviving) session daemon. The GUI uses
    /// this to decide whether re-attach (M2) is even possible — only a daemon retains a
    /// session across a GUI restart, so the in-process backend always re-spawns from the
    /// recorded spawn command instead.
    pub fn is_daemon(&self) -> bool {
        matches!(self, SessionManager::Daemon(_))
    }

    /// Mint a fresh, **unique** session uid for a NEWLY created pane, choosing a scheme that
    /// fits the backend's uid-stability needs (`docs/session-daemon-plan.md` "uid stability"):
    ///
    /// * **In-process** — the historical `pane-N` token from a process-global counter. The
    ///   PTYs die with the GUI, so a uid only ever has to be unique *within* this run; the
    ///   short readable form is kept (and existing call sites/tests see no change).
    /// * **Daemon** — a `pane-<uuid>` token. Daemon sessions OUTLIVE the GUI, so a re-attaching
    ///   pane references its session by a uid recorded in a *previous* run's snapshot; were a
    ///   new run to re-use a per-run counter (`pane-0`, `pane-1`, …) its fresh panes would
    ///   collide with the daemon's still-live sessions from the prior run (silently adopting a
    ///   stranger's pty). A v4 UUID is globally unique across runs, so a new pane's uid can
    ///   never alias a survivor — and that same uid is exactly what [`to_session_file`] records
    ///   and a later launch re-attaches by.
    ///
    /// (The wire side already PINS whatever uid the GUI passes — see
    /// [`daemon_client`](crate::session::daemon_client) — so making the GUI's *minting* stable
    /// is the whole fix; the daemon honors it verbatim.)
    pub fn fresh_uid(&self) -> String {
        match self {
            SessionManager::InProcess(_) => next_inproc_uid(),
            SessionManager::Daemon(_) => format!("pane-{}", uuid::Uuid::new_v4()),
        }
    }

    /// Spawn a real pty session for `opts`. Returns once the pty is live and its driver
    /// task is running (in-process), or the create request is sent (daemon). Errors if the
    /// pty fails to spawn (in-process) / the request can't be sent (daemon).
    pub fn create(&self, opts: SpawnOptions) -> io::Result<()> {
        match self {
            SessionManager::InProcess(r) => r.create(opts),
            SessionManager::Daemon(d) => d.create(opts),
        }
    }

    /// Spawn a session using a custom pty `factory` (tests inject a mock). The daemon
    /// backend ignores the factory (a closure can't cross a socket; the daemon owns real
    /// PTYs) and spawns a normal session — no production caller uses `create_with`.
    pub fn create_with(&self, opts: SpawnOptions, factory: SpawnFn) -> io::Result<()> {
        match self {
            SessionManager::InProcess(r) => r.create_with(opts, factory),
            SessionManager::Daemon(d) => d.create_with(opts, factory),
        }
    }

    /// Whether a session with `uid` is currently live.
    pub fn has(&self, uid: &str) -> bool {
        match self {
            SessionManager::InProcess(r) => r.has(uid),
            SessionManager::Daemon(d) => d.has(uid),
        }
    }

    /// What is running in `uid`'s foreground right now — the kernel's answer, wherever the
    /// pty happens to live: read directly in-process, read from the daemon's pushed
    /// snapshot otherwise. Either way a plain in-memory read, safe on a UI tick.
    ///
    /// `None` means the question has no answer here (an unknown uid, a platform with no
    /// foreground process group, a daemon that predates the field) — never "nothing is
    /// running". Per `docs/tool-panes-plan.md` §D5 the answer may upgrade a pane's chrome
    /// and must never rewrite what the pane relaunches.
    pub fn foreground_name(&self, uid: &str) -> Option<String> {
        match self {
            SessionManager::InProcess(r) => r.foreground_name(uid),
            SessionManager::Daemon(d) => d.foreground_name(uid),
        }
    }

    /// Where the pane's foreground process group currently *is*, by the same split: read
    /// from the kernel in-process, read from the daemon's pushed snapshot otherwise.
    ///
    /// This is the answer the left panel roots itself on when it follows the focused pane,
    /// and it is a live sample — a `cd` moves it without the shell having to cooperate.
    pub fn foreground_cwd(&self, uid: &str) -> Option<String> {
        match self {
            SessionManager::InProcess(r) => r.foreground_cwd(uid),
            SessionManager::Daemon(d) => d.foreground_cwd(uid),
        }
    }

    /// The uids of all live sessions.
    pub fn uids(&self) -> Vec<String> {
        match self {
            SessionManager::InProcess(r) => r.uids(),
            SessionManager::Daemon(d) => d.uids(),
        }
    }

    /// **Reattach-or-spawn** for one pane being loaded from a spec — the single decision the
    /// GUI's restore/open paths branch on (`docs/mux-backend-plan.md` M6, and the M2 re-attach
    /// of `docs/session-daemon-plan.md`).
    ///
    /// [`PaneLoad::Reattach`] only when ALL of:
    ///   * this manager is daemon-backed ([`is_daemon`](Self::is_daemon)) — the in-process
    ///     backend's PTYs die with the GUI, so a recorded uid can never name a survivor;
    ///   * the spec recorded a durable uid (M0's `pane-<uuid>`, minted by
    ///     [`fresh_uid`](Self::fresh_uid) and carried through the snapshot / saved workspace);
    ///   * that uid is STILL LIVE in the daemon ([`has`](Self::has)).
    ///
    /// Otherwise [`PaneLoad::Spawn`]: mint a fresh uid and re-spawn from the spec. Callers
    /// must use the returned uid verbatim — re-attaching under any other uid would spawn a
    /// second session instead of adopting the survivor.
    pub fn pane_load(&self, recorded_uid: Option<&str>) -> PaneLoad {
        match recorded_uid {
            Some(uid) if self.is_daemon() && self.has(uid) => PaneLoad::Reattach(uid.to_string()),
            _ => PaneLoad::Spawn(self.fresh_uid()),
        }
    }

    /// Recent output for a re-attaching view (the rolling replay buffer).
    pub fn replay(&self, uid: &str) -> Option<String> {
        match self {
            SessionManager::InProcess(r) => r.replay(uid),
            SessionManager::Daemon(d) => d.replay(uid),
        }
    }

    /// Monotonic count of all output UTF-16 code units ever emitted (the `since`
    /// cursor; pair with `control::output::sliceSince`).
    pub fn output_bytes(&self, uid: &str) -> Option<u64> {
        match self {
            SessionManager::InProcess(r) => r.output_bytes(uid),
            SessionManager::Daemon(d) => d.output_bytes(uid),
        }
    }

    /// Epoch-ms of the last output flush, or `None` if the pane has produced nothing
    /// yet (feeds `control::output::waitDecision`).
    pub fn last_output_at(&self, uid: &str) -> Option<u64> {
        match self {
            SessionManager::InProcess(r) => r.last_output_at(uid),
            SessionManager::Daemon(d) => d.last_output_at(uid),
        }
    }

    /// Serialize the pane's current screen to clean text (for `mode:"screen"` reads).
    /// Brings the lazily-fed screen mirror fully up to date first (see `screen_pending`);
    /// the daemon backend does a bounded `RenderScreen` round-trip.
    pub fn render_screen(&self, uid: &str) -> Option<String> {
        match self {
            SessionManager::InProcess(r) => r.render_screen(uid),
            SessionManager::Daemon(d) => d.render_screen(uid),
        }
    }

    /// Write input to the pane's pty.
    ///
    /// Fallible on purpose. Both backends could already tell a failed write from a good one
    /// — the in-process one gets an `io::Result` from the pty, the daemon one gets
    /// `BrokenPipe` off a closed socket — and both threw it away, which is how
    /// `POST /panes/{id}/input` came to answer `{"ok": true}` for keystrokes that landed
    /// nowhere. A caller that genuinely does not care can still say `let _ =`; the point is
    /// that it has to say so.
    pub fn write(&self, uid: &str, data: &str) -> io::Result<()> {
        match self {
            SessionManager::InProcess(r) => r.write(uid, data),
            SessionManager::Daemon(d) => d.write(uid, data),
        }
    }

    /// Snapshot a session's phase-4 liveness mirror (OSC-133 / OSC-9;hp), or `None`.
    ///
    /// The in-process backend reads the live mirror. The daemon backend has no shadow
    /// of the marker mirror yet (the marker `SessionEvent`s do flow over the proto, but
    /// the client doesn't fold them into a shadow), so it returns `None` → the activity
    /// ticker keeps using the silence heuristic for daemon-backed panes. STUB: a later
    /// pass can mirror markers into `daemon_client::Shadow` like `last_output_at`.
    pub fn liveness(&self, uid: &str) -> Option<Liveness> {
        match self {
            SessionManager::InProcess(r) => r.liveness(uid),
            SessionManager::Daemon(_) => None,
        }
    }

    /// Resize the pane (≥1×1) — both the pty grid and the live screen model.
    pub fn resize(&self, uid: &str, cols: u16, rows: u16) {
        match self {
            SessionManager::InProcess(r) => r.resize(uid, cols, rows),
            SessionManager::Daemon(d) => d.resize(uid, cols, rows),
        }
    }

    /// Current pty grid `(cols, rows)`, or `None` when unknown. The daemon-backed answer
    /// is mirrored into the shadow from each `SessionMeta`, so it is one snapshot old
    /// rather than live — close enough for the two callers that want it (`/state`, and the
    /// re-attach seed, which needs the width the retained replay was written at).
    pub fn dims(&self, uid: &str) -> Option<(u16, u16)> {
        match self {
            SessionManager::InProcess(r) => r.dims(uid),
            SessionManager::Daemon(d) => d.dims(uid),
        }
    }

    /// The replay buffer + byte cursor as an ATOMIC pair (see the in-process impl) —
    /// what remote clients seed from before splicing the live `output` frame stream.
    pub fn replay_with_cursor(&self, uid: &str) -> Option<(String, u64)> {
        match self {
            SessionManager::InProcess(r) => r.replay_with_cursor(uid),
            SessionManager::Daemon(d) => d.replay_with_cursor(uid),
        }
    }

    /// Kill the pane's pty and forget it. The natural-exit `Exit` event is suppressed
    /// (mirrors TS `destroy()`), so a deliberate kill is silent.
    pub fn kill(&self, uid: &str) {
        match self {
            SessionManager::InProcess(r) => r.kill(uid),
            SessionManager::Daemon(d) => d.kill(uid),
        }
    }

    /// Kill every live pane and clear the map.
    pub fn kill_all(&self) {
        match self {
            SessionManager::InProcess(r) => r.kill_all(),
            SessionManager::Daemon(d) => d.kill_all(),
        }
    }

    /// Ask the session **daemon** to shut down (kill its sessions + exit), the
    /// quit-vs-keep-alive "OFF" branch and `--kill-daemon` (`docs/session-daemon-plan.md` M3).
    /// **Inert for the in-process backend** — there is no out-of-process daemon to stop; the
    /// PTYs die with the GUI on exit anyway (the GUI's `main` already calls `kill_all` on the
    /// way out). Returns whether a daemon shutdown was actually requested, so a caller can
    /// distinguish "told the daemon to stop" from "nothing to do".
    pub fn shutdown_daemon(&self) -> bool {
        match self {
            SessionManager::InProcess(_) => false,
            SessionManager::Daemon(d) => {
                d.shutdown_daemon();
                true
            }
        }
    }

    // ---- M7: cross-process claims ----
    //
    // Why the in-process backend answers "yes, and nobody else holds anything": its PTYs are
    // children of *this* process and its registry is private to it. There is no other
    // hyperpanes process that could be hosting one of these uids, so a claim can never lose
    // and no uid can ever be claimed elsewhere. The daemon backend is the only one where the
    // question is real.

    /// **Claim `uid` for this process** — the no-double-adoption gate. Returns whether the
    /// claim was granted; a caller that gets `false` must NOT adopt the session, because
    /// another hyperpanes process is already hosting it.
    ///
    /// See [`DaemonSessionManager::claim`](crate::session::daemon_client::DaemonSessionManager::claim)
    /// for the round-trip and the fail-closed policy.
    pub fn claim_session(&self, uid: &str) -> bool {
        match self {
            SessionManager::InProcess(_) => true,
            SessionManager::Daemon(d) => d.claim(uid),
        }
    }

    /// Announce a claim on `uid` without blocking — for a pane this process already hosts
    /// (it created it, or it won the race to adopt it). Called from the GUI pump, so it must
    /// never wait on the daemon. Inert in-process.
    pub fn announce_claim(&self, uid: &str) {
        match self {
            SessionManager::InProcess(_) => {}
            SessionManager::Daemon(d) => d.announce_claim(uid),
        }
    }

    /// Give up this process's claim on `uid` (a pane that was closed but whose session
    /// stays alive). Inert in-process. Not needed for crash safety — the daemon releases
    /// every claim of a connection when that connection's socket closes.
    pub fn release_session(&self, uid: &str) {
        match self {
            SessionManager::InProcess(_) => {}
            SessionManager::Daemon(d) => d.release(uid),
        }
    }

    /// The uids some **other** hyperpanes process is currently hosting — what the left
    /// panel subtracts from its detached list so it never offers to adopt a pane that is
    /// visibly running in another window. Empty for the in-process backend.
    pub fn sessions_claimed_elsewhere(&self) -> std::collections::HashSet<String> {
        match self {
            SessionManager::InProcess(_) => std::collections::HashSet::new(),
            SessionManager::Daemon(d) => d.claims_held_elsewhere(),
        }
    }
}

// Build the resolved pty spec from spawn options — the port of the TS `Session`
// constructor's resolution block (resolveSpawn → win-resolve → integration → env).
fn build_spec(opts: &SpawnOptions) -> PtySpec {
    let shell = opts.shell.clone().unwrap_or_else(default_shell);
    let args = opts.args.as_deref();
    let resolved = resolve_spawn(
        &shell,
        opts.command.as_deref(),
        args,
        opts.cwd.as_deref(),
        opts.env.as_ref(),
    );

    // node-pty/conpty launches `file` directly and won't find a bare shell NAME like
    // 'cmd' — resolve to a full path on Windows (idempotent for an already-resolved
    // file or absolute path).
    let spawn_file = if cfg!(windows) {
        resolve_windows_command(&resolved.file, opts.cwd.as_deref(), opts.env.as_ref())
    } else {
        resolved.file
    };

    // Shell integration applies ONLY on the interactive branch (no `command`).
    let mut final_args = resolved.args;
    let mut integration_env = EnvMap::new();
    if opts.command.is_none() {
        if let Some(integration) = &opts.integration {
            let mut merged = integration.args.clone();
            merged.extend(final_args);
            final_args = merged;
            integration_env = integration.env.clone();
        }
    }

    // FRESH base env per spawn (#28): registry-resolved on Windows so PATH/user-var
    // changes made after app launch reach new panes — not the process env frozen at
    // startup. See `session::env`.
    let process_env: EnvMap = crate::session::env::fresh_env();
    let resolved_control_file = resolve_control_file(opts.control_file.as_deref());
    let env = build_env(&EnvInputs {
        process_env: &process_env,
        opts_env: opts.env.as_ref(),
        integration_env: &integration_env,
        pane_id: opts.pane_id.as_deref(),
        control_file: resolved_control_file.as_deref(),
        // No shim from here: `build_spec` is reached by the headless CLI and the daemon
        // as well as the GUI, and only a host that is actually scanning pane output for
        // `openurl` sequences may promise a tool that `BROWSER` will be answered.
        browser_shim: None,
    });

    PtySpec {
        file: spawn_file,
        args: final_args,
        // A spawnable working directory MUST exist or the underlying `posix_spawn`/
        // `CreateProcessW` fails with ENOENT *before the child ever runs* — sinking the
        // whole session silently (no pty, so no Data/Exit ever reaches an attached
        // client). `opts.cwd` is honored when it is a real directory; otherwise we fall
        // back to one that exists rather than inheriting a stale/missing cwd (e.g. a
        // pane's saved cwd that was since deleted, or a `$HOME` on an unmounted drive —
        // portable-pty defaults a None cwd to `$HOME`, which need not exist). `None`
        // means "let the pty layer pick its default" only when nothing valid is found.
        cwd: resolve_spawn_cwd(opts.cwd.as_deref(), &env),
        env,
        cols: opts.cols.unwrap_or(80),
        rows: opts.rows.unwrap_or(24),
    }
}

/// Pick a working directory that is guaranteed to exist (or `None` to defer to the pty
/// layer's own default). A non-existent cwd makes the child spawn fail with ENOENT, so
/// we never hand one through: the requested `cwd` if it is a real directory, else the
/// resolved env's `$HOME` if that exists, else the daemon/process cwd, else `/` (which
/// always exists on unix). `None` only if even the process cwd is unreadable AND there
/// is no usable `$HOME` — leaving the pty layer to apply its own fallback.
fn resolve_spawn_cwd(requested: Option<&str>, env: &EnvMap) -> Option<String> {
    let is_dir = |p: &str| std::path::Path::new(p).is_dir();
    if let Some(c) = requested {
        if is_dir(c) {
            return Some(c.to_string());
        }
    }
    if let Some(home) = env.get("HOME") {
        if is_dir(home) {
            return Some(home.clone());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(s) = cwd.to_str() {
            return Some(s.to_string());
        }
    }
    if cfg!(unix) && is_dir("/") {
        return Some("/".to_string());
    }
    None
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// The async driver: pull pty events, run them through the pipeline, forward emitted
// session events to the manager channel, and on terminal exit remove the session.
async fn drive_session(
    mut pipeline: SessionPipeline,
    mut prx: UnboundedReceiver<PtyEvent>,
    events: UnboundedSender<SessionEvent>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    uid: String,
) {
    let start = Instant::now();
    let now_mono = || start.elapsed().as_millis() as u64;

    loop {
        let sleep_for = pipeline
            .batcher
            .deadline()
            .map(|d| Duration::from_millis(d.saturating_sub(now_mono())));

        tokio::select! {
            maybe = prx.recv() => {
                match maybe {
                    Some(PtyEvent::Data(bytes)) => {
                        let decoded = pipeline.decode(&bytes);
                        for ev in pipeline.on_data(&decoded, now_mono(), epoch_ms()) {
                            let _ = events.send(ev);
                        }
                    }
                    Some(PtyEvent::Exit(code)) => {
                        tracing::info!(uid = %uid, code, "session exited");
                        for ev in pipeline.on_exit(code, epoch_ms()) {
                            let _ = events.send(ev);
                        }
                        break;
                    }
                    None => {
                        tracing::debug!(uid = %uid, "session pty sink dropped");
                        break;
                    }
                }
            }
            _ = async { tokio::time::sleep(sleep_for.unwrap()).await }, if sleep_for.is_some() => {
                for ev in pipeline.on_timer(epoch_ms()) {
                    let _ = events.send(ev);
                }
            }
        }
    }

    sessions.lock().unwrap().remove(&uid);
}

// The session output pipeline: cwd-sniff (raw) → batcher → flush → replay + screen +
// counters + Data/Cwd/Exit. Driver-task-local except for the shared read state.
// Unit-testable in isolation by driving its methods with controlled clocks.
struct SessionPipeline {
    uid: String,
    batcher: DataBatcher,
    /// Carry for an OSC cwd sequence split across pty chunks.
    osc_carry: String,
    /// Carry for a phase-4 semantic marker (OSC 133 / OSC 9;hp) split across chunks —
    /// independent of `osc_carry` so the two scanners never clobber each other's tail.
    marker_carry: String,
    /// De-dupe: emit `Cwd` only when the directory actually changes.
    last_cwd: Option<String>,
    /// Carry for an incomplete trailing UTF-8 sequence split across pty reads.
    utf8_carry: Vec<u8>,
    ended: bool,
    shared: Arc<Shared>,
}

impl SessionPipeline {
    fn new(uid: String, shared: Arc<Shared>) -> Self {
        Self {
            uid,
            batcher: DataBatcher::new(),
            osc_carry: String::new(),
            marker_carry: String::new(),
            last_cwd: None,
            utf8_carry: Vec::new(),
            ended: false,
            shared,
        }
    }

    /// Streaming UTF-8 decode of a raw pty chunk, buffering an incomplete trailing
    /// sequence so a multibyte glyph split across reads isn't mangled (node-pty's
    /// `StringDecoder` does the same). Genuinely invalid bytes become U+FFFD.
    fn decode(&mut self, chunk: &[u8]) -> String {
        decode_utf8_streaming(&mut self.utf8_carry, chunk)
    }

    /// Handle a decoded raw chunk: sniff cwd (pre-batch), then feed the batcher. A
    /// size-triggered flush is processed inline.
    fn on_data(&mut self, raw: &str, now_mono_ms: u64, now_epoch_ms: u64) -> Vec<SessionEvent> {
        if self.ended {
            return Vec::new();
        }
        let mut out = Vec::new();

        // Tap the RAW chunk for a cwd OSC before batching (xterm consumes these OSCs
        // silently; we only sniff the cwd out).
        let (cwd, carry) = parse_osc_cwd(&self.osc_carry, raw);
        self.osc_carry = carry;
        if let Some(cwd) = cwd {
            if Some(&cwd) != self.last_cwd.as_ref() {
                self.last_cwd = Some(cwd.clone());
                out.push(SessionEvent::Cwd {
                    uid: self.uid.clone(),
                    cwd,
                });
            }
        }

        // Phase 4: tap the same RAW chunk for semantic prompt markers (OSC 133 /
        // OSC 9;hp), updating the liveness mirror and emitting per-marker events. The
        // marker bytes are NOT stripped from the batched stream here — like the cwd OSC
        // they are inert escape sequences the terminal grid ignores, so they never render
        // visibly; this keeps the byte cursor / replay buffer faithful to what was sent.
        let (markers, mcarry) = crate::session::osc133::parse_osc_markers(&self.marker_carry, raw);
        self.marker_carry = mcarry;
        for m in &markers {
            self.shared.apply_marker(m);
            out.push(marker_to_event(&self.uid, m));
        }

        if let Some(flushed) = self.batcher.write(raw, now_mono_ms) {
            self.flush_into(flushed, now_epoch_ms, &mut out);
        }
        out
    }

    /// A time-triggered flush from the driver's 16 ms timer.
    fn on_timer(&mut self, now_epoch_ms: u64) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        if let Some(flushed) = self.batcher.flush() {
            self.flush_into(flushed, now_epoch_ms, &mut out);
        }
        out
    }

    /// Terminal pty exit. On a *natural* exit, flush remaining output then emit `Exit`.
    /// On a manual kill (`shared.killed`), stay silent — mirrors TS `destroy()` gating.
    fn on_exit(&mut self, code: i32, now_epoch_ms: u64) -> Vec<SessionEvent> {
        if self.ended {
            return Vec::new();
        }
        self.ended = true;
        if self.shared.killed.load(Ordering::SeqCst) {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(flushed) = self.batcher.flush() {
            self.flush_into(flushed, now_epoch_ms, &mut out);
        }
        out.push(SessionEvent::Exit {
            uid: self.uid.clone(),
            code,
        });
        out
    }

    // Apply a flushed batch: grow replay, BUFFER for the lazy screen mirror, bump the
    // cursor/stamp, emit Data. The screen is NOT parsed here — its bytes are stashed in
    // `screen_pending` and parsed on demand by `Shared::sync_screen` at read time. This
    // removes the per-flush second VTE parse (the GUI grid already parses the same bytes
    // via `SessionEvent::Data`), which was pure wasted CPU when no control client reads
    // the screen. See `Shared::screen_pending`.
    fn flush_into(&mut self, data: String, now_epoch_ms: u64, out: &mut Vec<SessionEvent>) {
        let n = data.encode_utf16().count() as u64;
        // Bump the cursor while HOLDING the replay lock: `replay_with_cursor` reads the
        // pair under the same lock, so a snapshot can never see one without the other
        // (a torn pair would drop or duplicate bytes on a remote attach splice).
        let cursor = {
            let mut replay = self.shared.replay.lock().unwrap();
            replay.append(&data);
            self.shared.output_bytes.fetch_add(n, Ordering::Relaxed) + n
        };
        self.shared
            .screen_pending
            .lock()
            .unwrap()
            .extend_from_slice(data.as_bytes());
        self.shared
            .last_output_at
            .store(now_epoch_ms, Ordering::Relaxed);
        out.push(SessionEvent::Data {
            uid: self.uid.clone(),
            data,
            cursor,
        });
    }
}

/// Map a parsed phase-4 [`Marker`](crate::session::osc133::Marker) to its `SessionEvent`.
fn marker_to_event(uid: &str, m: &crate::session::osc133::Marker) -> SessionEvent {
    use crate::session::osc133::Marker;
    match m {
        Marker::CommandStart => SessionEvent::CommandStart {
            uid: uid.to_string(),
        },
        Marker::CommandEnd { code } => SessionEvent::CommandEnd {
            uid: uid.to_string(),
            code: *code,
        },
        Marker::PromptReady => SessionEvent::PromptReady {
            uid: uid.to_string(),
        },
        Marker::Agent { state, code } => SessionEvent::AgentState {
            uid: uid.to_string(),
            state: (*state).into(),
            code: *code,
        },
    }
}

/// Streaming UTF-8 decoder: append `chunk` to `carry`, emit all decodable text, and
/// keep only an incomplete trailing sequence in `carry` for the next call. Invalid
/// bytes are replaced with U+FFFD (matching `from_utf8_lossy`). Free function so it can
/// be unit-tested directly.
pub fn decode_utf8_streaming(carry: &mut Vec<u8>, chunk: &[u8]) -> String {
    carry.extend_from_slice(chunk);
    let mut decoded = String::new();
    loop {
        match std::str::from_utf8(carry) {
            Ok(s) => {
                decoded.push_str(s);
                carry.clear();
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // SAFETY: bytes [..valid] are valid UTF-8 by `valid_up_to`'s contract.
                decoded.push_str(unsafe { std::str::from_utf8_unchecked(&carry[..valid]) });
                match e.error_len() {
                    Some(len) => {
                        // A genuinely invalid sequence mid-buffer: replace and continue.
                        decoded.push('\u{FFFD}');
                        carry.drain(..valid + len);
                    }
                    None => {
                        // An incomplete sequence at the tail: keep it for next time.
                        carry.drain(..valid);
                        break;
                    }
                }
            }
        }
    }
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::batcher::BATCH_MAX_SIZE;

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            replay: Mutex::new(Replay::new()),
            screen: Mutex::new(Screen::new(80, 24)),
            screen_pending: Mutex::new(Vec::new()),
            output_bytes: AtomicU64::new(0),
            last_output_at: AtomicU64::new(0),
            killed: AtomicBool::new(false),
            prompt_ready: AtomicBool::new(false),
            command_running: AtomicBool::new(false),
            last_exit_code: AtomicI32::new(i32::MIN),
            marker_seen: AtomicBool::new(false),
        })
    }

    // ---- streaming UTF-8 decoder ----

    #[test]
    fn decoder_passes_through_ascii() {
        let mut carry = Vec::new();
        assert_eq!(decode_utf8_streaming(&mut carry, b"hello"), "hello");
        assert!(carry.is_empty());
    }

    #[test]
    fn decoder_buffers_a_split_multibyte_char() {
        let mut carry = Vec::new();
        let emoji = "😀".as_bytes(); // 4 bytes: F0 9F 98 80
                                     // First read ends mid-emoji.
        let a = decode_utf8_streaming(&mut carry, &emoji[..2]);
        assert_eq!(a, "");
        assert_eq!(carry.len(), 2);
        // Second read completes it.
        let b = decode_utf8_streaming(&mut carry, &emoji[2..]);
        assert_eq!(b, "😀");
        assert!(carry.is_empty());
    }

    #[test]
    fn decoder_replaces_truly_invalid_bytes() {
        let mut carry = Vec::new();
        // 0xFF is never valid UTF-8.
        let out = decode_utf8_streaming(&mut carry, &[b'a', 0xFF, b'b']);
        assert_eq!(out, "a\u{FFFD}b");
        assert!(carry.is_empty());
    }

    // ---- pipeline: cwd sniffing + de-dupe ----

    #[test]
    fn pipeline_emits_cwd_on_change_only() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        let seq = "\u{1b}]7;file:///C:/proj\u{07}";
        let evs = p.on_data(seq, 0, 1000);
        assert_eq!(
            evs[0],
            SessionEvent::Cwd {
                uid: "u1".into(),
                cwd: "C:\\proj".into()
            }
        );
        // Same cwd again → no Cwd event (the prompt re-emits its OSC each keystroke).
        let evs2 = p.on_data(seq, 1, 1001);
        assert!(!evs2.iter().any(|e| matches!(e, SessionEvent::Cwd { .. })));
    }

    // ---- pipeline: phase-4 marker sniff + liveness mirror ----

    #[test]
    fn pipeline_sniffs_osc133_markers_and_updates_the_liveness_mirror() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        // Before any marker the mirror reports marker_seen=false (silence heuristic owns it).
        assert!(!sh.liveness().marker_seen);

        // A command starts running: 133;C → CommandStart event + command_running mirror.
        let evs = p.on_data("\u{1b}]133;C\u{07}", 0, 1000);
        assert!(evs
            .iter()
            .any(|e| matches!(e, SessionEvent::CommandStart { .. })));
        let l = sh.liveness();
        assert!(l.marker_seen && l.command_running && !l.prompt_ready);

        // The command finishes with code 0, then the prompt returns: 133;D;0 then 133;A.
        let evs2 = p.on_data("\u{1b}]133;D;0\u{07}\u{1b}]133;A\u{07}", 1, 1001);
        assert!(evs2
            .iter()
            .any(|e| matches!(e, SessionEvent::CommandEnd { code: Some(0), .. })));
        assert!(evs2
            .iter()
            .any(|e| matches!(e, SessionEvent::PromptReady { .. })));
        let l2 = sh.liveness();
        assert!(!l2.command_running && l2.prompt_ready);
        assert_eq!(l2.last_exit_code, Some(0));
    }

    #[test]
    fn pipeline_sniffs_agent_liveness_marker() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        let evs = p.on_data("\u{1b}]9;hp;state=busy\u{07}", 0, 1000);
        assert!(evs.iter().any(|e| matches!(
            e,
            SessionEvent::AgentState {
                state: AgentLiveness::Busy,
                ..
            }
        )));
        assert!(sh.liveness().command_running);
        // awaiting-input flips the mirror to prompt_ready.
        p.on_data("\u{1b}]9;hp;state=awaiting-input\u{07}", 1, 1001);
        let l = sh.liveness();
        assert!(l.prompt_ready && !l.command_running);
    }

    // ---- pipeline: flush → replay + counters + Data ----

    #[test]
    fn pipeline_time_flush_emits_data_and_updates_state() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        // Small writes don't flush synchronously.
        assert!(p.on_data("abc", 0, 500).is_empty());
        assert!(p.on_data("de", 5, 505).is_empty());
        // Timer fires.
        let evs = p.on_timer(520);
        assert_eq!(
            evs,
            vec![SessionEvent::Data {
                uid: "u1".into(),
                data: "abcde".into(),
                cursor: 5,
            }]
        );
        assert_eq!(sh.replay.lock().unwrap().get(), "abcde");
        assert_eq!(sh.output_bytes.load(Ordering::Relaxed), 5);
        assert_eq!(sh.last_output_at.load(Ordering::Relaxed), 520);
    }

    #[test]
    fn pipeline_output_bytes_counts_utf16_units() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        p.on_data("😀a", 0, 100); // emoji=2 u16, a=1 → 3
        p.on_timer(110);
        assert_eq!(sh.output_bytes.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn pipeline_size_overflow_flushes_inline() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        let big = "x".repeat(BATCH_MAX_SIZE - 1);
        assert!(p.on_data(&big, 0, 100).is_empty());
        // This pushes past the threshold → the buffered `big` flushes out as Data.
        let evs = p.on_data("yy", 1, 101);
        assert_eq!(
            evs,
            vec![SessionEvent::Data {
                uid: "u1".into(),
                data: big.clone(),
                cursor: (BATCH_MAX_SIZE - 1) as u64,
            }]
        );
        // The new chunk remains buffered until its own flush.
        let evs2 = p.on_timer(120);
        assert_eq!(
            evs2,
            vec![SessionEvent::Data {
                uid: "u1".into(),
                data: "yy".into(),
                cursor: (BATCH_MAX_SIZE + 1) as u64,
            }]
        );
    }

    // ---- pipeline: exit gating ----

    #[test]
    fn pipeline_natural_exit_flushes_then_emits_exit() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        p.on_data("tail", 0, 200);
        let evs = p.on_exit(0, 210);
        assert_eq!(
            evs,
            vec![
                SessionEvent::Data {
                    uid: "u1".into(),
                    data: "tail".into(),
                    cursor: 4,
                },
                SessionEvent::Exit {
                    uid: "u1".into(),
                    code: 0
                },
            ]
        );
    }

    #[test]
    fn pipeline_manual_kill_suppresses_exit() {
        let sh = shared();
        sh.killed.store(true, Ordering::SeqCst);
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        p.on_data("tail", 0, 200);
        let evs = p.on_exit(0, 210);
        assert!(evs.is_empty(), "manual kill must be silent");
    }

    #[test]
    fn pipeline_ignores_data_after_exit() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        p.on_exit(0, 100);
        assert!(p.on_data("late", 1, 101).is_empty());
    }

    // ---- end-to-end manager wiring via a mock pty (no ConPTY) ----

    #[derive(Default)]
    struct MockPty {
        last_resize: Mutex<Option<(u16, u16)>>,
        killed: AtomicBool,
    }
    impl Pty for MockPty {
        fn write(&self, _data: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
            *self.last_resize.lock().unwrap() = Some((cols, rows));
            Ok(())
        }
        fn kill(&self) -> io::Result<()> {
            self.killed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// A mock that CAN be handed over: it reports a real descriptor (a `/dev/null` handle,
    /// which is all `handoff_info` promises — an open fd the successor could receive) and
    /// keeps the default `relinquish`, so a test can prove the pty was released rather than
    /// killed or dropped.
    #[cfg(unix)]
    struct HandoffMockPty {
        fd: std::fs::File,
        killed: Arc<AtomicBool>,
    }
    #[cfg(unix)]
    impl Pty for HandoffMockPty {
        fn write(&self, _data: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn resize(&self, _cols: u16, _rows: u16) -> io::Result<()> {
            Ok(())
        }
        fn kill(&self) -> io::Result<()> {
            self.killed.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn handoff_info(&self) -> Option<crate::session::pty::HandoffInfo> {
            use std::os::fd::AsRawFd;
            Some(crate::session::pty::HandoffInfo {
                master_fd: self.fd.as_raw_fd(),
                pgrp: Some(4242),
            })
        }
    }

    /// Is `fd` still an open descriptor in this process?
    #[cfg(unix)]
    fn fd_is_open(fd: std::os::fd::RawFd) -> bool {
        unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
    }

    /// A session in `reg` whose pty is handoff-capable; returns its sink and kill flag.
    #[cfg(unix)]
    fn make_handoff_session(reg: &SessionRegistry, uid: &str) -> (EventSink, Arc<AtomicBool>) {
        let slot: Arc<Mutex<Option<EventSink>>> = Arc::new(Mutex::new(None));
        let killed = Arc::new(AtomicBool::new(false));
        let slot2 = Arc::clone(&slot);
        let killed2 = Arc::clone(&killed);
        let factory: SpawnFn = Box::new(move |_spec, sink| {
            *slot2.lock().unwrap() = Some(sink);
            Ok(Box::new(HandoffMockPty {
                fd: std::fs::File::open("/dev/null")?,
                killed: killed2,
            }) as Box<dyn Pty>)
        });
        reg.create_with(
            SpawnOptions {
                uid: uid.into(),
                cols: Some(100),
                rows: Some(40),
                ..Default::default()
            },
            factory,
        )
        .expect("create");
        let sink = slot.lock().unwrap().clone().expect("sink captured");
        (sink, killed)
    }

    /// Wait until `uid`'s output cursor reaches `want` (the 16 ms batch timer owns the flush).
    async fn cursor_reaches(reg: &SessionRegistry, uid: &str, want: u64) -> u64 {
        for _ in 0..100 {
            match reg.output_bytes(uid) {
                Some(n) if n >= want => return n,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        reg.output_bytes(uid).unwrap_or(0)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hand_off_snapshots_every_session_and_empties_the_registry() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let reg = SessionRegistry::new(etx);
        let (sink_a, killed_a) = make_handoff_session(&reg, "a");
        let (_sink_b, _killed_b) = make_handoff_session(&reg, "b");

        sink_a(PtyEvent::Data(b"hello".to_vec()));
        cursor_reaches(&reg, "a", 5).await;

        let mut handed = reg.hand_off();
        handed.sort_by(|x, y| x.0.uid.cmp(&y.0.uid));
        assert_eq!(handed.len(), 2, "both sessions are handed over");

        let (snap_a, fd_a) = &handed[0];
        assert_eq!(snap_a.uid, "a");
        assert_eq!(snap_a.replay, "hello", "the scrollback carries across");
        assert_eq!(snap_a.cursor, 5, "so does the output cursor");
        assert_eq!((snap_a.cols, snap_a.rows), (100, 40), "and the grid");
        assert_eq!(snap_a.pgrp, Some(4242));
        assert!(
            fd_is_open(*fd_a),
            "the descriptor must still be open — the pty was relinquished, not dropped"
        );
        assert!(
            !killed_a.load(Ordering::SeqCst),
            "a handed-over session must NOT be killed: that is the whole point"
        );

        // fd_index addresses the descriptor array in returned order.
        let idx: Vec<usize> = handed.iter().map(|(s, _)| s.fd_index).collect();
        assert_eq!(idx.len(), 2);
        assert!(idx.contains(&0) && idx.contains(&1));

        assert!(reg.uids().is_empty(), "the registry hands over everything");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hand_off_kills_a_session_that_cannot_be_handed_over() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let reg = SessionRegistry::new(etx);
        let killed = Arc::new(AtomicBool::new(false));
        let killed2 = Arc::clone(&killed);
        // The plain mock answers `handoff_info() == None` (the trait default).
        let factory: SpawnFn = Box::new(move |_spec, _sink| {
            struct Unhandoffable(Arc<AtomicBool>);
            impl Pty for Unhandoffable {
                fn write(&self, _d: &[u8]) -> io::Result<()> {
                    Ok(())
                }
                fn resize(&self, _c: u16, _r: u16) -> io::Result<()> {
                    Ok(())
                }
                fn kill(&self) -> io::Result<()> {
                    self.0.store(true, Ordering::SeqCst);
                    Ok(())
                }
            }
            Ok(Box::new(Unhandoffable(killed2)) as Box<dyn Pty>)
        });
        reg.create_with(
            SpawnOptions {
                uid: "stuck".into(),
                ..Default::default()
            },
            factory,
        )
        .expect("create");

        assert!(reg.hand_off().is_empty(), "nothing to hand over");
        assert!(
            killed.load(Ordering::SeqCst),
            "a session that cannot cross must be killed, not stranded in a dying process"
        );
        assert!(reg.uids().is_empty());
    }

    /// A pty whose `write` blocks (a wedged child that stopped draining its input) must not
    /// wedge the registry: every other accessor takes the map lock, so a write that held it
    /// across the pty call would stall reads, resizes and exits for every session at once.
    #[tokio::test]
    async fn a_blocking_pty_write_does_not_hold_the_registry_lock() {
        use std::sync::mpsc::{channel, Receiver, Sender};
        struct StuckPty {
            entered: Sender<()>,
            release: Mutex<Option<Receiver<()>>>,
        }
        impl Pty for StuckPty {
            fn write(&self, _data: &[u8]) -> io::Result<()> {
                let release = self.release.lock().unwrap().take().expect("a single write");
                let _ = self.entered.send(());
                let _ = release.recv();
                Ok(())
            }
            fn resize(&self, _cols: u16, _rows: u16) -> io::Result<()> {
                Ok(())
            }
            fn kill(&self) -> io::Result<()> {
                Ok(())
            }
        }

        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let reg = SessionRegistry::new(etx);
        let (entered_tx, entered_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let factory: SpawnFn = Box::new(move |_spec, _sink| {
            Ok(Box::new(StuckPty {
                entered: entered_tx,
                release: Mutex::new(Some(release_rx)),
            }) as Box<dyn Pty>)
        });
        reg.create_with(
            SpawnOptions {
                uid: "stuck".into(),
                ..Default::default()
            },
            factory,
        )
        .expect("create");

        let writer = {
            let reg = reg.clone();
            std::thread::spawn(move || reg.write("stuck", "x"))
        };
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the write reached the pty");

        // The write is parked inside the pty. The registry must still answer.
        let (probe_tx, probe_rx) = channel::<Vec<String>>();
        {
            let reg = reg.clone();
            std::thread::spawn(move || {
                let _ = probe_tx.send(reg.uids());
            });
        }
        let uids = probe_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the registry lock is free while a pty write blocks");
        assert_eq!(uids, vec!["stuck".to_string()]);

        release_tx.send(()).expect("release the pty");
        writer.join().expect("writer thread").expect("write ok");
    }

    /// `foreground_name` answers `None` for both "no such session" and "this pty exposes no
    /// descriptor" — the honest *no answer*. A caller that read it as "nothing is running"
    /// would downgrade a pane's identity on every platform and every mock that cannot ask.
    #[tokio::test]
    async fn a_pty_that_exposes_no_descriptor_gives_no_foreground_answer() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let reg = SessionRegistry::new(etx);
        let factory: SpawnFn =
            Box::new(|_spec, _sink| Ok(Box::new(MockPty::default()) as Box<dyn Pty>));
        reg.create_with(
            SpawnOptions {
                uid: "s1".into(),
                ..Default::default()
            },
            factory,
        )
        .expect("create");

        assert_eq!(reg.foreground_name("s1"), None, "the mock has no master fd");
        assert_eq!(
            reg.foreground_name("nobody"),
            None,
            "and an unknown uid is not a panic"
        );
    }

    #[tokio::test]
    async fn adopt_restores_the_snapshot_and_streams_on_from_the_carried_cursor() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let reg = SessionRegistry::new(etx);
        let snap = SessionSnapshot {
            uid: "s1".into(),
            replay: "before-the-upgrade".into(),
            cursor: 1000,
            cols: 100,
            rows: 40,
            ..Default::default()
        };

        let slot: Arc<Mutex<Option<EventSink>>> = Arc::new(Mutex::new(None));
        let slot2 = Arc::clone(&slot);
        reg.adopt(&snap, move |sink| {
            *slot2.lock().unwrap() = Some(sink);
            Ok(Box::new(MockPty::default()) as Box<dyn Pty>)
        })
        .expect("adopt");
        let sink = slot.lock().unwrap().clone().expect("sink captured");

        assert!(reg.has("s1"));
        assert_eq!(reg.replay("s1").as_deref(), Some("before-the-upgrade"));
        assert_eq!(reg.output_bytes("s1"), Some(1000));
        assert_eq!(reg.dims("s1"), Some((100, 40)));
        assert!(
            reg.render_screen("s1")
                .expect("screen")
                .contains("before-the-upgrade"),
            "the screen mirror is rebuilt by replaying the carried buffer"
        );

        // Live output continues from the carried cursor rather than restarting at 0 — a
        // client holding a pre-upgrade `since` value must not be handed a rewound stream.
        sink(PtyEvent::Data(b"after".to_vec()));
        assert_eq!(cursor_reaches(&reg, "s1", 1005).await, 1005);
        assert_eq!(reg.replay("s1").as_deref(), Some("before-the-upgradeafter"));
    }

    // A successor's uid counter starts at 1, so without a floor it would re-mint `s3` while
    // the `s3` it just adopted was still live — two sessions, one uid, and a pane bound to
    // whichever won the map. Adopting must push the counter past anything it inherits.
    #[tokio::test]
    async fn adopting_pushes_the_uid_counter_past_the_inherited_uids() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let reg = SessionRegistry::new(etx);
        assert_eq!(
            reg.mint_uid(),
            "s1",
            "a fresh registry starts at the bottom"
        );

        for uid in ["s3", "s2"] {
            let snap = SessionSnapshot {
                uid: uid.into(),
                cols: 80,
                rows: 24,
                ..Default::default()
            };
            reg.adopt(&snap, |_sink| {
                Ok(Box::new(MockPty::default()) as Box<dyn Pty>)
            })
            .expect("adopt");
        }

        assert_eq!(
            reg.mint_uid(),
            "s4",
            "the counter clears the HIGHEST adopted uid, not merely the last one"
        );

        // Uids the daemon did not mint (the GUI pins its own) leave the counter alone.
        let snap = SessionSnapshot {
            uid: "pane-abc".into(),
            cols: 80,
            rows: 24,
            ..Default::default()
        };
        reg.adopt(&snap, |_sink| {
            Ok(Box::new(MockPty::default()) as Box<dyn Pty>)
        })
        .expect("adopt");
        assert_eq!(reg.mint_uid(), "s5");
    }

    // Create a session whose pty is a mock; returns the manager event receiver and the
    // captured event sink the test uses to drive PtyEvents into the driver task.
    fn make_session(
        uid: &str,
    ) -> (
        SessionManager,
        tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
        EventSink,
    ) {
        let (etx, erx) = unbounded_channel::<SessionEvent>();
        let mgr = SessionManager::new(etx);
        let slot: Arc<Mutex<Option<EventSink>>> = Arc::new(Mutex::new(None));
        let slot2 = Arc::clone(&slot);
        let factory: SpawnFn = Box::new(move |_spec, sink| {
            *slot2.lock().unwrap() = Some(sink);
            Ok(Box::new(MockPty::default()) as Box<dyn Pty>)
        });
        mgr.create_with(
            SpawnOptions {
                uid: uid.into(),
                ..Default::default()
            },
            factory,
        )
        .expect("create");
        let sink = slot.lock().unwrap().clone().expect("sink captured");
        (mgr, erx, sink)
    }

    async fn recv(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
    ) -> Option<SessionEvent> {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn manager_streams_data_then_removes_on_natural_exit() {
        let (mgr, mut rx, sink) = make_session("u1");
        assert!(mgr.has("u1"));

        sink(PtyEvent::Data(b"hello".to_vec()));
        // Flushed by the 16 ms batch timer.
        assert_eq!(
            recv(&mut rx).await,
            Some(SessionEvent::Data {
                uid: "u1".into(),
                data: "hello".into(),
                cursor: 5,
            })
        );
        assert_eq!(mgr.output_bytes("u1"), Some(5));
        assert_eq!(mgr.replay("u1").as_deref(), Some("hello"));
        assert!(mgr.last_output_at("u1").is_some());

        sink(PtyEvent::Exit(0));
        assert_eq!(
            recv(&mut rx).await,
            Some(SessionEvent::Exit {
                uid: "u1".into(),
                code: 0
            })
        );

        // The driver removes the session from the map after the terminal exit.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!mgr.has("u1"));
    }

    #[tokio::test]
    async fn manager_kill_is_silent_and_forgets_the_session() {
        let (mgr, mut rx, sink) = make_session("u1");
        mgr.kill("u1");
        assert!(!mgr.has("u1"));

        // A pty Exit arriving after a manual kill must NOT surface as a SessionEvent.
        sink(PtyEvent::Exit(0));
        let got = recv(&mut rx).await;
        assert!(
            !matches!(got, Some(SessionEvent::Exit { .. })),
            "manual kill must suppress the Exit event, got {got:?}"
        );
    }

    #[tokio::test]
    async fn manager_emits_cwd_event_from_an_osc_sniff() {
        let (mgr, mut rx, sink) = make_session("u1");
        sink(PtyEvent::Data(b"\x1b]7;file:///C:/work\x07".to_vec()));
        // Cwd fires immediately (pre-batch), before any Data flush.
        assert_eq!(
            recv(&mut rx).await,
            Some(SessionEvent::Cwd {
                uid: "u1".into(),
                cwd: "C:\\work".into()
            })
        );
        let _ = mgr;
    }

    #[test]
    fn lazy_screen_reflects_buffered_output_on_sync() {
        // flush_into buffers for the screen instead of parsing eagerly; sync_screen must
        // bring the mirror fully current so a read sees everything flushed so far.
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        p.on_data("hello world", 0, 100);
        p.on_timer(120); // flush → buffered, screen NOT yet advanced
        assert_eq!(
            sh.screen.lock().unwrap().render(),
            "",
            "screen is lazy: empty before sync"
        );
        sh.sync_screen();
        assert_eq!(sh.screen.lock().unwrap().render(), "hello world");
        // A second sync with nothing pending is a no-op and leaves the screen intact.
        sh.sync_screen();
        assert_eq!(sh.screen.lock().unwrap().render(), "hello world");
    }

    #[tokio::test]
    async fn manager_render_screen_syncs_pending_output() {
        let (mgr, mut rx, sink) = make_session("u1");
        sink(PtyEvent::Data(b"abc\r\ndef".to_vec()));
        // Wait for the Data flush so the bytes are buffered for the screen.
        assert!(matches!(
            recv(&mut rx).await,
            Some(SessionEvent::Data { .. })
        ));
        // render_screen must lazily sync the buffered output before serializing.
        assert_eq!(mgr.render_screen("u1").as_deref(), Some("abc\ndef"));
    }

    #[tokio::test]
    async fn manager_resize_updates_screen_and_pty() {
        let (mgr, _rx, _sink) = make_session("u1");
        mgr.resize("u1", 120, 40);
        // Screen render works post-resize (smoke that the screen lock path is sound).
        assert!(mgr.render_screen("u1").is_some());
    }

    // ---- uid minting policy (session-daemon-plan "uid stability") ----

    #[test]
    fn in_process_backend_is_not_daemon() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let mgr = SessionManager::new(etx);
        assert!(
            !mgr.is_daemon(),
            "the in-process backend reports is_daemon() == false"
        );
    }

    // shutdown_daemon is INERT for the in-process backend (session-daemon M3): there is no
    // out-of-process daemon to stop, so it returns false and does nothing — the quit path
    // distinguishes this from a daemon shutdown by the bool.
    #[test]
    fn in_process_shutdown_daemon_is_inert() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let mgr = SessionManager::new(etx);
        assert!(
            !mgr.shutdown_daemon(),
            "in-process shutdown_daemon is a no-op returning false"
        );
    }

    #[test]
    fn in_process_fresh_uid_is_pane_n_and_unique() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let mgr = SessionManager::new(etx);
        // The in-process scheme is the readable `pane-N` (PTYs die with the GUI, so per-run
        // uniqueness suffices); the counter is process-global so two managers never alias.
        let a = mgr.fresh_uid();
        let b = mgr.fresh_uid();
        assert!(
            a.starts_with("pane-"),
            "in-process fresh_uid is pane-N, got {a}"
        );
        assert_ne!(a, b, "successive fresh_uids are unique");
        let (etx2, _erx2) = unbounded_channel::<SessionEvent>();
        let mgr2 = SessionManager::new(etx2);
        // A SECOND manager shares the process-global counter — no cross-manager `pane-0`
        // collision (the historical multi-window clobber this counter was hardened against).
        assert_ne!(
            mgr2.fresh_uid(),
            a,
            "the counter is process-global across managers"
        );
    }

    // ---- reattach-or-spawn (M6 / M2) ----

    /// On the IN-PROCESS backend re-attach is impossible (the PTYs die with the GUI), so
    /// `pane_load` always says Spawn — with a FRESH uid, never the recorded one. Adopting a
    /// recorded uid here would silently alias whatever `pane-N` this run happens to reissue.
    #[test]
    fn in_process_pane_load_always_spawns_with_a_fresh_uid() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let mgr = SessionManager::new(etx);
        for recorded in [None, Some("pane-from-last-run"), Some("pane-0")] {
            let load = mgr.pane_load(recorded);
            assert!(
                !load.is_reattach(),
                "in-process never re-attaches (recorded={recorded:?}), got {load:?}"
            );
            assert_ne!(
                Some(load.uid()),
                recorded,
                "a spawn mints a fresh uid, not the recorded one"
            );
            assert!(load.uid().starts_with("pane-"));
        }
    }

    /// A spec with no recorded uid (a saved workspace used as a plain launch template) can
    /// only ever spawn, and each pane gets its own uid.
    #[test]
    fn pane_load_without_a_recorded_uid_spawns_uniquely() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let mgr = SessionManager::new(etx);
        let a = mgr.pane_load(None);
        let b = mgr.pane_load(None);
        assert_eq!(a, PaneLoad::Spawn(a.uid().to_string()));
        assert_ne!(a.uid(), b.uid());
    }
}
