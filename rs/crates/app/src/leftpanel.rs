//! The left slide-out panel's data side (mux plan M5) — everything `ui/leftpanel.slint`
//! draws that isn't already in [`crate::state::State`].
//!
//! The panel has four sections, and this module owns the three that need to look outside
//! the window's own state:
//!
//! * **WORKSPACE tree** — pure projection over `State.tabs`, done in [`crate::paneview`];
//!   the only thing needed here is the per-pane liveness ([`liveness`] / [`is_idle`]),
//!   which reads the SAME activity source the idle glow does
//!   (`SessionManager::last_output_at` vs [`crate::glow::now_epoch_ms`]) — there is no
//!   second activity clock in the app, and this module must never introduce one.
//! * **LIBRARY** — the saved workspaces under [`library_dir`]. Cached in a thread-local and
//!   rescanned on the panel's closed→open edge, the same shape `sidebar.rs` uses for its
//!   project scans, so the projection never stats the disk on every tick.
//! * **SETS** — the saved workspace *sets* under [`hyperpanes_core::persistence::paths::sets_dir`]
//!   (mux plan M6): a named group of workspaces opened as one batch. Cached and rescanned
//!   on exactly the same edge as the library, since the two directories are siblings and a
//!   set write drops member files into the library's.
//! * **DETACHED** — live sessions that no window is showing. Computed by subtracting the
//!   claimed uids from `SessionManager::uids()`; see [`detached`] for exactly how complete
//!   that answer is today and where M7 fills in the rest.
//!
//! Nothing here mutates `State`: the projection calls it (Seam #1) and the commands it
//! feeds all land in `command::dispatch` (Seam #2).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use hyperpanes_core::persistence::paths::{self as paths, data_dir};
use hyperpanes_core::session_manager::SessionManager;
use hyperpanes_core::tools::PaneKind;
use hyperpanes_core::workspace::sets;

/// How long after a session's last output its liveness dot stays fully lit before fading
/// to the floor. 30s matches the "is this thing doing something right now?" question the
/// dot answers — long enough that a pane between prompts still reads as live, short enough
/// that an abandoned one goes quiet.
pub const LIVE_WINDOW_MS: u64 = 30_000;

/// The 0..1 liveness of a session whose last output was at `last` (epoch ms), as of
/// `now_ms`. `1.0` = output this instant, falling linearly to `0.0` a [`LIVE_WINDOW_MS`]
/// later; a session that has never produced output (or whose clock reads in the future)
/// is `0.0`. The UI floors the dot's opacity so a quiet pane still shows its color chip.
pub fn liveness(last: Option<u64>, now_ms: u64) -> f32 {
    let Some(last) = last else {
        return 0.0;
    };
    let age = now_ms.saturating_sub(last);
    if age >= LIVE_WINDOW_MS {
        return 0.0;
    }
    1.0 - (age as f32 / LIVE_WINDOW_MS as f32)
}

/// Whether a pane's idle alert has armed: the same gate `paneview::pump` uses to light the
/// glow ring (the feature is on, the pane runs an agent CLI, and it has been output-quiet
/// past the threshold). Reproduced here rather than read off `PaneState::glow` because the
/// pump only advances the glow for the ACTIVE tab's panes — the tree shows every tab, and
/// a background tab's stale `glow.alpha` would lie.
pub fn is_idle(
    shell_title: &str,
    last: Option<u64>,
    now_ms: u64,
    on: bool,
    threshold_ms: u64,
) -> bool {
    on && crate::glow::is_ai_pane(shell_title)
        && match last {
            Some(ms) => now_ms.saturating_sub(ms) >= threshold_ms,
            None => false,
        }
}

/// The mark kinds the workspace tree draws for a pane that is **not** a tool this build
/// knows. The pane header is free to leave those blank — it is chrome sitting on a pane you
/// are already looking at — but a tree is a column, and a column of marks with holes
/// punched in it reads as ragged, so every row here gets one.
///
/// Negative on purpose: the positive half of this namespace belongs to the tool registry
/// (`ToolDef.icon`, allocated from [`crate::theme::menu_icon::TOOL_BASE`]) and is drawn by
/// the shared `ToolIcon`. Keeping the two halves on opposite sides of zero means a tool
/// added to the registry tomorrow can never collide with a view added here today.
pub mod pane_mark {
    /// A plain shell — a `>_` prompt. Also what a tool id this build has no mark for falls
    /// back to: such a pane really is a terminal running something, and an honest prompt
    /// beats a borrowed brand (the same call the header makes when it draws nothing).
    pub const TERMINAL: i32 = -1;
    /// The file-browser view — a folder.
    pub const FILE_BROWSER: i32 = -2;
    /// The file-viewer view — a page.
    pub const FILE_VIEWER: i32 = -3;
    /// The markdown preview — the markdown badge.
    pub const MARKDOWN: i32 = -4;
    /// The internal browser view — a globe.
    pub const BROWSER: i32 = -5;
}

/// The mark one pane row carries, in the namespace `PaneMark` in `ui/leftpanel.slint`
/// switches on: the registry's own icon kind for a tool we know (>= `TOOL_BASE`, drawn by
/// the shared `ToolIcon` — the very component the pane header uses, which is the whole
/// point: a pane must be identifiable in the panel the same way it is on its chrome), and
/// one of the [`pane_mark`] negatives for everything else.
///
/// Takes the pane's EFFECTIVE kind (`State::effective_kind`), not `PaneState::kind`, so a
/// plain terminal that the title sniff caught running an agent is branded in the tree for
/// the same reason — and at the same moment — as it is in its header.
pub fn pane_mark_kind(kind: &PaneKind) -> i32 {
    match kind {
        PaneKind::FileBrowser => pane_mark::FILE_BROWSER,
        PaneKind::FileViewer => pane_mark::FILE_VIEWER,
        PaneKind::Markdown => pane_mark::MARKDOWN,
        PaneKind::Browser => pane_mark::BROWSER,
        // `ui_icon` answers 0 for a plain shell AND for a tool id with no mark in this
        // build; both are PTY panes, so both get the prompt rather than a gap.
        PaneKind::Terminal | PaneKind::Tool(_) => match kind.ui_icon() {
            0 => pane_mark::TERMINAL,
            icon => icon,
        },
    }
}

