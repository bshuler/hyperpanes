//! Native window chrome — the per-platform glue for the frameless window: strip the OS
//! title bar, install the subclass/hooks, min/max/close, borderless OS fullscreen
//! (save/restore placement), and the drag/hover cursor overrides.
//!
//! The dispatch is a cfg-selected module re-export (the lightest seam that keeps every
//! call site unchanged): each platform file exports the SAME function surface, frozen in
//! `docs/ports-seams.md`:
//!
//! ```text
//! pub type/struct SavedPlacement;                  // opaque pre-fullscreen placement
//! pub fn hwnd_of(win: &slint::Window) -> isize;    // native handle (0 until realized)
//! pub fn make_frameless(raw: isize);               // frameless chrome + hooks install
//! pub fn start_drag(raw: isize);                   // system move-drag (drag-the-bar)
//! pub fn begin_drag_cursor(raw: isize);            // force the tear-off drag cursor
//! pub fn end_drag_cursor(raw: isize);              // release drag cursor + capture
//! pub fn set_hover_cursor(on: bool);               // open-hand hover cursor on/off
//! pub fn minimize(raw: isize);
//! pub fn toggle_max(raw: isize);
//! pub fn is_maximized(raw: isize) -> bool;
//! pub fn close(raw: isize);
//! pub fn raise(raw: isize);                       // un-minimize + key + app to the front
//! pub fn enter_fullscreen(raw: isize) -> Option<SavedPlacement>;
//! pub fn exit_fullscreen(raw: isize, saved: SavedPlacement);
//! pub fn displays() -> Vec<(i32, i32, i32, i32)>; // usable desktop rects, top-left x/y/w/h
//! ```
//!
//! `displays()` is the one entry that is not about chrome: it feeds the startup frame
//! restore in [`geometry`], which has to know where the monitors are BEFORE any window
//! exists — so it cannot ask winit, and each platform answers for itself. The contract is
//! deliberately loose in one direction only: a rect must never be SMALLER than the logical
//! area it describes, because the caller uses it to decide whether a remembered frame is
//! stranded, and an under-estimate would move a window the human placed on purpose. macOS
//! answers exactly (AppKit points are logical px); Windows and X11 answer with the physical
//! virtual-screen box, a superset under any scaling. An EMPTY vec means "cannot tell"
//! (Wayland, headless) and the caller reads it as "do not clamp".
//!
//! `windows.rs` is the original Win32 implementation (moved verbatim from the old
//! `window.rs`); `linux.rs` / `macos.rs` are compiling no-op stubs owned by the Wave-1
//! platform tracks.

mod geometry;

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;

// Linux is also the fallback for other unixes (BSDs etc.).
#[cfg(not(any(windows, target_os = "macos")))]
#[path = "linux.rs"]
mod platform;

pub use geometry::{flush_geometry, restore_geometry};
pub use platform::*;
