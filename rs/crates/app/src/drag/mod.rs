//! Drag / tear-off — the app's signature interaction (Phase 4, Wave 2).
//!
//! This module owns the **global-cursor drag pump** (lifted from `spike-tearoff`) and
//! the pure geometry used to resolve a drop. The pump does *not* lean on Slint pointer
//! delivery (which is per-window and loses the grab the instant the cursor crosses into
//! another window — exactly what tear-off needs). Instead a drag is *started* by a Slint
//! pointer-down (on a pane header or a tab), and from then on the whole gesture is driven
//! from **global Win32 state** read every 8 ms by [`crate::app::App::tick`]:
//!   * `GetCursorPos`     → screen-global cursor (Slint has no global-cursor API);
//!   * `GetAsyncKeyState` → left-button-still-down / released (drag end);
//!   * `GetWindowRect`    → hit-test the cursor against each window's screen rect.
//!
//! Once the cursor leaves the source window a transparent / click-through / topmost
//! **ghost** (a pure Win32 layered window, kept out of Slint's render path) chases the
//! cursor. On release the drop is resolved against the window under the cursor:
//!   * over another window's **pane area** → *stitch* the pane in at the hovered slot;
//!   * over another window's **tab strip**  → *dock* the pane as a new tab;
//!   * over **empty space**                → a *new window* hosting the pane.
//! A drop back inside the source window **reorders** (pane → slot, tab → strip position).
//!
//! `State` is never mutated mid-drag; the source pane/tab stays put and the ghost+preview
//! provide the live feedback. The detach→adopt (replay-primed, no PTY restart) happens
//! only on release, so a cancelled drag costs nothing.

/// Movement past this many **physical** px (from the press point) promotes a pending
/// press into a real drag — below it, the gesture is just a click (focus / select).
pub const DRAG_THRESHOLD_PX: i32 = 6;

/// Fraction of a tile (along the layout axis) at each end that counts as the "insert
/// before / after" edge band for a stitch; capped so the band stays edge-like on a big
/// tile. Mirrors `src/renderer/stitch.ts` (`EDGE_BAND_FRAC` / `EDGE_BAND_MAX_PX`).
const EDGE_BAND_FRAC: f32 = 0.3;
const EDGE_BAND_MAX_PX: f32 = 140.0;

/// What is being dragged. Just the identity of the dragged element — the chrome (title /
/// accent) is re-read fresh from the live pane at drop time (via `detach_uid`), so a drag
/// never carries a stale snapshot.
#[derive(Debug, Clone)]
pub enum DragKind {
    /// A pane pulled by its header (by session `uid`).
    Pane { uid: String },
    /// A tab pulled along the strip (in-window reorder); `index` is its live position,
    /// updated as it slides between siblings.
    Tab { index: usize },
}

/// One in-flight drag, owned by the app while a gesture is live.
pub struct DragState {
    /// Registry index of the window the gesture started in.
    pub source_win: usize,
    pub kind: DragKind,
    /// Press point in **physical** screen px (to measure the drag threshold).
    pub origin: (i32, i32),
    /// Seen the button actually held (debounces a stale "up" right after the grab).
    pub armed: bool,
    /// Crossed [`DRAG_THRESHOLD_PX`] → a real drag (ghost + previews are now live).
    pub active: bool,
}

impl DragState {
    pub fn new(source_win: usize, kind: DragKind, origin: (i32, i32)) -> Self {
        DragState {
            source_win,
            kind,
            origin,
            armed: false,
            active: false,
        }
    }
    pub fn is_pane(&self) -> bool {
        matches!(self.kind, DragKind::Pane { .. })
    }
}