/// The ink a pane row's mark is drawn in: the tool's own brand when the registry knows it —
/// the identical colour the header tints its mark with, so the two readings of the same
/// pane never disagree — and the pane's accent otherwise, which is the header's own
/// fallback and is never invisible against the panel.
pub fn pane_mark_ink(kind: &PaneKind, accent: slint::Color) -> slint::Color {
    match kind.tool() {
        Some(t) => slint::Color::from_rgb_u8(t.brand.0, t.brand.1, t.brand.2),
        None => accent,
    }
}

/// How often the projection re-runs purely to age the liveness dots while the panel is
/// open. One resync a second is invisible next to the pump's own cadence and keeps a dot
/// from freezing at its last projected brightness on an otherwise-quiet workspace.
const HEARTBEAT: std::time::Duration = std::time::Duration::from_millis(1000);

/// Whether the panel's liveness heartbeat is due at `now` (and, if so, consume it by
/// stamping `last`). Called from the pump only while the panel is open.
///
/// `last` is PER WINDOW (`State::left_panel_beat`), not a module-global: `pump` runs once
/// per window, so a single shared stamp would be consumed by whichever window the app
/// happens to pump first and every other window's dots would freeze at their last
/// projected brightness.
pub fn heartbeat_due(last: &mut Option<std::time::Instant>, now: std::time::Instant) -> bool {
    match *last {
        Some(t) if now.duration_since(t) < HEARTBEAT => false,
        _ => {
            *last = Some(now);
            true
        }
    }
}

// ===================== the saved-workspace library =====================

/// Where the panel's workspace library lives: `<data dir>/workspaces`. Distinct from the
/// "Save workspace…" file dialog (which writes wherever the user points it) — the library
/// is the zero-friction drawer the panel lists, and it is app-owned on purpose so this
/// milestone doesn't reach into `core::persistence`.
pub fn library_dir() -> PathBuf {
    data_dir().join("workspaces")
}

/// One row of the LIBRARY section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryEntry {
    /// The file on disk (what a click reads back).
    pub path: PathBuf,
    /// The display name — the workspace's own `name` if it has one, else the file stem.
    pub name: String,
    /// The second line: pane/tab counts plus how long ago the file was written.
    pub detail: String,
}

thread_local! {
    /// The last scan of [`library_dir`], so the projection can hand the model rows every
    /// tick without touching the filesystem. Refreshed on the panel's closed→open edge and
    /// after the panel itself writes a workspace. Process-wide on purpose: the library is
    /// one directory on disk, so every window shows the same rows.
    static LIB_CACHE: RefCell<Vec<LibraryEntry>> = const { RefCell::new(Vec::new()) };
}

/// Note the panel's current open state; on the closed→open transition rescan the library
/// (files may have been added by another window — or by hand — while it was shut). Called
/// from the projection each tick, exactly like `sidebar::note_flyout_open`.
///
/// `seen` is the caller's PER-WINDOW memory of the last state (`State::left_panel_seen_open`).
/// A module-global flag would be wrong here: `resync` runs once per window, so two windows
/// disagreeing about the panel (one open, one shut) would flip a shared flag every tick and
/// rescan the directory on every single frame — the exact per-tick disk hit the cache exists
/// to avoid.
pub fn note_panel_open(seen: &mut bool, open: bool) {
    if rescan_due(seen, open) {
        refresh_library();
        refresh_sets();
    }
}

/// The edge test behind [`note_panel_open`], split from the disk scan so it can be tested
/// without reaching for the user's real data directory: true exactly on closed→open.
fn rescan_due(seen: &mut bool, open: bool) -> bool {
    let edge = open && !*seen;
    *seen = open;
    edge
}

/// Rescan [`library_dir`] into the cache. Cheap (one `read_dir` over a directory that
/// holds a handful of small files) and only ever called on an edge, never per tick.
pub fn refresh_library() {
    let rows = scan_library(&library_dir());
    LIB_CACHE.with(|c| *c.borrow_mut() = rows);
}

/// The cached library rows, in display order (newest first).
pub fn library() -> Vec<LibraryEntry> {
    LIB_CACHE.with(|c| c.borrow().clone())
}

