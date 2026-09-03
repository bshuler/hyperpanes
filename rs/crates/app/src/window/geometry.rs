//! Remember the main window's frame across launches — the cross-platform half.
//!
//! The app never had geometry of its own: it leaned on macOS's own frame restoration, so
//! startup visibly passed through TWO sizes (Slint built the window at the `.slint`
//! `preferred-width`/`preferred-height` of 1280x800, then the OS shoved it back to the
//! frame the human had left), and on Windows/Linux there was no restoration at all. Every
//! one of those size changes reaches each pane as a pty resize, which is precisely the
//! startup churn `paneview::PTY_RESIZE_SETTLE` exists to absorb; removing the churn at the
//! source is why this module applies the frame BEFORE the first show.
//!
//! **Why before `show()` works.** `AppWindow::show()` is called while
//! `slint::run_event_loop()` has not started yet, so no native window exists: the winit
//! adapter is still in its `WinitWindowOrNone::None(WindowAttributes)` state and every
//! `set_size`/`set_position`/`set_maximized` writes straight into the attributes the window
//! will be BORN with. Doing it here is not merely early, it is load-bearing — Slint resizes
//! a window to its component's preferred size on first show *unless* `has_explicit_size` is
//! set, and the only thing that sets that flag is a `set_size` call like this one. Restore
//! from any later point (a timer, the first frame, an event-loop callback) and the 1280x800
//! flash comes back.
//!
//! Sizes and positions are handed over as LOGICAL values on purpose: the adapter forwards
//! them to winit as `Logical`, so the physical conversion happens at window creation with
//! the real monitor's scale factor — a frame saved on a 2x display comes back the same
//! apparent size on a 1x one.
//!
//! **Why saving polls instead of hooking window events.** The obvious route,
//! `WinitWindowAccessor::on_winit_window_event`, installs a SINGLE per-window filter that
//! the next registration overwrites — and `window/linux.rs` installs its own from
//! `hwnd_of()` (pointer tracking + the frameless re-strip) after this code runs. Hooking
//! here would silently disable that on Linux, or be silently disabled by it, depending on
//! ordering. A poll of the public `Window::position()`/`size()` costs nothing measurable at
//! this cadence, works identically on all three platforms, and is inherently the debounce
//! the file writes need: a drag produces one write when it stops, not one per frame.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use hyperpanes_core::persistence::window_geometry::{self, DisplayRect, WindowGeometry};
use slint::{ComponentHandle, LogicalPosition, LogicalSize};

/// Poll cadence for the frame watcher. Long enough to be free, short enough that a
/// quit right after a resize still catches it (and the event-loop exit flushes anyway).
const POLL: Duration = Duration::from_millis(400);

/// Consecutive unchanged polls before the frame is written. This IS the debounce: a drag
/// or resize keeps resetting the counter, so the file is written roughly [`POLL`] * 2 after
/// the motion stops instead of once per frame while it is happening.
const SETTLE_POLLS: u8 = 2;

thread_local! {
    /// One window owns the remembered frame. Latched so a re-host or tear-off window can
    /// never start a second watcher fighting the first over the same file.
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    /// Kept alive for the process; dropping a `slint::Timer` stops it.
    static TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
    /// Last frame we observed, to detect "nothing moved" without touching the disk.
    static LAST_SEEN: Cell<Option<WindowGeometry>> = const { Cell::new(None) };
    /// The most recent NON-maximized frame. A maximized window reports the screen rect,
    /// which is useless to restore to — remembering the restored frame underneath is what
    /// makes un-maximizing after a relaunch land where the human left it.
    static LAST_FRAME: Cell<Option<WindowGeometry>> = const { Cell::new(None) };
    /// Observed but not yet written (see [`SETTLE_POLLS`]); also what [`flush`] writes.
    static PENDING: Cell<Option<WindowGeometry>> = const { Cell::new(None) };
    /// Polls since [`PENDING`] last changed.
    static STABLE: Cell<u8> = const { Cell::new(0) };
}

