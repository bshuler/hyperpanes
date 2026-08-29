//! **tmux control mode (`-CC`)** — the server half (`docs/mux-backend-plan.md` M4).
//!
//! iTerm2 and several mobile terminals speak tmux's *control mode*: instead of rendering a
//! full-screen terminal, the client drives the multiplexer over a line-oriented text
//! protocol and draws each pane as a **native tab**. This module makes hyperpanes speak the
//! server side of that protocol, so those clients see hyperpanes panes as tmux panes.
//!
//! Everything here is **pure**: no sockets, no daemon, no tty. [`ControlServer`] is fed
//! command lines and pane events and answers with byte lines; the transport is the caller's
//! problem (`app/src/control_mode_cli.rs` drives it over stdin/stdout, and M3's SSH channel
//! can drive the same object). That is what makes the wire format exhaustively testable, and
//! the wire format is the whole product here.
//!
//! ## Fidelity — what was confirmed, and how
//! The formats below were taken from tmux's own source and then checked byte-for-byte
//! against a transcript captured from a real `tmux -CC` (3.7b) on this machine. Citations:
//!
//! * **`%output` escaping** — `control.c:control_append_data`:
//!   `if (new_data[i] < ' ' || new_data[i] == '\\') printf("\\%03o")` else pass the byte
//!   through verbatim. So bytes `0x00..=0x1F` and `\` (0x5C) become three-digit octal, and
//!   **everything else is raw — including `0x7F` (DEL) and every byte `>= 0x80`.** Verified
//!   with a live capture of a pane emitting `\177 \200 \377 \\ \001 \037 \176 \040`, which
//!   came back as `7f 7c 80 7c ff 7c 5c 31 33 34 7c 5c 30 30 31 7c 5c 30 33 37 7c 7e 7c 20`
//!   — i.e. DEL and the high bytes untouched. This is the single most common place a
//!   re-implementation goes wrong, which is why [`escape_output`] returns **bytes, not a
//!   `String`**: building a `String` would re-encode `0x80..=0xFF` as two-byte UTF-8 and
//!   silently corrupt every non-ASCII pane.
//! * **Guard lines** — `control.c:control_write_guard`: `"%%%s %ld %u %d"` →
//!   `%begin <epoch-seconds> <command-number> <flags>`, closed by `%end` or `%error` with
//!   the *same* three fields. `flags` is `!!(state->flags & CMDQ_STATE_CONTROL)`
//!   (`cmd-queue.c:cmdq_fire_command`) — 1 for a command the control client typed, 0 for
//!   one tmux ran itself. Both values appear in the captured transcript.
//! * **Error body** — `control.c:control_error` writes `parse error: <message>` *inside*
//!   the block and closes it with `%error`, not `%end`.
//! * **Notification deferral** — `control.c:control_notify_write` never emits a
//!   notification inside an open guard block; it queues it and
//!   `control_write_guard` flushes the queue when the outermost block closes.
//!   [`ControlServer`] models that with [`ControlServer::notify`].
//! * **The `-CC` wrapper** — the server writes `ESC P 1000 p` on connect
//!   (`control.c:control_start`) and the client writes `ESC \` after `%exit`
//!   (`client.c`). We are both halves, so [`ControlMode::Wrapped`] emits both.
//! * **Layout strings** — `layout-custom.c:layout_dump` /`layout_append`:
//!   `"%04hx,%s"` of a body built from `WxH,x,y[,pane-id]` with `{}` for a left-right split
//!   and `[]` for top-bottom. The checksum is `csum = (csum >> 1) + ((csum & 1) << 15);
//!   csum += ch` over the body. [`layout_checksum`] reproduces it and its test asserts the
//!   two checksums (`b25d`, `a87d`) that the live transcript actually contained.
//! * **Input framing** — `control.c:control_read_callback` splits on **LF** and treats an
//!   **empty line as a detach**. We also tolerate a trailing CR, because a client that
//!   reaches us through a pty in ICRNL mode (tmux's own `-CC` client sets exactly that) or
//!   over an SSH channel may send CRLF.
//! * **User options** (`@name`) are a real store, not a stub: `cmd-set-option.c` lets a
//!   client write arbitrary `@`-prefixed options and read them back, and iTerm2 keeps its
//!   entire window model there (`@affinities` = which tmux windows share one iTerm2
//!   window, `@origins` = their screen positions, `@hidden`, `@tabcolors`, `@iterm2_id`).
//!   They are pure client scratch space — tmux itself never reads them — so honouring
//!   them is honest, and without them a reconnect re-opens every pane as an ungrouped,
//!   unpositioned tab. Every *other* option still errors: there is no hyperpanes setting
//!   behind `status` or `default-terminal` to change.
//! * **The commands a real client sends** were read out of iTerm2's own source
//!   (`sources/tmux/TmuxController.m`, `TmuxGateway.m`, `TmuxWindowOpener.m`) rather than
//!   guessed — see [`ControlServer::command`] for the list and where each one came from.
//!
//! ## The id mapping — DECIDED: one tmux window per hyperpanes pane
//! hyperpanes has durable pane uids (`pane-<uuid>`, M0) and no window/tab concept below the
//! GUI, so M4 has to invent one. It maps **one tmux window to one hyperpanes pane**, all
//! inside a single tmux session `$0`. That is not a compromise — it is the product goal:
//! iTerm2 opens a native tab per tmux *window*, so this is what turns hyperpanes panes into
//! iTerm2 tabs.
//!
//! Clients cache these ids, so [`IdMap`] derives them from the uid rather than handing out a
//! counter: `id = fnv1a64("pane" ++ uid)` folded to 31 bits, with deterministic linear
//! probing (in sorted-uid order) on the rare collision. Consequences, stated plainly:
//! **the same set of uids always produces the same ids**, in any process, after any
//! reconnect, with no persisted state — which is exactly what a client's cache needs. A
//! collision (~1 in 10^7 for a few dozen panes) can shift one id when the pane *set*
//! changes; it cannot ever alias two live panes onto one id.
//!
//! ## Resize policy
//! Control mode reuses M2's [`ResizePolicy`](crate::session::attach::ResizePolicy) and its
//! default, [`Observe`](crate::session::attach::ResizePolicy::Observe). This is coherent
//! rather than merely conservative: we report the pane's real grid in `#{window_width}` /
//! `#{window_height}` / `#{window_layout}`, and iTerm2 sizes its native tab to the layout it
//! is told. So `refresh-client -C` is acknowledged (never `%error`ed — iTerm2 sends it
//! before it can know our policy) but does not reflow anyone's desktop.
//! [`ResizePolicy::Request`] opts in.

use std::collections::{BTreeMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::session::attach::ResizePolicy;

/// The tmux version we claim in `#{version}` / `display-message -p "#{version}"`.
///
/// iTerm2 feature-gates on this string (`TmuxController.m -handleDisplayMessageVersion:`);
/// claiming 3.2 selects the modern paths we actually implement — `send-keys -H` literal
/// bytes, `#{window_visible_layout}`, `refresh-client -C w,h` — without claiming 3.3+
/// features (`%pause` flow control, `refresh-client -B` subscriptions) that we do not.
pub const CLAIMED_VERSION: &str = "3.2";

/// One control-mode line, **without** its terminating newline.
///
/// Bytes rather than `String` because `%output` carries raw pane bytes: an escaped payload
/// keeps every byte `>= 0x80` verbatim (see the module docs), which is not valid UTF-8 in
/// general and must never be re-encoded.
pub type Line = Vec<u8>;

/// Whether to wrap the stream in the `-CC` DCS envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ControlMode {
    /// `tmux -C`: bare lines.
    Plain,
    /// `tmux -CC`: `ESC P 1000 p` before the first line and `ESC \` after `%exit`. This is
    /// what iTerm2 and the mobile clients drive.
    #[default]
    Wrapped,
}

/// Opening DCS of the `-CC` envelope (`control.c:control_start`).
pub const DCS_OPEN: &[u8] = b"\x1bP1000p";
/// Closing ST of the `-CC` envelope (`client.c`, printed after `%exit`).
pub const DCS_CLOSE: &[u8] = b"\x1b\\";

/// Whether an emitted [`Line`] takes a trailing `\n` on the wire.
///
/// Every protocol line does (`control.c:control_write_line` writes the text then a single
/// `"\n"` — never `"\r\n"`; the `\r` seen in a captured transcript is the tty's `ONLCR`).
/// The two DCS wrapper strings do **not**: the live capture shows `\x1bP1000p` running
/// straight into `%begin` with no separator, and the closing `\x1b\\` following `%exit\n`
/// as the last bytes of the stream with nothing after it.
pub fn needs_newline(line: &[u8]) -> bool {
    line != DCS_OPEN && line != DCS_CLOSE
}