/// Scan `dir` for `*.hyperpanes` / `*.json` workspaces, newest first. Unreadable or
/// malformed files are skipped rather than shown as broken rows. Split out from
/// [`refresh_library`] so it can be tested against a temp directory.
pub fn scan_library(dir: &Path) -> Vec<LibraryEntry> {
    let now = crate::glow::now_epoch_ms();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    // (modified epoch-ms, entry) so the list can be sorted newest-first.
    let mut rows: Vec<(u64, LibraryEntry)> = Vec::new();
    for ent in rd.flatten() {
        let path = ent.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ext != "hyperpanes" && ext != "json" {
            continue;
        }
        let Some(file) = hyperpanes_core::workspace::io::read_workspace(&path) else {
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("workspace")
            .to_string();
        let name = match &file.name {
            Some(n) if !n.trim().is_empty() => n.trim().to_string(),
            _ => stem,
        };
        let mtime = ent
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        rows.push((
            mtime,
            LibraryEntry {
                name,
                detail: describe_workspace(&file, mtime, now),
                path,
            },
        ));
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    rows.into_iter().map(|(_, e)| e).collect()
}

/// The library row's second line: "3 panes · 2 tabs · 5m ago" (the time part is dropped
/// when the file's mtime is unknown). Reuses the sidebar's relative-time buckets rather
/// than growing a second wording of the same idea.
fn describe_workspace(
    file: &hyperpanes_core::workspace::model::WorkspaceFile,
    mtime: u64,
    now: u64,
) -> String {
    let groups = hyperpanes_core::workspace::io::windows_of(Some(file))
        .into_iter()
        .next()
        .map(|w| w.groups)
        .unwrap_or_default();
    let tabs = groups.len();
    let panes: usize = groups.iter().map(|g| g.panes.len()).sum();
    let mut out = format!(
        "{panes} pane{} · {tabs} tab{}",
        if panes == 1 { "" } else { "s" },
        if tabs == 1 { "" } else { "s" }
    );
    if mtime > 0 {
        let rel = crate::sidebar::relative_time(Some(mtime), now);
        if !rel.is_empty() {
            out.push_str(" · ");
            out.push_str(&rel);
        }
    }
    out
}

/// Write `file` into the library under `name` (sanitised, `.hyperpanes` appended), creating
/// the directory if needed, and refresh the cache. Returns the path written, or `None` if
/// the directory or the file could not be written. A name that collides gets `-2`, `-3`, …
/// appended, so saving twice never silently overwrites the earlier snapshot.
pub fn save_to_library(
    name: &str,
    file: &hyperpanes_core::workspace::model::WorkspaceFile,
) -> Option<PathBuf> {
    let dir = library_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let base = sanitize_name(name);
    let mut path = dir.join(format!("{base}.hyperpanes"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{base}-{n}.hyperpanes"));
        n += 1;
        if n > 999 {
            return None;
        }
    }
    if !hyperpanes_core::workspace::io::write_workspace(&path, file) {
        return None;
    }
    refresh_library();
    Some(path)
}

/// Reduce a tab title to a safe file stem: path separators and the Windows-reserved
/// punctuation become `-`, runs collapse, and an empty result falls back to "workspace".
fn sanitize_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.trim().chars() {
        let ok = ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == ' ' || ch == '-';
        if ok && ch != ' ' && ch != '-' {
            out.push(ch);
            last_dash = false;
        } else {
            if !last_dash && !out.is_empty() {
                out.push('-');
            }
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "workspace".to_string()
    } else {
        out.chars().take(64).collect()
    }
}

// ===================== the saved-workspace sets =====================

/// One row of the SETS section: a saved [`sets::WorkspaceSet`] on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetEntry {
    /// The `sets/*.json` index file (what a click reads back).
    pub path: PathBuf,
    /// The display name — the set's own `name`, falling back to the file stem.
    pub name: String,
    /// The second line: member count plus how long ago the index was written.
    pub detail: String,
}

thread_local! {
    /// The last scan of [`sets::path_for`]'s directory. Same contract as [`LIB_CACHE`]:
    /// process-wide, refreshed only on the panel's open edge and after this process writes
    /// a set, never per tick.
    static SET_CACHE: RefCell<Vec<SetEntry>> = const { RefCell::new(Vec::new()) };
}

/// Rescan the canonical sets directory into the cache.
pub fn refresh_sets() {
    let rows = scan_sets(&paths::sets_dir());
    SET_CACHE.with(|c| *c.borrow_mut() = rows);
}

/// The cached set rows, in display order (newest first).
pub fn sets_rows() -> Vec<SetEntry> {
    SET_CACHE.with(|c| c.borrow().clone())
}

/// Scan `dir` for readable sets, newest first. Split out from [`refresh_sets`] so it can be
/// tested against a temp directory.
///
/// Ordering differs from [`sets::list_sets_in`] on purpose: that returns file-name order (a
/// stable index for programmatic use), while this panel section is a *recency* drawer, like
/// [`scan_library`] beside it — the set you just saved belongs at the top.
pub fn scan_sets(dir: &Path) -> Vec<SetEntry> {
    let now = crate::glow::now_epoch_ms();
    let mut rows: Vec<(u64, SetEntry)> = Vec::new();
    for (path, set) in sets::list_sets_in(dir) {
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let name = if set.name.trim().is_empty() {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "set".to_string())
        } else {
            set.name.clone()
        };
        rows.push((
            mtime,
            SetEntry {
                detail: describe_set(set.members.len(), mtime, now),
                name,
                path,
            },
        ));
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    rows.into_iter().map(|(_, e)| e).collect()
}

/// The set row's second line: "4 workspaces · 5m ago". Counts the set's OWN member list
/// rather than reading each member file — a set is an index of references, and a stale
/// reference should not change the count the user saved.
fn describe_set(members: usize, mtime: u64, now: u64) -> String {
    let mut out = format!("{members} workspace{}", if members == 1 { "" } else { "s" });
    if mtime > 0 {
        let rel = crate::sidebar::relative_time(Some(mtime), now);
        if !rel.is_empty() {
            out.push_str(" · ");
            out.push_str(&rel);
        }
    }
    out
}

// ===================== detached (adoptable) sessions =====================

/// One row of the DETACHED section: a live session no window is currently showing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetachedSession {
    pub uid: String,
    /// The row's title — the session's short uid (there is no server-side label yet).
    pub label: String,
    /// The second line: how much output it has buffered and when it last spoke.
    pub detail: String,
    /// Epoch-ms of its last output, for the liveness dot.
    pub last_output_at: Option<u64>,
}

thread_local! {
    /// Session uids claimed by the windows of THIS process — the union over every live
    /// window, republished by `app.rs` once per pump and subtracted in [`detached`], so a
    /// pane sitting in the window next door is never offered for adoption. A union (rather
    /// than a per-window "everyone else" set) is enough: a window also subtracts its own
    /// uids, and a session held anywhere is not detached from anyone's point of view.
    static WINDOW_CLAIMS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());

    /// What this process last told the daemon it is hosting. Diffed against each new
    /// publish so the pump sends a `Claim`/`Release` frame only when the set actually
    /// changes — an unchanged frame costs nothing on the wire.
    static PUBLISHED_CLAIMS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Publish the uids every window in this process is hosting (laid out or parked). Called
/// from the app's pump, before the per-window renders that project the panel.
///
/// **M7:** this also registers those uids with the daemon's cross-process claim registry, so
/// that *other* hyperpanes processes stop offering them for adoption. Only the difference
/// from the previous publish goes on the wire, and it goes fire-and-forget: a claim on a
/// pane we already host is not contested, and the GUI pump must never block on the daemon.
/// The contested case — adopting an orphan — takes the blocking path in
/// [`SessionManager::claim_session`] and obeys its answer.
///
/// Releasing on the way out is a courtesy, not the safety net: the daemon drops every claim
/// a connection holds when that connection's socket closes, so a crash releases them too.
pub fn publish_window_claims(mgr: &SessionManager, uids: impl IntoIterator<Item = String>) {
    let held: HashSet<String> = uids.into_iter().collect();
    PUBLISHED_CLAIMS.with(|prev| {
        let mut prev = prev.borrow_mut();
        for uid in held.difference(&prev) {
            mgr.announce_claim(uid);
        }
        for uid in prev.difference(&held) {
            mgr.release_session(uid);
        }
        *prev = held.clone();
    });
    set_window_claims(held);
}

/// The thread-local half of [`publish_window_claims`]: record what this process's windows
/// hold, with no daemon traffic. Split out because the daemon half needs a live
/// `SessionManager` (and therefore a real pty) while the subtraction it feeds does not.
fn set_window_claims(held: HashSet<String>) {
    WINDOW_CLAIMS.with(|c| *c.borrow_mut() = held);
}

/// Uids claimed by a hyperpanes process *other than this one* — a session another window,
/// in another process, is currently hosting.
///
/// Answered from the claim snapshot the daemon pushes to every client whenever the picture
/// changes (M7), so this is a lock and a set filter: no I/O on the panel's paint path. Empty
/// for the in-process backend, where no other process can be holding one of our sessions.
///
/// A claim is scoped to the owner's daemon *connection*, so a process that dies — cleanly,
/// by panic, or by `SIGKILL` — has its claims dropped the moment the kernel closes its
/// socket, and its panes appear here no longer.
pub fn claimed_by_other_processes(mgr: &SessionManager) -> HashSet<String> {
    mgr.sessions_claimed_elsewhere()
}

/// The adoptable sessions: everything the session manager knows about, minus everything
/// already claimed.
///
/// `claimed_here` is this window's own uids (its panes + its parked reminders). The other
/// two subtractions come from [`publish_window_claims`] (every window in this process) and
/// [`claimed_by_other_processes`] (every OTHER process, via the daemon's claim registry).
///
/// How complete this is: in daemon mode `SessionManager::uids()` answers from the client's
/// shadow table, which is seeded by `ListSessions` at connect and then kept current by the
/// `Exit` stream, this client's own creates, and (M7) the full `SessionsChanged` snapshot the
/// daemon pushes on every create/kill/exit — so a session another client made after we
/// connected shows up here without a reconnect, and a session it killed stops showing up.
/// In-process mode lists exactly the sessions this process spawned, which is the correct
/// answer for a single-process run.
pub fn detached(mgr: &SessionManager, claimed_here: &HashSet<String>) -> Vec<DetachedSession> {
    let now = crate::glow::now_epoch_ms();
    let other_procs = claimed_by_other_processes(mgr);
    let mut rows: Vec<DetachedSession> = adoptable_uids(mgr.uids(), claimed_here, &other_procs)
        .into_iter()
        .map(|uid| {
            let last = mgr.last_output_at(&uid);
            DetachedSession {
                label: short_uid(&uid),
                detail: describe_session(mgr.output_bytes(&uid).unwrap_or(0), last, now),
                last_output_at: last,
                uid,
            }
        })
        .collect();
    // Most recently active first — the one you're most likely to be looking for.
    rows.sort_by_key(|r| std::cmp::Reverse(r.last_output_at));
    rows
}

/// The subtraction behind [`detached`], split out so it can be tested without a live
/// `SessionManager` (which can only be populated by actually spawning a PTY): `all` minus
/// this window's own claims, minus every other window in this process
/// ([`publish_window_claims`]), minus every other process (`other_procs`, which [`detached`]
/// fills from [`claimed_by_other_processes`] — passed in rather than fetched here, since
/// M7's source for it needs a live `SessionManager`). Input order is preserved; [`detached`]
/// re-sorts by last output.
pub fn adoptable_uids(
    all: Vec<String>,
    claimed_here: &HashSet<String>,
    other_procs: &HashSet<String>,
) -> Vec<String> {
    let elsewhere = WINDOW_CLAIMS.with(|c| c.borrow().clone());
    all.into_iter()
        .filter(|uid| {
            !claimed_here.contains(uid) && !elsewhere.contains(uid) && !other_procs.contains(uid)
        })
        .collect()
}

/// A session uid shortened for display (uids are long and opaque; the head is enough to
/// tell two apart). Mirrors `sidebar::short_id`'s approach.
fn short_uid(uid: &str) -> String {
    let head: String = uid.chars().take(12).collect();
    format!("session {head}")
}

/// The detached row's second line: buffered output size + relative last-output time.
fn describe_session(bytes: u64, last: Option<u64>, now: u64) -> String {
    let size = if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} B")
    };
    let rel = crate::sidebar::relative_time(last, now);
    if rel.is_empty() {
        format!("{size} buffered")
    } else {
        format!("{size} buffered · {rel}")
    }
}

