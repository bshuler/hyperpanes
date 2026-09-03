//! One winit window-event filter per window, shared by every feature that wants one.
//!
//! `WinitWindowAccessor::on_winit_window_event` installs a **single** filter per window —
//! `adapter.window_event_filter.set(..)` — so the second feature to register silently
//! unhooks the first. Two features want it now (the Linux frameless re-strip + pointer
//! tracker in [`crate::window::linux`], and the OS file drop in [`crate::filedrop`]),
//! and on Linux they would land on the same window.
//!
//! So nothing registers with Slint directly any more: everything goes through [`add`],
//! which installs the real filter once per window and fans each event out to every hook.
//! `PreventDefault` from any one hook wins — a hook that swallows an event is making a
//! claim about the event, not about the other hooks — but every hook still runs, so a
//! swallowed event can't skip a tracker that was counting on seeing it.
//!
//! Windows are keyed by the same native handle the app already uses as a window's
//! identity (`Window::hwnd`), so a hook can be added from anywhere that has one.

use std::cell::RefCell;
use std::collections::HashMap;

use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::{EventResult, WinitWindowAccessor};

/// One registered hook. `&slint::Window` is the window the event arrived on.
pub type Hook = Box<dyn FnMut(&slint::Window, &WindowEvent) -> EventResult>;

thread_local! {
    /// Hooks per window key, in registration order. The event loop is single-threaded, so
    /// a thread-local is the whole synchronization story.
    static HOOKS: RefCell<HashMap<isize, Vec<Hook>>> = RefCell::new(HashMap::new());
}

/// Register `hook` for the window identified by `key` (its native handle).
///
/// The first hook for a key installs the underlying Slint filter; later ones just join the
/// list, so callers never have to know whether they are first.
#[tracing::instrument(level = "debug", skip_all)]
pub fn add(win: &slint::Window, key: isize, hook: Hook) {
    let first = HOOKS.with(|h| {
        let mut h = h.borrow_mut();
        let list = h.entry(key).or_default();
        list.push(hook);
        list.len() == 1
    });
    if first {
        win.on_winit_window_event(move |w, ev| dispatch(key, w, ev));
    }
}

/// Forget every hook for a window (called when its native handle is retired, so a later
/// window that reuses the address doesn't inherit them).
#[allow(dead_code)]
#[tracing::instrument(level = "debug", ret)]
pub fn clear(key: isize) {
    HOOKS.with(|h| {
        h.borrow_mut().remove(&key);
    });
}

#[tracing::instrument(level = "debug", skip(win))]
fn dispatch(key: isize, win: &slint::Window, ev: &WindowEvent) -> EventResult {
    // Take the list out for the duration of the call. A hook is free to `add` another one
    // while it runs; holding the borrow across that would panic, and re-entering
    // `dispatch` would run the same list twice.
    let mut list = HOOKS
        .with(|h| h.borrow_mut().remove(&key))
        .unwrap_or_default();
    let mut result = EventResult::Propagate;
    for hook in list.iter_mut() {
        if matches!(hook(win, ev), EventResult::PreventDefault) {
            result = EventResult::PreventDefault;
        }
    }
    HOOKS.with(|h| {
        let mut h = h.borrow_mut();
        // Anything registered *during* the dispatch is already under `key`; keep it, after
        // the hooks that were there first.
        let added = h.remove(&key).unwrap_or_default();
        list.extend(added);
        h.insert(key, list);
    });
    result
}
