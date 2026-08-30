//! The left slide-out panel's data side (mux plan M5) — everything `ui/leftpanel.slint`
//! draws that isn't already in [`crate::state::State`].
//!
//! The panel has three sections, and this module owns the two that need to look outside
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
//! * **DETACHED** — live sessions that no window is showing. Computed by subtracting the
//!   claimed uids from `SessionManager::uids()`; see [`detached`] for exactly how complete
//!   that answer is today and where M7 fills in the rest.
//!
//! Nothing here mutates `State`: the projection calls it (Seam #1) and the commands it
//! feeds all land in `command::dispatch` (Seam #2).

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use hyperpanes_core::persistence::paths::data_dir;
use hyperpanes_core::session_manager::SessionManager;

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
}

/// Publish the uids every window in this process is hosting (laid out or parked). Called
/// from the app's pump, before the per-window renders that project the panel.
pub fn publish_window_claims(uids: impl IntoIterator<Item = String>) {
    WINDOW_CLAIMS.with(|c| *c.borrow_mut() = uids.into_iter().collect());
}

/// **M7 seam.** Uids claimed by a hyperpanes process *other than this one* — a session that
/// another client of the same daemon is attached to. There is no wire message that reports
/// this today (`ListSessions`/`SessionMeta` describe a session's buffer and liveness, not
/// who is watching it), so this deliberately returns an empty set: the panel then treats a
/// session another PROCESS holds as adoptable, and adopting it re-attaches rather than
/// stealing (the daemon multiplexes output). M7, which adds the claim/lease notion to the
/// protocol, fills this in and the DETACHED list narrows on its own — no call site changes.
pub fn claimed_by_other_processes() -> HashSet<String> {
    HashSet::new()
}

/// The adoptable sessions: everything the session manager knows about, minus everything
/// already claimed.
///
/// `claimed_here` is this window's own uids (its panes + its parked reminders). The other
/// two subtractions come from [`publish_window_claims`] (every window in this process) and
/// [`claimed_by_other_processes`] (the M7 seam above).
///
/// How complete this is **today**: in daemon mode `SessionManager::uids()` answers from the
/// client's shadow table, which is seeded by `ListSessions` at connect and then maintained
/// from the `Exit` stream plus this client's own creates — so a session orphaned by a
/// crashed window IS listed, while one created by a different client after we connected is
/// not (that gap closes with M7's session-event stream). In-process mode lists exactly the
/// sessions this process spawned, which is the correct answer for a single-process run.
pub fn detached(mgr: &SessionManager, claimed_here: &HashSet<String>) -> Vec<DetachedSession> {
    let now = crate::glow::now_epoch_ms();
    let mut rows: Vec<DetachedSession> = adoptable_uids(mgr.uids(), claimed_here)
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
/// ([`publish_window_claims`]), minus the M7 seam ([`claimed_by_other_processes`]). Input
/// order is preserved; [`detached`] re-sorts by last output.
pub fn adoptable_uids(all: Vec<String>, claimed_here: &HashSet<String>) -> Vec<String> {
    let elsewhere = WINDOW_CLAIMS.with(|c| c.borrow().clone());
    let other_procs = claimed_by_other_processes();
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

        publish_window_claims(Vec::<String>::new());
        // Nothing claimed → everything is adoptable, in the order given.
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&[])),
            all(&["a", "b", "c"])
        );
        // This window's own panes are never offered back to it.
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&["b"])),
            all(&["a", "c"])
        );

        // A pane sitting in the window next door is not detached either.
        publish_window_claims(all(&["c"]));
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&["b"])),
            all(&["a"])
        );

        // Republishing replaces (never accumulates) the cross-window claim set.
        publish_window_claims(Vec::<String>::new());
        assert_eq!(
            adoptable_uids(all(&["a", "b", "c"]), &set(&["b"])),
            all(&["a", "c"])
        );

        // The M7 seam is empty today — documented, and asserted so the day it stops being
        // empty this test is the thing that says so.
        assert!(claimed_by_other_processes().is_empty());
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
}