// ===== the tool modes: one tool's resumable sessions, across every project =====
//
// The panel's non-workspace modes (D9). Unlike the sidebar's per-project Claude list, this
// scans the tool's WHOLE store, so it is far too expensive for the UI thread — it goes
// through `history_scan`'s thread like the other two enumerations, and this module only
// caches and shapes what comes back.
//
// The resumability verdict is decided on that thread, once per scan, and travels with the
// row. The alternative — asking the provider on every frame — would re-run binary detection
// and a `stat` per row per tick, and the answer would still be the same one.

/// One row of a tool mode, already answered: what it is, and whether a click can act on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedSession {
    /// The tool's own resume key (a Claude session uuid) — what the UI sends back on click.
    pub id: String,
    /// The project heading this row sits under (the full path; the view shortens it).
    pub project: PathBuf,
    /// One-line summary of the conversation.
    pub summary: String,
    /// The branch the conversation was on, its message count, its age — the detail line.
    /// Replaced by the blocked reason when there is one, so a row always says something true.
    pub detail: String,
    /// `Some(shell line)` when a click can resume it; `None` when the row is blocked.
    pub command: Option<String>,
    /// The cwd that shell line must run in. Empty when blocked.
    pub cwd: PathBuf,
    /// `Some(local_… id)` when Claude Desktop also holds this conversation — the id its deep
    /// link speaks. Resolved on the scan thread with the rest of the row, because it comes
    /// from a second store on disk and the UI thread must not go looking for it per frame.
    pub desktop: Option<String>,
}

impl ScannedSession {
    /// Whether a click on this row does anything.
    pub fn resumable(&self) -> bool {
        self.command.is_some()
    }
}

thread_local! {
    /// The last completed scan per tool id, plus when it landed (epoch ms) so
    /// [`tool_sessions`] can age it out. Process-wide for the same reason [`LIB_CACHE`] is:
    /// a tool's history is one store on disk and every window sees the same rows.
    static TOOL_CACHE: RefCell<HashMap<String, (u64, Vec<ScannedSession>)>> =
        RefCell::new(HashMap::new());
}

/// How long a completed scan is served before the projection asks for a fresh one. A warm
/// re-scan re-parses only changed transcripts (~8ms on a 132-session store), so this is
/// about not spamming the scan thread, not about the cost of the scan itself. Long enough
/// that switching modes back and forth is free; short enough that a conversation you just
/// had shows up without restarting the app.
pub const TOOL_SCAN_TTL_MS: u64 = 20_000;