// ---------------------------------------------------------------------------
// %output escaping
// ---------------------------------------------------------------------------

/// Escape raw pane bytes for a `%output` payload, exactly as tmux does.
///
/// `b < 0x20 || b == b'\\'` → `\NNN` (three-digit octal); every other byte passes through
/// **unchanged**, DEL (`0x7F`) and the whole `0x80..=0xFF` range included. See the module
/// docs for the source citation and the live-capture verification.
pub fn escape_output(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if b < 0x20 || b == b'\\' {
            out.push(b'\\');
            out.push(b'0' + (b >> 6));
            out.push(b'0' + ((b >> 3) & 7));
            out.push(b'0' + (b & 7));
        } else {
            out.push(b);
        }
    }
    out
}

/// Inverse of [`escape_output`] — used by the round-trip tests and by anyone writing a
/// control-mode *client* against this module. A malformed escape is passed through
/// literally rather than dropped, because silently losing bytes is worse than showing them.
pub fn unescape_output(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let digits = &bytes[i + 1..i + 4];
            if digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                let v = (digits[0] - b'0') as u16 * 64
                    + (digits[1] - b'0') as u16 * 8
                    + (digits[2] - b'0') as u16;
                if v <= 0xFF {
                    out.push(v as u8);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Layout strings
// ---------------------------------------------------------------------------

/// tmux's layout checksum (`layout-custom.c:layout_checksum`): a 16-bit rotate-right-then-add
/// over the layout body, rendered by the caller as `%04x`.
pub fn layout_checksum(body: &str) -> u16 {
    let mut csum: u16 = 0;
    for b in body.bytes() {
        csum = (csum >> 1) | ((csum & 1) << 15);
        csum = csum.wrapping_add(b as u16);
    }
    csum
}

/// The layout string for a window holding exactly one pane — which is every window we
/// publish, since M4 maps one tmux window to one hyperpanes pane.
///
/// Body shape is `layout_append`'s `"%ux%u,%d,%d,%u"`; the checksum prefix is
/// `layout_dump`'s `"%04hx,%s"`.
pub fn single_pane_layout(cols: u16, rows: u16, pane_id: u32) -> String {
    let body = format!("{cols}x{rows},0,0,{pane_id}");
    format!("{:04x},{}", layout_checksum(&body), body)
}

// ---------------------------------------------------------------------------
// Stable id mapping
// ---------------------------------------------------------------------------

/// FNV-1a over `domain ++ uid`, folded to 31 bits.
///
/// 31 rather than 32 bits because tmux ids are `u_int` on the wire but iTerm2 parses them
/// with `intValue` (a signed 32-bit `int`) — `TmuxController.m` passes them around as `int`
/// throughout — so an id with the top bit set would come back negative.
fn hash31(domain: &str, uid: &str) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in domain.bytes().chain(uid.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    ((h ^ (h >> 32)) as u32) & 0x7fff_ffff
}

/// The uid ↔ tmux-id mapping. See the module docs for why it hashes rather than counts.
///
/// Pane ids (`%n`) and window ids (`@n`) are separate tmux namespaces, so the two maps are
/// derived independently and may collide with each other harmlessly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdMap {
    panes: BTreeMap<String, u32>,
    windows: BTreeMap<String, u32>,
}

impl IdMap {
    /// Build the mapping for exactly this set of uids. A pure function of the *set* (order
    /// of the slice is irrelevant — it is sorted first), so two processes, or the same
    /// process before and after a reconnect, always agree.
    pub fn rebuild(uids: &[String]) -> Self {
        let mut sorted: Vec<&String> = uids.iter().collect();
        sorted.sort();
        sorted.dedup();
        Self {
            panes: Self::assign("pane", &sorted),
            windows: Self::assign("window", &sorted),
        }
    }

    fn assign(domain: &str, sorted: &[&String]) -> BTreeMap<String, u32> {
        let mut taken: HashSet<u32> = HashSet::new();
        let mut out = BTreeMap::new();
        for uid in sorted {
            let mut id = hash31(domain, uid);
            // Deterministic probing: sorted order + a fixed step means the outcome depends
            // only on the uid set, never on when a pane happened to appear.
            while !taken.insert(id) {
                id = (id + 1) & 0x7fff_ffff;
            }
            out.insert((*uid).clone(), id);
        }
        out
    }

    /// This uid's tmux pane id (the `n` in `%n`).
    pub fn pane_id(&self, uid: &str) -> Option<u32> {
        self.panes.get(uid).copied()
    }

    /// This uid's tmux window id (the `n` in `@n`).
    pub fn window_id(&self, uid: &str) -> Option<u32> {
        self.windows.get(uid).copied()
    }

    /// Reverse lookup for a `%n` target in a client command.
    pub fn uid_for_pane(&self, id: u32) -> Option<&str> {
        self.panes
            .iter()
            .find(|(_, v)| **v == id)
            .map(|(k, _)| k.as_str())
    }

    /// Reverse lookup for an `@n` target in a client command.
    pub fn uid_for_window(&self, id: u32) -> Option<&str> {
        self.windows
            .iter()
            .find(|(_, v)| **v == id)
            .map(|(k, _)| k.as_str())
    }
}

// ---------------------------------------------------------------------------
// Panes
// ---------------------------------------------------------------------------

/// What the control server needs to know about one hyperpanes pane. The driver fills this
/// from `SessionMeta` plus whatever else it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneInfo {
    /// The durable hyperpanes uid (`pane-<uuid>`, M0). The key for everything.
    pub uid: String,
    /// Pane grid width, or `None` if the daemon predates `SessionMeta::cols`.
    pub cols: Option<u16>,
    /// Pane grid height.
    pub rows: Option<u16>,
    /// Last sniffed cwd (OSC 7), reported as `#{pane_current_path}`.
    pub cwd: Option<String>,
    /// The window/pane title. Defaults to the uid's short form when unknown.
    pub title: Option<String>,
    /// Plain-text screen mirror, if the driver has refreshed it — see
    /// [`wants_screen_refresh`]. `capture-pane -p` answers from this.
    pub screen: Option<String>,
}

impl PaneInfo {
    /// A pane with nothing known but its uid.
    pub fn new(uid: impl Into<String>) -> Self {
        Self {
            uid: uid.into(),
            cols: None,
            rows: None,
            cwd: None,
            title: None,
            screen: None,
        }
    }

    /// Grid width with tmux's 80x24 fallback for an unknown grid.
    fn width(&self) -> u16 {
        self.cols.unwrap_or(80).max(1)
    }

    /// Grid height with tmux's 80x24 fallback for an unknown grid.
    fn height(&self) -> u16 {
        self.rows.unwrap_or(24).max(1)
    }

    /// The window name a client shows on the tab. The full `pane-<uuid>` is unreadable on a
    /// tab, so fall back to the uuid's first segment.
    fn name(&self) -> String {
        if let Some(t) = self.title.as_deref().filter(|t| !t.trim().is_empty()) {
            return t.to_string();
        }
        let short = self.uid.strip_prefix("pane-").unwrap_or(&self.uid);
        short.split('-').next().unwrap_or(short).to_string()
    }
}

// ---------------------------------------------------------------------------
// Driver actions
// ---------------------------------------------------------------------------

/// Something the *driver* must do on the server's behalf, because it needs the daemon.
///
/// Returned alongside the reply lines so the pure state machine never touches a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Forward these bytes to the pane's pty (`send-keys`).
    Write { uid: String, data: Vec<u8> },
    /// Resize the pane. **Only** produced under [`ResizePolicy::Request`].
    Resize { uid: String, cols: u16, rows: u16 },
    /// The client asked to leave (`detach`, or the empty-line detach). The driver should
    /// emit [`ControlServer::goodbye`] and close the transport.
    Detach,
}

/// One command's complete effect: the lines to write, and the work to hand the daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reaction {
    /// Lines to write to the client, in order, each needing a trailing `\n`.
    pub lines: Vec<Line>,
    /// Side effects for the driver to perform.
    pub actions: Vec<Action>,
}

/// Where `%begin`'s timestamp comes from. Fixed in tests so transcripts are comparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clock {
    /// Wall clock, epoch seconds — what tmux uses (`item->time`).
    System,
    /// A frozen value, for tests.
    Fixed(u64),
}