/// Apply the remembered frame to `aw` and start watching it. Call once, from the window's
/// construction, BEFORE `show()` — see the module docs for why the ordering is not
/// cosmetic.
///
/// `id` is the window's registry id: only window 0 is the app's window in the sense the
/// human means. Tear-off and re-host windows keep their existing cascade placement, because
/// restoring them all to one remembered frame would stack them exactly on top of each other.
#[tracing::instrument(level = "debug", ret, skip(aw))]
pub fn restore_geometry(id: usize, aw: &crate::AppWindow) {
    if id != 0 || WATCHING.with(|w| w.replace(true)) {
        return;
    }
    let win = aw.window();
    let saved = window_geometry::load();
    let clamped = saved.clamp_to_displays(&displays());
    if clamped != saved {
        tracing::debug!("geometry: remembered frame {saved:?} is off the attached displays; using {clamped:?}");
    }
    // Size FIRST: it is the call that pins the adapter's `has_explicit_size`, and until
    // that is set the first show would resize the window to the .slint preferred size and
    // drag the position along with it.
    if let Some((w, h)) = clamped.size() {
        win.set_size(LogicalSize::new(w as f32, h as f32));
    }
    if let Some((x, y)) = clamped.position() {
        win.set_position(LogicalPosition::new(x as f32, y as f32));
    }
    if clamped.maximized {
        win.set_maximized(true);
    }
    // Seed the watcher with what we just applied, so an untouched session does not rewrite
    // an identical file on its first poll.
    LAST_SEEN.with(|c| c.set(Some(clamped)));
    LAST_FRAME.with(|c| c.set(Some(clamped)));

    let weak = aw.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, POLL, move || {
        let Some(aw) = weak.upgrade() else {
            // The window is gone; write whatever the last poll saw and stop looking.
            flush_geometry();
            return;
        };
        poll(aw.window());
    });
    TIMER.with(|t| *t.borrow_mut() = Some(timer));
}

/// One watcher tick: read the live frame, and write it only once it has stopped changing.
#[tracing::instrument(level = "debug", ret, skip(win))]
fn poll(win: &slint::Window) {
    // A fullscreen window's frame is the screen, and restoring INTO fullscreen without the
    // human asking is hostile — so fullscreen is simply not observed. The last windowed
    // frame stays remembered, which is the one worth coming back to.
    if win.is_fullscreen() {
        return;
    }
    let maximized = win.is_maximized();
    let now = if maximized {
        // Keep the frame underneath; only the flag changes.
        WindowGeometry {
            maximized: true,
            ..LAST_FRAME.with(|c| c.get()).unwrap_or_default()
        }
    } else {
        // `position()` is the OUTER top-left and `size()` the INNER size, both physical —
        // the same asymmetric pair `set_position`/`set_size` take back, so save and restore
        // are exact inverses. A zero scale factor would silently produce an infinite frame.
        let scale = win.scale_factor();
        let scale = if scale > 0.0 { scale } else { 1.0 };
        let p = win.position();
        let s = win.size();
        let g = WindowGeometry {
            x: Some((p.x as f32 / scale).round() as i64),
            y: Some((p.y as f32 / scale).round() as i64),
            width: Some((s.width as f32 / scale).round() as i64),
            height: Some((s.height as f32 / scale).round() as i64),
            maximized: false,
        };
        LAST_FRAME.with(|c| c.set(Some(g)));
        g
    };
    // A window that has not been realized yet reports a degenerate size; recording it would
    // overwrite a perfectly good remembered frame with a placeholder.
    if now.size().is_none_or(|(w, h)| w <= 0 || h <= 0) {
        return;
    }
    if LAST_SEEN.with(|c| c.get()) != Some(now) {
        LAST_SEEN.with(|c| c.set(Some(now)));
        PENDING.with(|c| c.set(Some(now)));
        STABLE.with(|c| c.set(0));
        return;
    }
    if PENDING.with(|c| c.get()).is_none() {
        return;
    }
    let stable = STABLE.with(|c| {
        c.set(c.get().saturating_add(1));
        c.get()
    });
    if stable >= SETTLE_POLLS {
        flush_geometry();
    }
}

/// Write any frame the watcher observed but has not committed yet.
///
/// Called from the watcher once the frame settles, and again from `main` after the event
/// loop returns — a quit within the settle window would otherwise lose the very last
/// resize, which is exactly the one the human just made.
#[tracing::instrument(level = "debug", ret)]
pub fn flush_geometry() {
    let Some(g) = PENDING.with(|c| c.take()) else {
        return;
    };
    STABLE.with(|c| c.set(0));
    if let Err(e) = window_geometry::save(&g) {
        // Never fatal: a window that cannot be remembered is a papercut, a failed launch or
        // a failed quit is not.
        tracing::debug!("geometry: save failed: {e}");
    }
}

/// The attached displays, as logical rectangles, via the platform seam.
///
/// An empty list means "could not enumerate" (Wayland, an unexpected platform) and makes
/// the clamp a no-op rather than a guess — see `WindowGeometry::clamp_to_displays`.
#[tracing::instrument(level = "debug", ret)]
fn displays() -> Vec<DisplayRect> {
    super::platform::displays()
        .into_iter()
        .filter(|&(_, _, w, h)| w > 0 && h > 0)
        .map(|(x, y, w, h)| DisplayRect {
            x: x as i64,
            y: y as i64,
            width: w as i64,
            height: h as i64,
        })
        .collect()
}
