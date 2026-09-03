//! Linux (and other non-macOS unix) half of the rights seam.
//!
//! There is no central permission authority. Under X11 nothing is gated at all: any client
//! can read the screen and any process can read the files its uid owns. Under Wayland,
//! screen capture goes through the xdg-desktop-portal ScreenCast interface, which raises
//! its own picker at capture time — so the honest answer there is `Undetermined`, and the
//! "request" is simply to start the capture and let the portal ask.
//!
//! `WAYLAND_DISPLAY` is the same signal `drag/linux.rs` already keys off, so the two agree
//! about which session type we are in.
//!
//! Nothing here probes, and that is the honest answer rather than a gap. The portal's
//! `ScreenCast` interface has no "do I already have this" call — a grant is a session token
//! handed back by a picker the user just answered, not state we can read — and the remaining
//! rights are ungated for a native build, so there is no state to read at all. A Flatpak or
//! Snap of Hyperpanes would change that; if one is ever shipped, this file is where the
//! sandbox-aware answers go.

use super::{Grant, Right};

#[tracing::instrument(level = "debug", ret)]
pub fn label(right: Right) -> &'static str {
    match right {
        Right::ScreenRecording => "Screen capture",
        Right::FullDiskAccess => "File access",
        Right::Accessibility => "Accessibility",
        Right::Notifications => "Notifications",
        Right::Microphone => "Microphone",
        Right::Automation => "Automation",
    }
}

/// True when we're on a Wayland session rather than X11.
#[tracing::instrument(level = "debug", ret)]
fn is_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok_and(|v| !v.is_empty())
}

#[tracing::instrument(level = "debug", ret)]
pub fn status(right: Right) -> Grant {
    match right {
        // Portal-mediated on Wayland (the portal asks at capture time), ungated on X11.
        Right::ScreenRecording if is_wayland() => Grant::Undetermined,
        // Flatpak/snap sandboxes gate these, but a native build has whatever its uid has.
        _ => Grant::NotApplicable,
    }
}

/// The portal raises its own picker when the capture starts, so there is nothing for us to
/// raise ahead of it — asking twice would only mean two dialogs for one grant.
#[tracing::instrument(level = "debug", ret)]
pub fn prompt(right: Right) -> Grant {
    status(right)
}

#[tracing::instrument(level = "debug", ret)]
pub fn request(right: Right) -> Result<(), String> {
    match right {
        // Nothing to open: the portal's own picker appears when capture starts, and there
        // is no cross-desktop settings pane that would show a pending grant.
        Right::ScreenRecording if is_wayland() => Ok(()),
        _ => Err(format!("{} is not gated on this desktop", right.id())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_wayland_screen_capture_is_ever_gated() {
        for r in Right::all() {
            if *r == Right::ScreenRecording {
                continue;
            }
            assert_eq!(status(*r), Grant::NotApplicable, "{:?}", r);
        }
    }
}