impl Clock {
    fn now(&self) -> u64 {
        match self {
            Clock::System => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            Clock::Fixed(t) => *t,
        }
    }
}

/// Whether this command line needs a fresh screen mirror before it is dispatched.
///
/// [`ControlServer::command`] is synchronous and pure, so it cannot go and ask the daemon
/// for a pane's screen mid-dispatch. `capture-pane` is the only command that needs one, so
/// the driver peeks with this and refreshes [`PaneInfo::screen`] first.
pub fn wants_screen_refresh(line: &str) -> bool {
    let head = line.trim_start();
    let word = head.split(|c: char| c.is_whitespace()).next().unwrap_or("");
    matches!(word, "capture-pane" | "capturep")
}

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

/// The control-mode state machine: hyperpanes panes in, tmux control protocol out.
///
/// Single-threaded and synchronous by design. Feed it [`command`](Self::command) lines from
/// the client and [`output`](Self::output) / [`pane_exited`](Self::pane_exited) /
/// [`pane_added`](Self::pane_added) from the daemon; write everything it returns.
pub struct ControlServer {
    session_name: String,
    /// Always `$0`: M4 publishes exactly one tmux session (see the module docs).
    session_id: u32,
    mode: ControlMode,
    policy: ResizePolicy,
    clock: Clock,
    ids: IdMap,
    /// uid → pane, sorted, so window indices and `list-*` output have a stable order.
    panes: BTreeMap<String, PaneInfo>,
    /// The uid the client last selected; drives `#{window_active}` and the `*` flag.
    active: Option<String>,
    /// tmux's global command counter. Starts at 1; the greeting's synthetic block uses 0.
    next_command: u32,
    /// Open `%begin` blocks (`control.c`'s `cs->guard_depth`).
    guard_depth: u32,
    /// Notifications produced while a guard block was open (`control.c`'s `cs->deferred`).
    deferred: Vec<Line>,
    /// tmux **user options** (`@name`) the client has stored, keyed by
    /// `(scope, name)` — see [`ControlServer::option_scope`]. iTerm2 keeps its whole
    /// window model in here (`@affinities`, `@origins`, `@hidden`, `@iterm2_id`), writing
    /// with `set -t $0 @… …` and reading it back with `show -v -q -t $0 @…`, so without a
    /// store every reconnect re-opens the panes as ungrouped, unpositioned tabs.
    options: BTreeMap<(String, String), String>,
    /// Last size the client asked for via `refresh-client -C`, reported as `#{client_width}`.
    client_size: (u16, u16),
    /// Reported as `#{client_name}`.
    client_name: String,
}

impl ControlServer {
    /// A server publishing `panes` as one tmux session named `session_name`.
    pub fn new(session_name: impl Into<String>, panes: Vec<PaneInfo>) -> Self {
        let mut s = Self {
            session_name: session_name.into(),
            session_id: 0,
            mode: ControlMode::default(),
            policy: ResizePolicy::default(),
            clock: Clock::System,
            ids: IdMap::default(),
            panes: BTreeMap::new(),
            active: None,
            next_command: 1,
            guard_depth: 0,
            deferred: Vec::new(),
            options: BTreeMap::new(),
            client_size: (80, 24),
            client_name: "hyperpanes".to_string(),
        };
        for p in panes {
            s.panes.insert(p.uid.clone(), p);
        }
        s.reindex();
        s.active = s.panes.keys().next().cloned();
        s
    }

    /// `-C` (bare) instead of the default `-CC` (DCS-wrapped).
    pub fn with_mode(mut self, mode: ControlMode) -> Self {
        self.mode = mode;
        self
    }