/// Run one provider's scan and shape every row, deciding resumability as it goes. Called on
/// the scan thread (`history_scan`), never on the UI thread — it walks a whole transcript
/// store and stats every project directory. The provider carries the human's binary
/// overrides (it was built with them), so the verdict here honours them.
pub fn scan_with(
    provider: &mut dyn hyperpanes_core::tools::history::SessionProvider,
) -> Vec<ScannedSession> {
    use hyperpanes_core::tools::history::ResumePlan;
    let now = crate::glow::now_epoch_ms();
    let sessions = provider.scan();
    // One pass over Claude Desktop's store for the whole scan, not one per row — and only
    // for the tool that has a desktop app at all. Every other tool's rows get `None`.
    let desktop = if provider.id() == "claude" {
        hyperpanes_core::tools::claude_desktop::scan()
    } else {
        HashMap::new()
    };
    sessions
        .iter()
        .map(|s| {
            let (detail, command, cwd) = match provider.resume(s) {
                ResumePlan::Ready(cmd) => (session_detail(s, now), Some(cmd.shell_line()), cmd.cwd),
                // The reason REPLACES the detail line rather than being appended to it: a
                // row that cannot be resumed has one thing worth saying, and the panel is
                // narrow enough that a second clause would elide it away.
                ResumePlan::Blocked(b) => (b.reason(), None, PathBuf::new()),
            };
            ScannedSession {
                desktop: desktop.get(&s.id).cloned(),
                id: s.id.clone(),
                project: s.project.clone(),
                summary: session_label(s),
                detail,
                command,
                cwd,
            }
        })
        .collect()
}

/// The row's first line: the transcript's summary, its first user message, or — when a
/// transcript carries neither — the head of its id, so a row is never blank.
fn session_label(s: &hyperpanes_core::tools::history::ToolSession) -> String {
    for candidate in [s.summary.trim(), s.first_user.trim()] {
        if !candidate.is_empty() {
            return candidate.chars().take(120).collect();
        }
    }
    format!("session {}", s.id.chars().take(8).collect::<String>())
}

/// The row's second line for a resumable session: branch · messages · age. Each part is
/// dropped when unknown rather than shown empty.
fn session_detail(s: &hyperpanes_core::tools::history::ToolSession, now: u64) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(b) = s.branch.as_deref().filter(|b| !b.is_empty()) {
        parts.push(b.to_string());
    }
    if s.message_count > 0 {
        parts.push(format!(
            "{} message{}",
            s.message_count,
            if s.message_count == 1 { "" } else { "s" }
        ));
    }
    let rel = crate::sidebar::relative_time(s.started_at, now);
    if !rel.is_empty() {
        parts.push(rel);
    }
    parts.join(" · ")
}

// ===== where a conversation is already running =====
//
// A resumable row's click means "take me to this conversation", and the answer depends on
// where it already is. Two of the three places are found here.
//
// The pane answer has to be process-wide rather than per-window, because the projection that
// draws the badge (`paneview::resync`) is handed exactly ONE window's `State` and a session
// open in another window's pane is just as open. Each window publishes its own set on every
// pump and the lookup is their union; a window that stops publishing (it closed) is dropped
// by [`forget_window`] rather than left to claim panes that no longer exist.

thread_local! {
    /// Per window id, the tool conversations that window has a pane in. Written by
    /// [`publish_open_sessions`] once per pump — cheap, and it means the badge can never
    /// outlive the pane by more than a frame.
    static OPEN_SESSIONS: RefCell<HashMap<usize, HashSet<String>>> = RefCell::new(HashMap::new());
}

/// Record which conversations window `window_id` is showing, replacing its last answer.
pub fn publish_open_sessions(window_id: usize, ids: HashSet<String>) {
    OPEN_SESSIONS.with(|c| {
        c.borrow_mut().insert(window_id, ids);
    });
}

/// Drop a closed window's claims. Without this its sessions would read as open forever and
/// a click would look for a pane in a window that is gone.
pub fn forget_window(window_id: usize) {
    OPEN_SESSIONS.with(|c| {
        c.borrow_mut().remove(&window_id);
    });
}

/// Whether ANY window currently has a pane in conversation `id`.
pub fn session_open_in_a_pane(id: &str) -> bool {
    OPEN_SESSIONS.with(|c| c.borrow().values().any(|ids| ids.contains(id)))
}

/// Store a finished tool scan (called from `history_scan::drain`).
pub fn apply_tool_sessions(tool_id: &str, rows: Vec<ScannedSession>) {
    let now = crate::glow::now_epoch_ms();
    TOOL_CACHE.with(|c| {
        c.borrow_mut().insert(tool_id.to_string(), (now, rows));
    });
}

/// The cached rows for `tool_id`, requesting a background (re-)scan when there are none yet
/// or the last one has aged past [`TOOL_SCAN_TTL_MS`]. Never touches the disk itself: a miss
/// returns empty and the rows appear a tick after the scan lands.
pub fn tool_sessions(
    tool_id: &str,
    overrides: &std::collections::BTreeMap<String, String>,
    now_ms: u64,
) -> Vec<ScannedSession> {
    let (stale, rows) = TOOL_CACHE.with(|c| match c.borrow().get(tool_id) {
        Some((at, rows)) => (now_ms.saturating_sub(*at) > TOOL_SCAN_TTL_MS, rows.clone()),
        None => (true, Vec::new()),
    });
    if stale {
        crate::history_scan::request_tool_sessions(tool_id, overrides.clone());
    }
    rows
}