/// Where the cursor currently is, resolved into a drop target. Built each tick by the
/// app from the live window geometry; consumed both to paint previews and to apply the
/// drop on release.
#[derive(Debug, Clone, Default)]
pub struct Hover {
    /// Registry index of the window under the cursor (`None` = empty space).
    pub win: Option<usize>,
    /// Cursor is over that window's tab strip (the top bar).
    pub over_strip: bool,
    /// Cursor is over that window's left panel (the workspace tree). The tree resolves the
    /// group + slot itself — see `LeftPanelAdapter::ext_drop_tab` — so no geometry for it is
    /// duplicated here.
    pub over_left_panel: bool,
    /// The cursor in that window's own logical coordinates (from the window's top-left),
    /// which is the frame Slint's `absolute-position` uses.
    pub local: (f32, f32),
    /// Insertion index in the strip (for a tab reorder / dock caret).
    pub tab_slot: usize,
    /// The existing tab chip directly under the cursor (vs the empty strip / `+`), if any.
    /// Drives spring-load (hover-to-switch) and dock-into-that-tab on drop.
    pub tab_over: Option<usize>,
    /// Pane tile under the cursor (active-tab pane index), if any.
    pub pane_idx: Option<usize>,
    /// Cursor is within the hovered pane's **header** band (the drag handle) — drives the
    /// idle open-hand cursor.
    pub over_header: bool,
    /// Insertion index among that tab's panes for a stitch (edge-band aware).
    pub slot_index: usize,
    /// The hovered pane's rect (area-relative logical px) — for the slot highlight.
    pub pane_rect: (f32, f32, f32, f32),
    /// The edge marker within the hovered tile: 0 left · 1 right · 2 top · 3 bottom.
    pub edge: u8,
}

/// Edge bands of a tile of size `size` along its layout axis. Returns the slot offset
/// (`0` insert-before, `1` insert-after) and which edge the marker sits on.
///
/// The whole tile is a drop target — the edge bands are only the *unambiguous* ends; the
/// middle resolves to whichever half the pointer is in. It used to resolve to insert-after
/// unconditionally, which made the commonest gesture in the app a silent no-op: dragging a
/// pane leftward onto the centre of its immediate left neighbour asks for slot `j + 1`,
/// which is the slot the pane already occupies, so `reorder_pane_in` returned without
/// moving anything. Splitting at the midpoint means a drop only no-ops when the caret is
/// genuinely already where the pane sits.
pub fn edge_band(pos: f32, size: f32, vertical: bool) -> (usize, u8) {
    let band = (size * EDGE_BAND_FRAC).min(EDGE_BAND_MAX_PX);
    let before = if pos <= band {
        true // near edge → insert before
    } else if pos >= size - band {
        false // far edge → insert after
    } else {
        pos < size / 2.0 // centre → the nearer half
    };
    if before {
        (0, if vertical { 2 } else { 0 }) // before → top/left
    } else {
        (1, if vertical { 3 } else { 1 }) // after → bottom/right
    }
}

/// Translate "put the dragged pane **on** tile `dest`" into the insertion index
/// [`crate::state::State::reorder_pane`] wants.
///
/// An insertion caret and a tile index are not the same thing, and conflating them is why a
/// pane could never be dropped into the first tile. The caret model asks "before or after
/// tile `j`?", which is a sound question only for a *linear* strip. The pane area is not
/// linear: four panes lay out as a 2×2 grid (row-major — 0 top-left, 1 top-right, 2
/// bottom-left, 3 bottom-right) and [`edge_band`] then consults the **x** offset alone, so
/// the grid's whole vertical axis is invisible to the hit test. Pointing at the middle of the
/// top-left tile — the natural aim — lands in its right half and resolves to caret 1, i.e.
/// **top-right**; and if the pane being dragged already sits at index 1, that same caret is
/// the slot it occupies, so the move is a silent no-op and the pane *stays* top-right. Either
/// way the top-left tile is unreachable from anywhere but its left 50%.
///
/// Treating the hovered tile as the destination makes every tile — the first included — a
/// full-tile target, in a grid and in a strip alike. The caret model is kept where it is
/// still the right question: stitching a pane in from another window or another tab, where
/// there is no "current index" to move away from.
pub fn insertion_for(from: usize, dest: usize) -> usize {
    if dest >= from {
        dest + 1
    } else {
        dest
    }
}

// ---- the per-platform pointer pump + ghost (the GlobalPointer seam) ----

/// The global-pointer seam the drag pump runs on. The whole tear-off gesture is driven
/// from OS-global pointer state polled every tick (Slint pointer delivery is per-window
/// and loses the grab the instant the cursor crosses into another window).
///
/// Implementations: Windows = `GetCursorPos`/`GetAsyncKeyState` (`windows.rs`). The
/// Wave-1 platform tracks own `linux.rs`/`macos.rs`; Wayland cannot poll a global
/// cursor, so its implementation returns `supports_cross_window() == false` and the
/// app falls back to in-window drags only.
pub trait GlobalPointer {
    /// Screen-global cursor position (physical px) + whether the primary (left) button
    /// is currently held. `None` when the platform cannot read global pointer state —
    /// the drag pump then never engages.
    fn poll(&self) -> Option<(slint::PhysicalPosition, bool)>;
    /// Whether the pointer can be tracked across/outside this app's own windows
    /// (drives tear-off-to-new-window and cross-window stitch/dock). Unused on Windows
    /// today (always true there); the Wayland in-window fallback branches on it.
    #[allow(dead_code)]
    fn supports_cross_window(&self) -> bool;
}