    /// Opt in to reflowing panes on `refresh-client -C` — see the module docs.
    pub fn with_policy(mut self, policy: ResizePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Freeze `%begin`'s timestamp (tests).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The current uid → tmux id mapping.
    pub fn ids(&self) -> &IdMap {
        &self.ids
    }

    /// Replace a pane's cached screen mirror, for `capture-pane -p`.
    pub fn set_screen(&mut self, uid: &str, screen: Option<String>) {
        if let Some(p) = self.panes.get_mut(uid) {
            p.screen = screen;
        }
    }

    /// Record a pane's sniffed cwd (`SessionEvent::Cwd`), reported to the client as
    /// `#{pane_current_path}`. Silent for an unknown uid, and emits nothing: tmux has no
    /// notification for a cwd change, clients poll it with a format.
    pub fn set_cwd(&mut self, uid: &str, cwd: Option<String>) {
        if let Some(p) = self.panes.get_mut(uid) {
            p.cwd = cwd;
        }
    }

    /// Whether this uid is one of the panes currently published.
    pub fn has_pane(&self, uid: &str) -> bool {
        self.panes.contains_key(uid)
    }

    /// Every published uid, in the id-assignment order (sorted).
    pub fn uids(&self) -> Vec<String> {
        self.panes.keys().cloned().collect()
    }

    fn reindex(&mut self) {
        let uids: Vec<String> = self.panes.keys().cloned().collect();
        self.ids = IdMap::rebuild(&uids);
    }

    // -- framing -----------------------------------------------------------

    /// Emit a notification, or defer it if a `%begin` block is open.
    ///
    /// `control.c:control_notify_write`: notifications must never land inside a command's
    /// guard block, because a client reads everything between `%begin` and `%end` as that
    /// command's output.
    fn notify(&mut self, out: &mut Vec<Line>, line: String) {
        if self.guard_depth == 0 {
            out.push(line.into_bytes());
        } else {
            self.deferred.push(line.into_bytes());
        }
    }

    /// Wrap `body` (already-formatted output lines) in a `%begin`/`%end`-or-`%error` block,
    /// then flush anything deferred while it was open.
    fn guarded(&mut self, out: &mut Vec<Line>, body: Vec<String>, is_error: bool) {
        let t = self.clock.now();
        let n = self.next_command;
        self.next_command = self.next_command.wrapping_add(1);

        self.guard_depth += 1;
        out.push(format!("%begin {t} {n} 1").into_bytes());
        out.extend(body.into_iter().map(String::into_bytes));
        let closer = if is_error { "error" } else { "end" };
        out.push(format!("%{closer} {t} {n} 1").into_bytes());
        self.guard_depth -= 1;

        if self.guard_depth == 0 {
            out.append(&mut self.deferred);
        }
    }

    // -- lifecycle ---------------------------------------------------------

    /// The bytes a client sees the instant it connects, in tmux's own order.
    ///
    /// Mirrors the captured `tmux -CC` transcript exactly: the DCS opener, tmux's own
    /// synthetic empty command block (note **flags 0** — it is not the client's command),
    /// a `%window-add` per window, `%sessions-changed`, then `%session-changed`.
    pub fn greeting(&mut self) -> Vec<Line> {
        let mut out = Vec::new();
        if self.mode == ControlMode::Wrapped {
            out.push(DCS_OPEN.to_vec());
        }
        let t = self.clock.now();
        let n = 0;
        out.push(format!("%begin {t} {n} 0").into_bytes());
        out.push(format!("%end {t} {n} 0").into_bytes());

        let windows: Vec<u32> = self
            .panes
            .keys()
            .filter_map(|uid| self.ids.window_id(uid))
            .collect();
        for w in windows {
            out.push(format!("%window-add @{w}").into_bytes());
        }
        out.push(b"%sessions-changed".to_vec());
        out.push(
            format!(
                "%session-changed ${} {}",
                self.session_id, self.session_name
            )
            .into_bytes(),
        );
        out
    }

    /// `%exit [reason]` and, in `-CC`, the closing ST.
    ///
    /// `client.c` prints `%exit <message>` when there is an exit reason and bare `%exit`
    /// otherwise, then the ST.
    pub fn goodbye(&mut self, reason: Option<&str>) -> Vec<Line> {
        let mut out = Vec::new();
        out.push(match reason {
            Some(r) => format!("%exit {r}").into_bytes(),
            None => b"%exit".to_vec(),
        });
        if self.mode == ControlMode::Wrapped {
            out.push(DCS_CLOSE.to_vec());
        }
        out
    }

    // -- pane events -------------------------------------------------------

    /// A pane produced output → one `%output %<pane> <escaped>` line.
    ///
    /// Returns nothing for an unknown uid or an empty chunk rather than emitting a bare
    /// `%output` a client would have to special-case.
    pub fn output(&mut self, uid: &str, data: &[u8]) -> Vec<Line> {
        if data.is_empty() {
            return Vec::new();
        }
        let Some(pane) = self.ids.pane_id(uid) else {
            return Vec::new();
        };
        let mut line = format!("%output %{pane} ").into_bytes();
        line.extend_from_slice(&escape_output(data));
        vec![line]
    }

    /// A new hyperpanes pane appeared → a new tmux window.
    ///
    /// Re-deriving the whole [`IdMap`] here is deliberate: ids are a pure function of the
    /// uid set (module docs), so the map must be rebuilt, not appended to.
    pub fn pane_added(&mut self, info: PaneInfo) -> Vec<Line> {
        let uid = info.uid.clone();
        if self.panes.contains_key(&uid) {
            return Vec::new();
        }
        self.panes.insert(uid.clone(), info);
        self.reindex();
        if self.active.is_none() {
            self.active = Some(uid.clone());
        }
        let mut out = Vec::new();
        if let Some(w) = self.ids.window_id(&uid) {
            self.notify(&mut out, format!("%window-add @{w}"));
            let layout = self.layout_for(&uid);
            let flags = self.window_flags(&uid);
            self.notify(
                &mut out,
                format!("%layout-change @{w} {layout} {layout} {flags}"),
            );
        }
        out
    }

    /// A pane exited → its window closes. The tmux **session stays**, because the
    /// hyperpanes daemon is still there and other panes may still be live.
    pub fn pane_exited(&mut self, uid: &str) -> Vec<Line> {
        let Some(w) = self.ids.window_id(uid) else {
            return Vec::new();
        };
        self.panes.remove(uid);
        if self.active.as_deref() == Some(uid) {
            self.active = self.panes.keys().next().cloned();
        }
        self.reindex();
        let mut out = Vec::new();
        self.notify(&mut out, format!("%window-close @{w}"));
        out
    }

    /// A pane's grid changed → `%layout-change`, which is how a client learns to resize the
    /// native tab it is drawing.
    pub fn pane_resized(&mut self, uid: &str, cols: u16, rows: u16) -> Vec<Line> {
        let Some(p) = self.panes.get_mut(uid) else {
            return Vec::new();
        };
        if p.cols == Some(cols) && p.rows == Some(rows) {
            return Vec::new();
        }
        p.cols = Some(cols);
        p.rows = Some(rows);
        let Some(w) = self.ids.window_id(uid) else {
            return Vec::new();
        };
        let layout = self.layout_for(uid);
        let flags = self.window_flags(uid);
        let mut out = Vec::new();
        self.notify(
            &mut out,
            format!("%layout-change @{w} {layout} {layout} {flags}"),
        );
        out
    }

    /// A pane's title changed → `%window-renamed`.
    pub fn pane_renamed(&mut self, uid: &str, title: &str) -> Vec<Line> {
        let Some(p) = self.panes.get_mut(uid) else {
            return Vec::new();
        };
        if p.title.as_deref() == Some(title) {
            return Vec::new();
        }
        p.title = Some(title.to_string());
        let Some(w) = self.ids.window_id(uid) else {
            return Vec::new();
        };
        let name = self.panes[uid].name();
        let mut out = Vec::new();
        self.notify(&mut out, format!("%window-renamed @{w} {name}"));
        out
    }

    // -- helpers -----------------------------------------------------------

    fn layout_for(&self, uid: &str) -> String {
        let p = &self.panes[uid];
        let pane = self.ids.pane_id(uid).unwrap_or(0);
        single_pane_layout(p.width(), p.height(), pane)
    }

    /// tmux's `#{window_flags}`: `*` marks the active window, `-` the previous one. We have
    /// no "previous window" concept, so only `*` is ever produced.
    fn window_flags(&self, uid: &str) -> String {
        if self.active.as_deref() == Some(uid) {
            "*".to_string()
        } else {
            String::new()
        }
    }

    fn window_index(&self, uid: &str) -> usize {
        self.panes.keys().position(|k| k == uid).unwrap_or(0)
    }

    /// Resolve a `-t` target to a uid. Accepts `%n` (pane), `@n` (window), `$n` (session →
    /// the active pane), a bare hyperpanes uid, and a window index.
    fn resolve_target(&self, target: &str) -> Option<String> {
        let t = target.trim().trim_matches('"').trim_matches('\'');
        if let Some(rest) = t.strip_prefix('%') {
            return rest
                .parse::<u32>()
                .ok()
                .and_then(|id| self.ids.uid_for_pane(id))
                .map(str::to_string);
        }
        if let Some(rest) = t.strip_prefix('@') {
            return rest
                .parse::<u32>()
                .ok()
                .and_then(|id| self.ids.uid_for_window(id))
                .map(str::to_string);
        }
        if t.strip_prefix('$').is_some() {
            return self.active.clone();
        }
        if self.panes.contains_key(t) {
            return Some(t.to_string());
        }
        t.parse::<usize>()
            .ok()
            .and_then(|i| self.panes.keys().nth(i).cloned())
    }

    // -- commands ----------------------------------------------------------

    /// Handle one line from the client.
    ///
    /// An **empty line detaches** (`control.c:control_read_callback`). A trailing CR is
    /// stripped first: tmux's own `-CC` client relies on the pty's `ICRNL` to turn the CR
    /// into the LF the server splits on, and an SSH channel may deliver CRLF verbatim.
    ///
    /// The implemented set was read out of iTerm2's source rather than guessed:
    /// `list-sessions -F "#{session_id} #{session_name}"`, the tab-separated
    /// `list-windows -F …` and `list-panes -s -t $n -F "#{pane_id}"` from
    /// `TmuxController.m`; `send -lt`/`send -t 0xNN`/`send -H -t` and `detach` from
    /// `TmuxGateway.m`; `capture-pane -p -P -C` and `capture-pane -peqJN -t … -S -N` from
    /// `TmuxWindowOpener.m`; `refresh-client -C w,h` and `refresh-client -C @n:wxh` from
    /// `TmuxController.m -commandListToSetWindowSizes:`; and the user-option round trip
    /// `set -t $0 @affinities …` / `show -v -q -t $0 @affinities` (also `@origins`,
    /// `@hidden`, `@tabcolors`, `@iterm2_id`) from `TmuxController.m -saveAffinities`,
    /// `-saveWindowOrigins` and `-saveHiddenWindows` — that store is how a client keeps
    /// its tab grouping and window positions across a reconnect.
    ///
    /// **Anything else answers `%error`**, in tmux's exact shape — a `parse error: unknown
    /// command: <name>` body line closed by `%error` — rather than a silent `%end` that
    /// would make a client believe a destructive command had succeeded.
    pub fn command(&mut self, line: &str) -> Reaction {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let line = line.strip_suffix('\r').unwrap_or(line);

        let mut r = Reaction::default();
        if line.trim().is_empty() {
            r.actions.push(Action::Detach);
            return r;
        }

        let words = match split_words(line) {
            Ok(w) => w,
            Err(e) => {
                self.guarded(&mut r.lines, vec![format!("parse error: {e}")], true);
                return r;
            }
        };
        if words.is_empty() {
            r.actions.push(Action::Detach);
            return r;
        }

        match self.dispatch(&words, &mut r.actions) {
            Ok(body) => self.guarded(&mut r.lines, body, false),
            Err(msg) => self.guarded(&mut r.lines, vec![msg], true),
        }
        r
    }

    /// Run one parsed command. `Ok(body)` closes with `%end`, `Err(message)` with `%error`.
    fn dispatch(
        &mut self,
        words: &[String],
        actions: &mut Vec<Action>,
    ) -> Result<Vec<String>, String> {
        let name = words[0].as_str();
        let rest = &words[1..];
        match name {
            // `ls`/`lsw`/`lsp` are tmux's own aliases; iTerm2 spells them out but a human
            // poking at the socket will not.
            "list-sessions" | "ls" => self.cmd_list_sessions(rest),
            "list-windows" | "lsw" => self.cmd_list_windows(rest),
            "list-panes" | "lsp" => self.cmd_list_panes(rest),
            "list-clients" | "lsc" => self.cmd_list_clients(rest),
            "display-message" | "display" | "displayp" => self.cmd_display_message(rest),
            "send-keys" | "send" => self.cmd_send_keys(rest, actions),
            "refresh-client" | "refresh" => self.cmd_refresh_client(rest, actions),
            "resize-window" | "resizew" | "resize-pane" | "resizep" => {
                self.cmd_resize(rest, actions)
            }
            "capture-pane" | "capturep" => self.cmd_capture_pane(rest),
            "select-pane" | "selectp" | "select-window" | "selectw" => self.cmd_select(rest),
            // The `-window-` spellings imply `-w` (`cmd-set-option.c` sets
            // `CMD_SET_OPTION_WINDOW` from the entry, not from the flags).
            "show-options" | "show" => self.cmd_show_options(rest, false),
            "show-window-options" | "showw" => self.cmd_show_options(rest, true),
            "set-option" | "set" => self.cmd_set_option(rest, false),
            "set-window-option" | "setw" => self.cmd_set_option(rest, true),
            "has-session" | "has" => self.cmd_has_session(rest),
            "detach-client" | "detach" => {
                actions.push(Action::Detach);
                Ok(Vec::new())
            }
            // Deliberately NOT silently accepted. These are the destructive and
            // structure-changing commands; hyperpanes owns pane lifetime through the GUI
            // and the daemon, and a control client inventing panes or killing someone's
            // shell is exactly the failure mode the plan's "return %error for what you do
            // not implement" rule exists to prevent.
            "kill-pane" | "killp" | "kill-window" | "killw" | "kill-session" | "kill-server"
            | "new-window" | "neww" | "new-session" | "new" | "split-window" | "splitw"
            | "break-pane" | "breakp" | "join-pane" | "joinp" | "move-window" | "movew"
            | "swap-pane" | "swapp" | "swap-window" | "swapw" | "link-window" | "linkw"
            | "unlink-window" | "unlinkw" | "respawn-pane" | "respawnp" | "respawn-window"
            | "rename-window" | "renamew" | "rename-session" | "rename" => Err(format!(
                "{name} is not supported by hyperpanes control mode: pane and window \
                 lifetime is owned by the hyperpanes app, not by the control client"
            )),
            other => Err(format!("parse error: unknown command: {other}")),
        }
    }

    fn cmd_list_sessions(&mut self, args: &[String]) -> Result<Vec<String>, String> {
        let a = Args::parse(args, "F:f:")?;
        let ctx = FmtCtx {
            uid: self.active.clone(),
        };
        match a.value('F') {
            // tmux's own default `list-sessions` line, reproduced from a live capture:
            // `cap: 1 windows (created Sat Aug 29 16:27:42 2026) (attached)`. We have no
            // creation time to report, so the parenthetical is dropped rather than faked.
            None => Ok(vec![format!(
                "{}: {} windows (attached)",
                self.session_name,
                self.panes.len()
            )]),
            Some(f) => Ok(vec![self.expand(f, &ctx)]),
        }
    }

    fn cmd_list_windows(&mut self, args: &[String]) -> Result<Vec<String>, String> {
        let a = Args::parse(args, "F:t:f:")?;
        let fmt = a.value('F');
        // A `-t` naming a *window* narrows to it. iTerm2's actual call is
        // `list-windows -F … -t $0`, a *session* target, which means "every window" — and
        // `-a` (all sessions) is the same set for us, since there is only one session.
        let target = a.value('t');
        let narrow = target.map(|t| t.starts_with('@')).unwrap_or(false);
        let uids: Vec<String> = match target
            .filter(|_| narrow)
            .and_then(|t| self.resolve_target(t))
        {
            Some(u) => vec![u],
            None => self.panes.keys().cloned().collect(),
        };
        Ok(uids
            .into_iter()
            .map(|uid| {
                let ctx = FmtCtx { uid: Some(uid) };
                match fmt {
                    Some(f) => self.expand(f, &ctx),
                    // tmux's default, from the live capture:
                    // `0: zsh* (1 panes) [80x24] [layout b25d,80x24,0,0,0] @0 (active)`
                    None => self.expand(
                        "#{window_index}: #{window_name}#{window_flags} (#{window_panes} panes) \
                         [#{window_width}x#{window_height}] [layout #{window_layout}] \
                         #{window_id}#{?window_active, (active),}",
                        &ctx,
                    ),
                }
            })
            .collect())
    }

    fn cmd_list_panes(&mut self, args: &[String]) -> Result<Vec<String>, String> {
        let a = Args::parse(args, "F:t:f:")?;
        let fmt = a.value('F');
        // `-s` = every pane in the session, `-a` = every pane on the server; with one
        // session those are the same thing. Without either, `-t` narrows to one.
        let all = a.flag('s') || a.flag('a');
        let uids: Vec<String> = match (all, a.value('t').and_then(|t| self.resolve_target(t))) {
            (false, Some(u)) => vec![u],
            _ => self.panes.keys().cloned().collect(),
        };
        Ok(uids
            .into_iter()
            .map(|uid| {
                let ctx = FmtCtx { uid: Some(uid) };
                match fmt {
                    Some(f) => self.expand(f, &ctx),
                    // tmux's default, from the live capture:
                    // `0: [80x24] [history 1/2000, 1800 bytes] %0 (active)`.
                    // The history counters are always zero — the daemon keeps a rolling
                    // replay buffer, not a tmux-shaped scrollback we can measure in lines.
                    None => self.expand(
                        "#{pane_index}: [#{pane_width}x#{pane_height}] \
                         [history #{history_size}/#{history_limit}, #{history_bytes} bytes] \
                         #{pane_id}#{?pane_active, (active),}",
                        &ctx,
                    ),
                }
            })
            .collect())
    }

    fn cmd_list_clients(&mut self, args: &[String]) -> Result<Vec<String>, String> {
        let a = Args::parse(args, "F:t:f:")?;
        let ctx = FmtCtx {
            uid: self.active.clone(),
        };
        Ok(vec![match a.value('F') {
            Some(f) => self.expand(f, &ctx),
            None => format!(
                "{}: {} [{}x{} control] (control mode)",
                self.client_name, self.session_name, self.client_size.0, self.client_size.1
            ),
        }])
    }

    fn cmd_display_message(&mut self, args: &[String]) -> Result<Vec<String>, String> {
        // iTerm2's version probe is `display-message -p "#{version}"`; `-p` means "print to
        // stdout" which, in control mode, means "put it in the block body".
        let a = Args::parse(args, "t:c:F:")?;
        let ctx = FmtCtx {
            uid: a
                .value('t')
                .and_then(|t| self.resolve_target(t))
                .or_else(|| self.active.clone()),
        };
        let template = a
            .positionals
            .first()
            .cloned()
            .or_else(|| a.value('F').map(str::to_string))
            .unwrap_or_else(|| "[#{session_name}] #{window_index}:#{window_name}".to_string());
        Ok(vec![self.expand(&template, &ctx)])
    }

    fn cmd_send_keys(
        &mut self,
        args: &[String],
        actions: &mut Vec<Action>,
    ) -> Result<Vec<String>, String> {
        let a = Args::parse(args, "t:N:X:")?;
        let uid = a
            .value('t')
            .and_then(|t| self.resolve_target(t))
            .or_else(|| self.active.clone())
            .ok_or_else(|| "can't find pane".to_string())?;

        let mut data = Vec::new();
        if a.flag('H') {
            // `send -H -t %n 41 42 …` — each argument is one literal byte in hex.
            // (`TmuxGateway.m -numbersAsLiteralByteHexArguments:`.)
            for w in &a.positionals {
                let b = u8::from_str_radix(w, 16).map_err(|_| format!("invalid hex byte '{w}'"))?;
                data.push(b);
            }
        } else if a.flag('l') {
            // `send -lt %n <chars>` — literal text, arguments concatenated with no
            // separator (tmux sends each argument's bytes in turn).
            for w in &a.positionals {
                data.extend_from_slice(w.as_bytes());
            }
        } else {
            // Bare arguments are key names — or, from iTerm2, `0xNN` code points
            // (`TmuxGateway.m -numbersAsHexStrings`), which are code *points*, so they
            // UTF-8 encode rather than being written raw.
            for w in &a.positionals {
                if let Some(hex) = w.strip_prefix("0x").or_else(|| w.strip_prefix("0X")) {
                    let cp =
                        u32::from_str_radix(hex, 16).map_err(|_| format!("invalid key '{w}'"))?;
                    match char::from_u32(cp) {
                        Some(c) => {
                            let mut buf = [0u8; 4];
                            data.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                        }
                        None => return Err(format!("invalid key '{w}'")),
                    }
                } else {
                    data.extend_from_slice(&key_name_to_bytes(w)?);
                }
            }
        }

        if !data.is_empty() {
            actions.push(Action::Write { uid, data });
        }
        Ok(Vec::new())
    }

    fn cmd_refresh_client(
        &mut self,
        args: &[String],
        actions: &mut Vec<Action>,
    ) -> Result<Vec<String>, String> {
        // Every value-taking flag iTerm2 uses: `-C` sizing, `-f` client flags,
        // `-A` flow control, `-B` format subscriptions, `-t` target.
        let a = Args::parse(args, "C:f:A:B:t:")?;
        let Some(spec) = a.value('C') else {
            // `-f`, `-A`, `-B`, `-r`, `-S` are accepted and ignored. They configure
            // behaviour we do not implement (flow control, pausing, subscriptions), and
            // erroring would abort iTerm2's attach — it sends them unconditionally.
            return Ok(Vec::new());
        };

        // Three spellings across tmux versions, all seen in the wild:
        //   `-C w,h`      (tmux 3.2+, what iTerm2 sends today)
        //   `-C wxh`      (tmux 3.1)
        //   `-C @n:wxh`   (per-window, tmux 3.3+; iTerm2 uses it when it thinks we support it)
        let (target, dims) = match spec.split_once(':') {
            Some((t, d)) if t.starts_with('@') => (Some(t.to_string()), d),
            _ => (None, spec),
        };
        let (w, h) = dims
            .split_once([',', 'x'])
            .ok_or_else(|| format!("bad size '{spec}'"))?;
        let w: u16 = w.trim().parse().map_err(|_| format!("bad size '{spec}'"))?;
        let h: u16 = h.trim().parse().map_err(|_| format!("bad size '{spec}'"))?;
        self.client_size = (w, h);

        if self.policy == ResizePolicy::Request {
            let uids: Vec<String> = match target.as_deref().and_then(|t| self.resolve_target(t)) {
                Some(u) => vec![u],
                None => self.panes.keys().cloned().collect(),
            };
            for uid in uids {
                actions.push(Action::Resize {
                    uid,
                    cols: w,
                    rows: h,
                });
            }
        }
        // Under `Observe` this is a successful no-op: the client is told nothing changed,
        // and the layout it already has still describes the pane. See the module docs.
        Ok(Vec::new())
    }

    fn cmd_resize(
        &mut self,
        args: &[String],
        actions: &mut Vec<Action>,
    ) -> Result<Vec<String>, String> {
        let a = Args::parse(args, "t:x:y:")?;
        let uid = a
            .value('t')
            .and_then(|t| self.resolve_target(t))
            .or_else(|| self.active.clone())
            .ok_or_else(|| "can't find pane".to_string())?;
        let cur = self.panes.get(&uid).ok_or("can't find pane")?;
        let cols = match a.value('x') {
            Some(v) => v.parse().map_err(|_| format!("bad width '{v}'"))?,
            None => cur.width(),
        };
        let rows = match a.value('y') {
            Some(v) => v.parse().map_err(|_| format!("bad height '{v}'"))?,
            None => cur.height(),
        };
        if self.policy == ResizePolicy::Request {
            actions.push(Action::Resize { uid, cols, rows });
        }
        Ok(Vec::new())
    }

    fn cmd_capture_pane(&mut self, args: &[String]) -> Result<Vec<String>, String> {
        // iTerm2 sends `capture-pane -p -P -C -t "%n"` (pending output) and
        // `capture-pane -peqJN -t "%n" -S -<n>` (scrollback). We answer both from the
        // daemon's headless screen mirror; see the honesty note below.
        let a = Args::parse(args, "t:S:E:b:")?;
        let uid = a
            .value('t')
            .and_then(|t| self.resolve_target(t))
            .or_else(|| self.active.clone())
            .ok_or_else(|| "can't find pane".to_string())?;
        // `-P` asks only for output not yet sent to this client. We stream every byte as
        // `%output` the moment it arrives, so there is never any pending — an empty
        // successful reply is the honest answer, not an error.
        if a.flag('P') {
            return Ok(Vec::new());
        }
        let screen = self
            .panes
            .get(&uid)
            .and_then(|p| p.screen.as_deref())
            .unwrap_or("");
        if screen.is_empty() {
            return Ok(Vec::new());
        }
        // The mirror is the visible grid only, so `-S`/`-E` (scrollback range) cannot be
        // honoured and are ignored; `-e` (keep escape sequences) cannot be either, because
        // the mirror is already rendered to plain text.
        Ok(screen
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect::<Vec<_>>())
    }

    fn cmd_select(&mut self, args: &[String]) -> Result<Vec<String>, String> {
        let a = Args::parse(args, "t:")?;
        let Some(uid) = a.value('t').and_then(|t| self.resolve_target(t)) else {
            return Err("can't find pane".to_string());
        };
        if self.active.as_deref() == Some(uid.as_str()) {
            return Ok(Vec::new());
        }
        self.active = Some(uid.clone());
        // Deferred, not emitted inline: these are notifications and a guard block is open.
        if let (Some(w), Some(p)) = (self.ids.window_id(&uid), self.ids.pane_id(&uid)) {
            self.deferred
                .push(format!("%session-window-changed ${} @{w}", self.session_id).into_bytes());
            self.deferred
                .push(format!("%window-pane-changed @{w} %{p}").into_bytes());
        }
        Ok(Vec::new())
    }

    /// Which option store a `set`/`show` addresses.
    ///
    /// tmux has four scopes (server / session / window / pane) plus a "global" tier per
    /// scope, and a real inheritance chain between them. We do not model the chain — we
    /// only need writes and reads of the *same* option to land in the same bucket, which
    /// is all iTerm2 does. So a scope is just a key string.
    fn option_scope(&self, a: &Args) -> String {
        if a.flag('s') {
            return "server".to_string();
        }
        let pane = a.flag('p');
        let window = a.flag('w');
        if a.flag('g') {
            return match (pane, window) {
                (true, _) => "global-pane".to_string(),
                (_, true) => "global-window".to_string(),
                _ => "global".to_string(),
            };
        }
        if pane || window {
            let tier = if pane { "pane" } else { "window" };
            // A `-t` we cannot resolve falls back to the global tier rather than minting a
            // per-target bucket keyed on a string that names nothing.
            return match a.value('t').and_then(|t| self.resolve_target(t)) {
                Some(uid) => format!("{tier}:{uid}"),
                None => format!("global-{tier}"),
            };
        }
        // Session scope. M4 publishes exactly one session, so every `-t $n` lands here.
        format!("${}", self.session_id)
    }

    /// `set-option` / `set` / `set-window-option` / `setw`.
    ///
    /// **Only user options (`@name`) are honoured.** Those are pure client-side scratch
    /// storage — tmux itself never reads them — so storing one is honest. A real tmux
    /// option (`status`, `default-terminal`, `mouse`, …) errors instead: hyperpanes has no
    /// option store behind it, and a silent `%end` would tell the client a setting took
    /// effect when nothing changed. Same rule as the lifecycle commands above.
    fn cmd_set_option(
        &mut self,
        args: &[String],
        window_scope: bool,
    ) -> Result<Vec<String>, String> {
        let mut a = Args::parse(args, "t:")?;
        if window_scope {
            a.flags.insert('w');
        }
        let name = a
            .positionals
            .first()
            .cloned()
            .ok_or("set-option: not enough arguments")?;
        if !name.starts_with('@') {
            return Err(format!(
                "{name} is not supported by hyperpanes control mode: only user options \
                 (@name) are stored — hyperpanes has no tmux option store behind the rest"
            ));
        }
        let scope = self.option_scope(&a);
        // `-u`/`-U` unset.
        if a.flag('u') || a.flag('U') {
            self.options.remove(&(scope, name));
            return Ok(Vec::new());
        }
        let value = a.positionals.get(1).cloned().unwrap_or_default();
        // `-a` appends to the current value (`cmd-set-option.c`); iTerm2 does not use it,
        // but a human at the socket reasonably expects tmux's semantics.
        let value = if a.flag('a') {
            let mut prev = self
                .options
                .get(&(scope.clone(), name.clone()))
                .cloned()
                .unwrap_or_default();
            prev.push_str(&value);
            prev
        } else {
            value
        };
        self.options.insert((scope, name), value);
        Ok(Vec::new())
    }

    /// `show-options` / `show` / `show-window-options` / `showw`.
    ///
    /// iTerm2 probes a pile of options and user options: `show -v -q -t $0 @iterm2_id`,
    /// `@affinities`, `@origins`, `@hidden`, `show-options -v -s default-terminal`, …
    /// A user option someone `set` comes back; anything else is unset.
    ///
    /// `-q` means "do not error if it is unset", and iTerm2 passes it on the ones it cares
    /// about. `-v` ("value only") is likewise answered with an empty successful block on a
    /// miss rather than an error, because a client asking only for a value treats an error
    /// block as a protocol failure. A bare `show <name>` on something unset errors, as
    /// tmux does. Note `-v` and `-q` are **booleans**, so they must stay out of the
    /// optstring or `show -v -q …` would eat `-q` as `-v`'s value.
    fn cmd_show_options(
        &mut self,
        args: &[String],
        window_scope: bool,
    ) -> Result<Vec<String>, String> {
        let mut a = Args::parse(args, "t:")?;
        if window_scope {
            a.flags.insert('w');
        }
        let scope = self.option_scope(&a);
        let Some(name) = a.positionals.first().cloned() else {
            // No name: tmux lists every option in scope. We hold only user options, so
            // that is what comes back — in `name value` form, or bare values under `-v`.
            return Ok(self
                .options
                .iter()
                .filter(|((s, _), _)| *s == scope)
                .map(|((_, n), v)| {
                    if a.flag('v') {
                        v.clone()
                    } else {
                        format!("{n} {}", quote_option_value(v))
                    }
                })
                .collect());
        };
        match self.options.get(&(scope, name.clone())) {
            Some(v) if a.flag('v') => Ok(vec![v.clone()]),
            Some(v) => Ok(vec![format!("{name} {}", quote_option_value(v))]),
            None if a.flag('q') || a.flag('v') => Ok(Vec::new()),
            None => Err(format!("unknown option: {name}")),
        }
    }

    fn cmd_has_session(&mut self, args: &[String]) -> Result<Vec<String>, String> {
        let a = Args::parse(args, "t:")?;
        match a.value('t') {
            None => Ok(Vec::new()),
            Some(t) if t.trim_start_matches('$').parse::<u32>() == Ok(self.session_id) => {
                Ok(Vec::new())
            }
            Some(t) if self.resolve_target(t).is_some() => Ok(Vec::new()),
            Some(t) => Err(format!("can't find session: {t}")),
        }
    }

    // -- format expansion --------------------------------------------------

    /// Expand a tmux `-F` template.
    ///
    /// Supports literal text, `##` → `#`, `#{var}`, and the conditional
    /// `#{?cond,then,else}` (iTerm2's `list-windows` format contains
    /// `#{?window_active,1,0}`, so the conditional is not optional). Prefixed forms
    /// (`#{E:…}`, `#{T:…}`) and any variable we do not model expand to the empty string —
    /// which is what tmux does with an unknown variable, and is why an unrecognised probe
    /// degrades quietly instead of breaking the attach.
    fn expand(&self, template: &str, ctx: &FmtCtx) -> String {
        let b = template.as_bytes();
        let mut out = String::with_capacity(template.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'#' {
                // Copy the whole run up to the next '#' as a &str slice. Copying byte by
                // byte via `as char` would re-encode every byte >= 0x80 as Latin-1 and
                // mangle any non-ASCII in a template or a cwd.
                let start = i;
                while i < b.len() && b[i] != b'#' {
                    i += 1;
                }
                out.push_str(&template[start..i]);
                continue;
            }
            if i + 1 < b.len() && b[i + 1] == b'#' {
                out.push('#');
                i += 2;
                continue;
            }
            if i + 1 < b.len() && b[i + 1] == b'{' {
                match matching_brace(b, i + 1) {
                    Some(end) => {
                        let inner = &template[i + 2..end];
                        out.push_str(&self.expand_var(inner, ctx));
                        i = end + 1;
                        continue;
                    }
                    None => {
                        // Unbalanced: emit the rest literally rather than looping forever.
                        out.push_str(&template[i..]);
                        break;
                    }
                }
            }
            out.push('#');
            i += 1;
        }
        out
    }

    fn expand_var(&self, inner: &str, ctx: &FmtCtx) -> String {
        if let Some(cond) = inner.strip_prefix('?') {
            let parts = split_top_level_commas(cond);
            if parts.len() >= 2 {
                let truthy = {
                    let v = self.expand_var(&parts[0], ctx);
                    !v.is_empty() && v != "0"
                };
                let branch = if truthy {
                    &parts[1]
                } else {
                    parts.get(2).map_or("", |s| s.as_str())
                };
                return self.expand(branch, ctx);
            }
            return String::new();
        }
        // `#{E:x}` / `#{T:x}` / `#{q:x}` and friends: strip the modifier and expand the rest.
        if let Some((prefix, rest)) = inner.split_once(':') {
            if prefix.len() <= 2 && !prefix.contains(' ') {
                return self.expand(rest, ctx);
            }
        }
        self.variable(inner.trim(), ctx)
    }

    /// One `#{...}` variable. Unknown → empty (tmux's behaviour).
    fn variable(&self, name: &str, ctx: &FmtCtx) -> String {
        let pane = ctx.uid.as_deref().and_then(|u| self.panes.get(u));
        let uid = ctx.uid.as_deref();
        match name {
            // -- server / client ------------------------------------------------
            "version" => CLAIMED_VERSION.to_string(),
            "pid" => std::process::id().to_string(),
            "socket_path" => String::new(),
            "client_name" => self.client_name.clone(),
            "client_width" => self.client_size.0.to_string(),
            "client_height" => self.client_size.1.to_string(),
            "client_control_mode" => "1".to_string(),
            // -- session --------------------------------------------------------
            "session_id" => format!("${}", self.session_id),
            "session_name" => self.session_name.clone(),
            "session_windows" => self.panes.len().to_string(),
            "session_attached" => "1".to_string(),
            // -- window ---------------------------------------------------------
            "window_id" => uid
                .and_then(|u| self.ids.window_id(u))
                .map(|w| format!("@{w}"))
                .unwrap_or_default(),
            "window_index" => uid
                .map(|u| self.window_index(u).to_string())
                .unwrap_or_default(),
            "window_name" => pane.map(PaneInfo::name).unwrap_or_default(),
            "window_width" => pane.map(|p| p.width().to_string()).unwrap_or_default(),
            "window_height" => pane.map(|p| p.height().to_string()).unwrap_or_default(),
            "window_panes" => "1".to_string(),
            "window_layout" | "window_visible_layout" => {
                uid.map(|u| self.layout_for(u)).unwrap_or_default()
            }
            "window_flags" | "window_raw_flags" => {
                uid.map(|u| self.window_flags(u)).unwrap_or_default()
            }
            "window_active" => bool_fmt(uid.is_some() && self.active.as_deref() == uid),
            "window_zoomed_flag" => "0".to_string(),
            // -- pane -----------------------------------------------------------
            "pane_id" => uid
                .and_then(|u| self.ids.pane_id(u))
                .map(|p| format!("%{p}"))
                .unwrap_or_default(),
            "pane_index" => "0".to_string(),
            "pane_width" => pane.map(|p| p.width().to_string()).unwrap_or_default(),
            "pane_height" => pane.map(|p| p.height().to_string()).unwrap_or_default(),
            "pane_active" => bool_fmt(uid.is_some() && self.active.as_deref() == uid),
            "pane_current_path" => pane.and_then(|p| p.cwd.clone()).unwrap_or_default(),
            "pane_title" => pane.map(PaneInfo::name).unwrap_or_default(),
            "pane_dead" | "pane_in_mode" | "pane_synchronized" | "pane_pipe" => "0".to_string(),
            // `TmuxStateParser`'s per-pane VT state. We report a plausible reset state
            // rather than nothing: iTerm2 parses these into its own emulator, and an empty
            // string parses as 0 anyway. See the "unverified" note in the docs.
            "cursor_x"
            | "cursor_y"
            | "scroll_region_upper"
            | "alternate_on"
            | "alternate_saved_x"
            | "alternate_saved_y"
            | "insert_flag"
            | "keypad_cursor_flag"
            | "keypad_flag"
            | "mouse_standard_flag"
            | "mouse_button_flag"
            | "mouse_any_flag"
            | "mouse_utf8_flag"
            | "mouse_sgr_flag"
            | "bracket_paste_flag"
            | "pane_key_mode" => "0".to_string(),
            "cursor_flag" | "wrap_flag" => "1".to_string(),
            "scroll_region_lower" => pane
                .map(|p| p.height().saturating_sub(1).to_string())
                .unwrap_or_default(),
            "pane_tabs" => String::new(),
            "history_size" | "history_bytes" => "0".to_string(),
            "history_limit" => "2000".to_string(),
            _ => String::new(),
        }
    }
}

/// How `show-options` (without `-v`) prints a value.
///
/// tmux quotes when the value is empty or holds anything the lexer would re-split
/// (`options.c:options_to_string` → `args_escape`). Quoting only when it is needed keeps
/// the common `@iterm2_id 1234` line byte-identical to real tmux.
fn quote_option_value(v: &str) -> String {
    let needs = v.is_empty()
        || v.chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '$' | ';' | '#'));
    if !needs {
        return v.to_string();
    }
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for c in v.chars() {
        if matches!(c, '"' | '\\' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// tmux renders boolean formats as `1`/`0`.
fn bool_fmt(b: bool) -> String {
    if b { "1" } else { "0" }.to_string()
}

/// The pane a format template is being expanded against.
struct FmtCtx {
    uid: Option<String>,
}

/// Index of the `}` matching the `{` at `open`, honouring nesting.
fn matching_brace(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, &c) in b.iter().enumerate().skip(open) {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split `#{?a,b,c}`'s body on commas that are not inside a nested `#{...}`.
fn split_top_level_commas(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(s[start..].to_string());
    parts
}

// ---------------------------------------------------------------------------
// Command-line lexing
// ---------------------------------------------------------------------------

/// Split a tmux command line into words.
///
/// tmux's quoting: single quotes are **fully literal** (no `$`, `#` or `\` expansion —
/// which is exactly why iTerm2 single-quotes key names, see `TmuxGateway.m
/// -sendKeyName:toWindowPane:`), double quotes allow backslash escapes, and a backslash
/// outside quotes escapes the next character.
///
/// tmux's `;` command separator is **not** implemented: no client we target sends compound
/// lines (iTerm2's `sendCommandList:` writes one command per line), and quietly running only
/// the first half of a compound command would be worse than not seeing one at all.
pub fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if has {
                    words.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            '\'' => {
                has = true;
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '\'' {
                        closed = true;
                        break;
                    }
                    cur.push(c);
                }
                if !closed {
                    return Err("unterminated single quote".to_string());
                }
            }
            '"' => {
                has = true;
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' => match chars.next() {
                            Some('n') => cur.push('\n'),
                            Some('r') => cur.push('\r'),
                            Some('t') => cur.push('\t'),
                            Some('e') => cur.push('\x1b'),
                            Some(o) => cur.push(o),
                            None => return Err("trailing backslash".to_string()),
                        },
                        other => cur.push(other),
                    }
                }
                if !closed {
                    return Err("unterminated double quote".to_string());
                }
            }
            '\\' => {
                has = true;
                match chars.next() {
                    Some(o) => cur.push(o),
                    None => return Err("trailing backslash".to_string()),
                }
            }
            other => {
                has = true;
                cur.push(other);
            }
        }
    }
    if has {
        words.push(cur);
    }
    Ok(words)
}