/// One cached row by (tool, resume id) — what the resume click looks up. By id rather than
/// by index so a click can never act on the wrong row after a re-scan reorders the list.
pub fn tool_session(tool_id: &str, id: &str) -> Option<ScannedSession> {
    TOOL_CACHE.with(|c| {
        c.borrow()
            .get(tool_id)
            .and_then(|(_, rows)| rows.iter().find(|r| r.id == id).cloned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperpanes_core::workspace::model::{GroupSpec, PaneSpec, WindowSpec, WorkspaceFile};

    #[test]
    fn liveness_decays_over_the_window() {
        let now = 1_000_000u64;
        assert_eq!(liveness(None, now), 0.0);
        assert_eq!(liveness(Some(now), now), 1.0);
        // half way through the window → about half lit
        let half = liveness(Some(now - LIVE_WINDOW_MS / 2), now);
        assert!((half - 0.5).abs() < 0.01, "half = {half}");
        assert_eq!(liveness(Some(now - LIVE_WINDOW_MS), now), 0.0);
        assert_eq!(liveness(Some(now - LIVE_WINDOW_MS * 10), now), 0.0);
        // a timestamp in the future (clock skew) must not exceed 1.0
        assert_eq!(liveness(Some(now + 5_000), now), 1.0);
    }

    #[test]
    fn a_pane_row_carries_the_mark_of_the_tool_it_runs() {
        // A tool the registry knows resolves to ITS icon kind — the registry's own number,
        // not one this module invents — so the tree hands `ToolIcon` exactly what the pane
        // header hands it and the two draw the same mark.
        for t in hyperpanes_core::tools::registry::TOOLS {
            let kind = PaneKind::Tool(t.id.to_string());
            assert_eq!(
                pane_mark_kind(&kind),
                t.icon as i32,
                "{} must carry the registry's own mark",
                t.id
            );
        }
    }

    #[test]
    fn every_pane_row_gets_a_mark_even_when_it_runs_no_tool() {
        // The whole point of the negatives: a column with a hole in it reads as ragged, so
        // no kind may ever come back as "draw nothing" (0, which is what the header's
        // `ui_icon` answers for all of these).
        let views = [
            (PaneKind::Terminal, pane_mark::TERMINAL),
            (PaneKind::FileBrowser, pane_mark::FILE_BROWSER),
            (PaneKind::FileViewer, pane_mark::FILE_VIEWER),
            (PaneKind::Markdown, pane_mark::MARKDOWN),
            (PaneKind::Browser, pane_mark::BROWSER),
            // A tool id from a build newer than this one: still a terminal running
            // something, so it gets the prompt rather than a gap or a borrowed brand.
            (
                PaneKind::Tool("tool-from-the-future".into()),
                pane_mark::TERMINAL,
            ),
        ];
        for (kind, want) in views {
            let got = pane_mark_kind(&kind);
            assert_eq!(got, want, "{kind:?}");
            assert_ne!(got, 0, "{kind:?} must never draw nothing");
        }
    }

    #[test]
    fn pane_marks_never_collide_with_the_registrys_icon_kinds() {
        // The two halves of the namespace are split at zero. If a view mark ever went
        // positive it would draw whichever tool happened to own that number.
        for m in [
            pane_mark::TERMINAL,
            pane_mark::FILE_BROWSER,
            pane_mark::FILE_VIEWER,
            pane_mark::MARKDOWN,
            pane_mark::BROWSER,
        ] {
            assert!(m < 0, "view mark {m} is inside the registry's half");
        }
        // …and every registry kind stays in the other half, which is what lets
        // `PaneMark` dispatch on the sign alone.
        for t in hyperpanes_core::tools::registry::TOOLS {
            assert!(t.icon as i32 > 0);
        }
    }

    #[test]
    fn a_marks_ink_is_the_tools_brand_and_the_panes_accent_otherwise() {
        let accent = slint::Color::from_rgb_u8(1, 2, 3);
        let claude = hyperpanes_core::tools::registry::by_id("claude").unwrap();
        assert_eq!(
            pane_mark_ink(&PaneKind::Tool("claude".into()), accent),
            slint::Color::from_rgb_u8(claude.brand.0, claude.brand.1, claude.brand.2)
        );
        // No registry entry, so no brand: the pane's own accent, which is what the header
        // falls back to and is never invisible against the panel.
        for kind in [
            PaneKind::Terminal,
            PaneKind::FileBrowser,
            PaneKind::Markdown,
            PaneKind::Tool("tool-from-the-future".into()),
        ] {
            assert_eq!(pane_mark_ink(&kind, accent), accent, "{kind:?}");
        }
    }

    #[test]
    fn idle_gate_matches_the_glow_gate() {
        let now = 1_000_000u64;
        let thr = 30_000u64;
        // off → never idle, whatever the pane is
        assert!(!is_idle("claude", Some(now - 60_000), now, false, thr));
        // a plain shell never arms, however quiet
        assert!(!is_idle("zsh", Some(now - 60_000), now, true, thr));
        // an agent pane quiet past the threshold arms
        assert!(is_idle("claude", Some(now - 60_000), now, true, thr));
        // …but not before it
        assert!(!is_idle("claude", Some(now - 1_000), now, true, thr));
        // no output at all is not "idle" (the pane never started)
        assert!(!is_idle("claude", None, now, true, thr));
    }

    #[test]
    fn heartbeat_is_per_window_and_rate_limited() {
        let t0 = std::time::Instant::now();
        // A window that has never beaten fires immediately, then not again inside the window.
        let mut a: Option<std::time::Instant> = None;
        assert!(heartbeat_due(&mut a, t0));
        assert!(!heartbeat_due(&mut a, t0 + HEARTBEAT / 2));
        assert!(heartbeat_due(&mut a, t0 + HEARTBEAT));
        // A SECOND window keeps its own stamp: the first window consuming the beat must not
        // starve it (the bug a module-global stamp had — window 2's dots froze forever).
        let mut b: Option<std::time::Instant> = None;
        assert!(heartbeat_due(&mut b, t0 + HEARTBEAT));
        assert!(!heartbeat_due(&mut b, t0 + HEARTBEAT));
    }

    #[test]
    fn library_rescans_only_on_the_closed_to_open_edge() {
        let mut seen = false;
        assert!(rescan_due(&mut seen, true), "closed → open rescans");
        assert!(!rescan_due(&mut seen, true), "still open → no rescan");
        assert!(!rescan_due(&mut seen, false), "open → closed → no rescan");
        assert!(rescan_due(&mut seen, true), "and again on the next edge");
        // Per-window memory: another window's panel state can't cancel this one's edge.
        let mut other = false;
        assert!(rescan_due(&mut other, true));
        assert!(
            !rescan_due(&mut seen, true),
            "unaffected by the other window"
        );
    }

    #[test]
    fn adoptable_subtracts_this_window_and_the_others() {
        let all = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<HashSet<_>>();

        let none = HashSet::new();

        set_window_claims(HashSet::new());
        // Nothing claimed → everything is adoptable, in the order given.
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&[]), &none),
            all(&["a", "b", "c"])
        );
        // This window's own panes are never offered back to it.
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&["b"]), &none),
            all(&["a", "c"])
        );

        // A pane sitting in the window next door is not detached either.
        set_window_claims(set(&["c"]));
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&["b"]), &none),
            all(&["a"])
        );

        // Republishing replaces (never accumulates) the cross-window claim set.
        set_window_claims(HashSet::new());
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&["b"]), &none),
            all(&["a", "c"])
        );

        // And the M7 subtraction: a uid another PROCESS has claimed in the daemon's registry
        // is not offered here either, even though nothing in this process holds it.
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&["b"]), &set(&["c"])),
            all(&["a"])
        );
    }

    #[test]
    fn sanitize_name_makes_a_safe_stem() {
        assert_eq!(sanitize_name("my project"), "my-project");
        assert_eq!(sanitize_name("a/b\\c"), "a-b-c");
        assert_eq!(sanitize_name("  spaced  out  "), "spaced-out");
        assert_eq!(sanitize_name(""), "workspace");
        assert_eq!(sanitize_name("///"), "workspace");
        assert_eq!(sanitize_name("keep_this.1"), "keep_this.1");
        assert!(sanitize_name(&"x".repeat(200)).chars().count() <= 64);
    }

    fn wf(name: Option<&str>, groups: Vec<usize>) -> WorkspaceFile {
        WorkspaceFile {
            name: name.map(|s| s.to_string()),
            windows: Some(vec![WindowSpec {
                groups: groups
                    .into_iter()
                    .map(|n| GroupSpec {
                        panes: (0..n).map(|_| PaneSpec::default()).collect(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn describe_workspace_counts_panes_and_tabs() {
        let now = 1_000_000u64;
        let d = describe_workspace(&wf(Some("api"), vec![2, 1]), now - 60_000, now);
        assert!(d.starts_with("3 panes · 2 tabs"), "{d}");
        assert!(d.ends_with("1m ago"), "{d}");
        // singulars, and no time part when the mtime is unknown
        let d1 = describe_workspace(&wf(None, vec![1]), 0, now);
        assert_eq!(d1, "1 pane · 1 tab");
    }

    #[test]
    fn describe_session_formats_size_and_age() {
        let now = 1_000_000u64;
        assert_eq!(describe_session(512, None, now), "512 B buffered");
        assert_eq!(describe_session(2048, None, now), "2 KB buffered");
        assert_eq!(
            describe_session(3 * 1_048_576, Some(now - 120_000), now),
            "3.0 MB buffered · 2m ago"
        );
    }

    #[test]
    fn short_uid_is_stable_and_short() {
        assert_eq!(short_uid("abcdefghijklmnopqrst"), "session abcdefghijkl");
        assert_eq!(short_uid("abc"), "session abc");
    }

    #[test]
    fn library_scan_reads_workspaces_newest_first() {
        let dir = std::env::temp_dir().join(format!("hp-lib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // a valid workspace, a valid one with no name, a non-workspace extension, and junk
        assert!(hyperpanes_core::workspace::io::write_workspace(
            dir.join("one.hyperpanes"),
            &wf(Some("alpha"), vec![2])
        ));
        assert!(hyperpanes_core::workspace::io::write_workspace(
            dir.join("two.json"),
            &wf(None, vec![1, 1])
        ));
        std::fs::write(dir.join("notes.txt"), b"not a workspace").unwrap();
        std::fs::write(dir.join("broken.hyperpanes"), b"{{{").unwrap();

        let rows = scan_library(&dir);
        assert_eq!(rows.len(), 2, "only the two readable workspaces: {rows:?}");
        let names: HashSet<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        // the named file uses its `name`; the unnamed one falls back to the file stem
        assert!(names.contains("alpha"), "{names:?}");
        assert!(names.contains("two"), "{names:?}");

        // a directory that doesn't exist is empty, not a panic
        assert!(scan_library(&dir.join("nope")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_scan_reads_sets_and_skips_junk() {
        let dir = std::env::temp_dir().join(format!("hp-sets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let member = |p: &str| sets::SetMember {
            path: p.to_string(),
            name: None,
        };
        assert!(sets::write_set(
            dir.join("morning.json"),
            &sets::WorkspaceSet {
                name: "Morning".to_string(),
                members: vec![member("a.hyperpanes"), member("b.hyperpanes")],
            }
        ));
        // A set whose stored name is blank falls back to the file stem, like the library.
        assert!(sets::write_set(
            dir.join("unnamed.json"),
            &sets::WorkspaceSet {
                name: String::new(),
                members: vec![member("c.hyperpanes")],
            }
        ));
        std::fs::write(dir.join("broken.json"), b"{{{").unwrap();

        let rows = scan_sets(&dir);
        assert_eq!(rows.len(), 2, "only the two readable sets: {rows:?}");
        let by_name: HashSet<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert!(by_name.contains("Morning"), "{by_name:?}");
        assert!(by_name.contains("unnamed"), "{by_name:?}");

        // The detail line counts the set's own members, and singular/plural agree.
        let morning = rows.iter().find(|r| r.name == "Morning").unwrap();
        assert!(
            morning.detail.starts_with("2 workspaces"),
            "{:?}",
            morning.detail
        );
        let one = rows.iter().find(|r| r.name == "unnamed").unwrap();
        assert!(
            one.detail.starts_with("1 workspace ·") || one.detail == "1 workspace",
            "{:?}",
            one.detail
        );

        // a directory that doesn't exist is empty, not a panic
        assert!(scan_sets(&dir.join("nope")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- tool modes: the per-tool resumable-session list -------------------------------

    use hyperpanes_core::claude_history::{HistorySource, ProjectOrigin};
    use hyperpanes_core::tools::history::{
        ResumeBlocked, ResumeCommand, ResumePlan, SessionProvider, ToolSession,
    };

    /// A session with everything blank but the fields a test names. `started_at` stays `None`
    /// so the detail line is deterministic — the age part is dropped rather than moving with
    /// the wall clock `scan_with` reads.
    fn sess(id: &str, project: &str) -> ToolSession {
        ToolSession {
            id: id.to_string(),
            source: HistorySource::Claude,
            project: PathBuf::from(project),
            project_origin: ProjectOrigin::TranscriptExact,
            branch: None,
            started_at: None,
            summary: String::new(),
            first_user: String::new(),
            message_count: 0,
            full_text: String::new(),
        }
    }

    /// Answers `scan` from a fixed list and `resume` from a fixed verdict per id, so the
    /// shaping in `scan_with` can be checked without a transcript store on disk.
    struct FakeProvider {
        rows: Vec<ToolSession>,
        blocked: Vec<String>,
    }

    impl SessionProvider for FakeProvider {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn scan(&mut self) -> Vec<ToolSession> {
            self.rows.clone()
        }
        fn resume(&self, session: &ToolSession) -> ResumePlan {
            if self.blocked.contains(&session.id) {
                return ResumePlan::Blocked(ResumeBlocked::ToolNotInstalled { tool_id: "fake" });
            }
            ResumePlan::Ready(ResumeCommand {
                program: PathBuf::from("/opt/bin/fake"),
                args: vec!["--resume".into(), session.id.clone()],
                cwd: session.project.clone(),
            })
        }
    }

    #[test]
    fn session_label_falls_back_summary_then_prompt_then_id() {
        let mut s = sess("abcdef0123456789", "/p");
        s.summary = "  Fix the parser  ".into();
        s.first_user = "hello".into();
        assert_eq!(session_label(&s), "Fix the parser");

        // no summary → the opening prompt
        s.summary = "   ".into();
        assert_eq!(session_label(&s), "hello");

        // neither → a row is still never blank
        s.first_user = String::new();
        assert_eq!(session_label(&s), "session abcdef01");
    }

    #[test]
    fn session_detail_drops_unknown_parts_and_agrees_on_number() {
        let now = 10_000_000u64;
        let mut s = sess("id", "/p");

        // nothing known at all → an empty line rather than " ·  · "
        assert_eq!(session_detail(&s, now), "");

        s.message_count = 1;
        assert_eq!(session_detail(&s, now), "1 message");

        s.message_count = 4;
        s.branch = Some("main".into());
        assert_eq!(session_detail(&s, now), "main · 4 messages");

        // an empty branch string is "unknown", not a blank leading part
        s.branch = Some(String::new());
        assert_eq!(session_detail(&s, now), "4 messages");

        s.branch = Some("main".into());
        s.started_at = Some(now - 2 * 60 * 60 * 1000);
        assert_eq!(session_detail(&s, now), "main · 4 messages · 2h ago");
    }

    #[test]
    fn scan_with_answers_resumability_once_per_row() {
        let mut ready = sess("aaaa1111", "/work/app");
        ready.summary = "ship it".into();
        ready.branch = Some("main".into());
        ready.message_count = 3;
        let mut gone = sess("bbbb2222", "/work/old");
        gone.summary = "old thread".into();
        gone.message_count = 9;

        let mut p = FakeProvider {
            rows: vec![ready, gone],
            blocked: vec!["bbbb2222".into()],
        };
        let rows = scan_with(&mut p);
        assert_eq!(rows.len(), 2);

        // ready: keeps its own detail line, and gains a command to spawn
        assert!(rows[0].resumable());
        assert_eq!(rows[0].summary, "ship it");
        assert_eq!(rows[0].detail, "main · 3 messages");
        assert_eq!(
            rows[0].command.as_deref(),
            Some("/opt/bin/fake --resume aaaa1111")
        );
        assert_eq!(rows[0].cwd, PathBuf::from("/work/app"));

        // blocked: the reason REPLACES the detail, and there is nothing to click
        assert!(!rows[1].resumable());
        assert_eq!(rows[1].command, None);
        assert_eq!(rows[1].cwd, PathBuf::new());
        assert_ne!(rows[1].detail, "9 messages");
        assert_eq!(
            rows[1].detail,
            ResumeBlocked::ToolNotInstalled { tool_id: "fake" }.reason()
        );
    }

    #[test]
    fn a_conversation_is_open_if_any_window_has_it() {
        // The reason this registry exists: the projection that draws the badge is handed
        // ONE window's state, and a session open in another window is just as open.
        publish_open_sessions(101, HashSet::from(["here".to_string()]));
        publish_open_sessions(102, HashSet::from(["elsewhere".to_string()]));
        assert!(session_open_in_a_pane("here"));
        assert!(session_open_in_a_pane("elsewhere"));
        assert!(!session_open_in_a_pane("nowhere"));

        // A window republishes its whole set every pump, so closing the pane inside it
        // retracts the claim without anyone having to say so.
        publish_open_sessions(102, HashSet::new());
        assert!(!session_open_in_a_pane("elsewhere"));

        // And a window that closes stops claiming anything at all — otherwise the badge
        // would outlive the pane and a click would hunt for a window that is gone.
        forget_window(101);
        assert!(!session_open_in_a_pane("here"));
        forget_window(102);
    }

    #[test]
    fn applied_rows_are_served_from_cache_and_looked_up_by_id() {
        let rows = vec![
            ScannedSession {
                id: "one".into(),
                project: PathBuf::from("/work/app"),
                summary: "first".into(),
                detail: "main".into(),
                command: Some("/opt/bin/fake --resume one".into()),
                cwd: PathBuf::from("/work/app"),
                desktop: None,
            },
            ScannedSession {
                id: "two".into(),
                project: PathBuf::from("/work/app"),
                summary: "second".into(),
                detail: "gone".into(),
                command: None,
                cwd: PathBuf::new(),
                desktop: None,
            },
        ];
        apply_tool_sessions("cachetest", rows.clone());

        // Served from the cache: `now` is read back off the entry, so asking inside the TTL
        // must not enqueue a scan (this test would otherwise start the scan thread).
        let at = TOOL_CACHE.with(|c| c.borrow().get("cachetest").map(|(at, _)| *at).unwrap());
        let overrides = std::collections::BTreeMap::new();
        assert_eq!(tool_sessions("cachetest", &overrides, at), rows);
        assert_eq!(
            tool_sessions("cachetest", &overrides, at + TOOL_SCAN_TTL_MS),
            rows
        );

        // By id, never by index — a re-scan reorders rows and a click must still land right.
        assert_eq!(tool_session("cachetest", "two").unwrap().summary, "second");
        assert!(!tool_session("cachetest", "two").unwrap().resumable());
        assert!(tool_session("cachetest", "one").unwrap().resumable());
        assert!(tool_session("cachetest", "nope").is_none());
        assert!(tool_session("othertool", "one").is_none());
    }
}