/// The platform's global pointer (a static zero-sized provider).
pub fn global_pointer() -> &'static dyn GlobalPointer {
    &platform::PlatformPointer
}

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(not(any(windows, target_os = "macos")))]
#[path = "linux.rs"]
mod platform;

pub use platform::{window_rect, Ghost};

#[cfg(test)]
mod edge_band_tests {
    //! The centre of a tile used to resolve to insert-*after* unconditionally, which made
    //! the most natural reorder gesture a silent no-op: dragging a pane onto the middle of
    //! its immediate left neighbour asks for the slot the pane already occupies.
    use super::edge_band;

    const W: f32 = 400.0; // band = 0.3 * 400 = 120px at each end

    #[test]
    fn the_near_edge_inserts_before_and_the_far_edge_after() {
        assert_eq!(edge_band(4.0, W, false), (0, 0)); // left edge
        assert_eq!(edge_band(W - 4.0, W, false), (1, 1)); // right edge
        assert_eq!(edge_band(4.0, W, true), (0, 2)); // top edge
        assert_eq!(edge_band(W - 4.0, W, true), (1, 3)); // bottom edge
    }

    #[test]
    fn the_centre_resolves_to_the_half_the_pointer_is_in() {
        // Just inside the near half → before; just past the midpoint → after.
        assert_eq!(edge_band(W / 2.0 - 1.0, W, false), (0, 0));
        assert_eq!(edge_band(W / 2.0 + 1.0, W, false), (1, 1));
        assert_eq!(edge_band(W / 2.0 - 1.0, W, true), (0, 2));
        assert_eq!(edge_band(W / 2.0 + 1.0, W, true), (1, 3));
    }

    #[test]
    fn dropping_on_a_left_neighbours_near_half_is_a_real_move() {
        // Panes [0,1,2]; drag pane 2 onto the near half of pane 1 → slot 1, which
        // `reorder_pane_in` turns into a genuine move (before: slot 2 = a no-op).
        let (off, _) = edge_band(W * 0.4, W, false);
        assert_eq!(1 + off, 1);
    }

    #[test]
    fn a_capped_band_on_a_huge_tile_still_splits_at_the_midpoint() {
        // 1000px tile: 0.3 * 1000 = 300 > EDGE_BAND_MAX_PX, so the bands cap at 140px and
        // the (large) middle must still fall to the nearer half.
        let w = 1000.0;
        assert_eq!(edge_band(400.0, w, false), (0, 0));
        assert_eq!(edge_band(600.0, w, false), (1, 1));
    }
}

#[cfg(test)]
mod insertion_for_tests {
    //! A tile is a *destination*, not a caret. `reorder_pane_in` removes the pane first and
    //! then inserts, so landing on index `dest` needs the caret one past it when moving
    //! rightward — the arithmetic that makes tile 0 reachable at all.
    use super::insertion_for;

    /// The round trip through `reorder_pane_in`'s own translation (`to > from → to - 1`).
    fn lands_at(from: usize, dest: usize) -> usize {
        let to = insertion_for(from, dest);
        if to > from {
            to - 1
        } else {
            to
        }
    }

    #[test]
    fn every_tile_is_reachable_from_every_other() {
        for n in 2..=6 {
            for from in 0..n {
                for dest in 0..n {
                    assert_eq!(lands_at(from, dest), dest, "{from} → {dest} of {n}");
                }
            }
        }
    }

    #[test]
    fn the_top_left_tile_is_reachable_from_the_top_right_one() {
        // The reported bug, as a 2×2 grid: pane 1 (top-right) dropped on tile 0 (top-left)
        // must actually land at 0. Under the old caret model the natural aim — the middle of
        // the top-left tile — resolved to caret 1, which is where the pane already was.
        assert_eq!(lands_at(1, 0), 0);
        assert_eq!(lands_at(2, 0), 0);
        assert_eq!(lands_at(3, 0), 0);
    }

    #[test]
    fn dropping_a_pane_back_on_its_own_tile_is_a_no_op() {
        // dest == from asks for caret from + 1, which `reorder_pane_in` folds back to `from`
        // and returns early on — no relabel, no layout churn.
        assert_eq!(insertion_for(2, 2), 3);
        assert_eq!(lands_at(2, 2), 2);
    }
}