/// getopt-style flags for one command.
///
/// `optstring` names the flags that take a **value** (`"F:t:"` → `-F` and `-t` do). Every
/// other `-x` is a boolean. Clustering works the way tmux's own parser does, which matters:
/// iTerm2 sends `send -lt %0 abc`, where `-lt` is the boolean `-l` followed by the
/// value-taking `-t`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    flags: HashSet<char>,
    values: Vec<(char, String)>,
    positionals: Vec<String>,
}

impl Args {
    fn parse(words: &[String], optstring: &str) -> Result<Self, String> {
        let takes_value: HashSet<char> = {
            let cs: Vec<char> = optstring.chars().collect();
            cs.iter()
                .enumerate()
                .filter(|(i, _)| cs.get(i + 1) == Some(&':'))
                .map(|(_, c)| *c)
                .collect()
        };
        let mut out = Args::default();
        let mut it = words.iter().peekable();
        while let Some(w) = it.next() {
            // A bare "-" or anything not starting with '-' is positional. So is a negative
            // number, which `capture-pane -S -100` relies on.
            let is_flag = w.len() > 1
                && w.starts_with('-')
                && !w[1..].starts_with(|c: char| c.is_ascii_digit());
            if !is_flag {
                out.positionals.push(w.clone());
                continue;
            }
            let cluster: Vec<char> = w[1..].chars().collect();
            let mut i = 0;
            while i < cluster.len() {
                let c = cluster[i];
                if takes_value.contains(&c) {
                    let rest: String = cluster[i + 1..].iter().collect();
                    let v = if rest.is_empty() {
                        it.next()
                            .cloned()
                            .ok_or_else(|| format!("-{c} needs an argument"))?
                    } else {
                        rest
                    };
                    out.values.push((c, v));
                    break;
                }
                out.flags.insert(c);
                i += 1;
            }
        }
        Ok(out)
    }

    fn flag(&self, c: char) -> bool {
        self.flags.contains(&c)
    }

    fn value(&self, c: char) -> Option<&str> {
        self.values
            .iter()
            .find(|(k, _)| *k == c)
            .map(|(_, v)| v.as_str())
    }
}

// ---------------------------------------------------------------------------
// Key names
// ---------------------------------------------------------------------------

/// Translate one tmux `send-keys` key name into the bytes a pty expects.
///
/// Covers the names iTerm2 delegates to the server on tmux >= 3.2
/// (`TmuxGateway.m -sendKeyName:toWindowPane:`), plus the `C-x` / `M-x` modifier prefixes.
/// Anything else is sent as literal text, which is what tmux does with a name it does not
/// recognise. The escape sequences are the xterm/`TERM=xterm-256color` ones, matching what
/// the panes' `TERM` advertises.
pub fn key_name_to_bytes(name: &str) -> Result<Vec<u8>, String> {
    if name.is_empty() {
        return Ok(Vec::new());
    }
    // Strip every modifier prefix first and apply them together, so `C-M-a` is
    // ESC + 0x01 rather than whatever a naive recursion happens to produce (recursing
    // through `M-` yields a two-byte inner value, and the Ctrl fold then silently does
    // nothing).
    let mut base = name;
    let (mut ctrl, mut meta) = (false, false);
    loop {
        let lower = base.to_ascii_lowercase();
        if lower.starts_with("c-") {
            ctrl = true;
        } else if lower.starts_with("m-") {
            meta = true;
        } else if lower.starts_with("s-") {
            // Shift is already folded into the literal for printable keys, and we have no
            // distinct sequence for the named ones, so it only strips.
        } else {
            break;
        }
        base = &base[2..];
        if base.is_empty() {
            return Ok(Vec::new());
        }
    }
    if ctrl || meta {
        let mut inner = key_name_to_bytes(base)?;
        if ctrl && inner.len() == 1 {
            let c = inner[0].to_ascii_uppercase();
            // Ctrl-<c> is the ASCII control byte: @ A..Z [ \ ] ^ _ → 0x00..0x1F.
            if (0x3F..=0x5F).contains(&c) {
                inner = vec![c & 0x1F];
            } else if c == b'?' {
                inner = vec![0x7f];
            }
        }
        if meta {
            let mut v = vec![0x1b];
            v.extend_from_slice(&inner);
            return Ok(v);
        }
        return Ok(inner);
    }
    let bytes: &[u8] = match name {
        "Enter" | "C-m" | "CR" => b"\r",
        "Tab" => b"\t",
        "BTab" => b"\x1b[Z",
        "Escape" | "Esc" => b"\x1b",
        "Space" => b" ",
        "BSpace" => b"\x7f",
        "Up" => b"\x1b[A",
        "Down" => b"\x1b[B",
        "Right" => b"\x1b[C",
        "Left" => b"\x1b[D",
        "Home" => b"\x1b[H",
        "End" => b"\x1b[F",
        "PageUp" | "PPage" => b"\x1b[5~",
        "PageDown" | "NPage" | "PgDn" => b"\x1b[6~",
        "Insert" | "IC" => b"\x1b[2~",
        "Delete" | "DC" => b"\x1b[3~",
        "F1" => b"\x1bOP",
        "F2" => b"\x1bOQ",
        "F3" => b"\x1bOR",
        "F4" => b"\x1bOS",
        "F5" => b"\x1b[15~",
        "F6" => b"\x1b[17~",
        "F7" => b"\x1b[18~",
        "F8" => b"\x1b[19~",
        "F9" => b"\x1b[20~",
        "F10" => b"\x1b[21~",
        "F11" => b"\x1b[23~",
        "F12" => b"\x1b[24~",
        // Not a name we know: tmux sends it as literal text.
        other => return Ok(other.as_bytes().to_vec()),
    };
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests;
