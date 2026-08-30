//! The OS-rights seam: what Hyperpanes may need permission for, whether it has it, and
//! how to take the user to the place where they can grant it.
//!
//! Nothing in the app needs any of these on day one. The point of landing the seam early
//! is that the day a feature does need one — a screen-capture pane, a tool that reads
//! `~/Library`, a global hotkey — the ask is a two-line call rather than a per-OS research
//! project bolted on under deadline.
//!
//! Deliberately conservative for now: [`status`] answers `NotApplicable` where the OS has
//! no such concept and `Undetermined` where it does, rather than guessing. A real probe
//! (`CGPreflightScreenCaptureAccess` and friends on macOS) needs framework bindings this
//! crate does not carry yet, and a *wrong* "granted" is worse than an honest "don't know" —
//! the caller's fallback for `Undetermined` and `Denied` is the same: offer [`request`].
//!
//! On macOS a grant is keyed to the *signing identity*, so on today's unsigned bundle any
//! grant the user makes is revoked by the next rebuild. That is a packaging question
//! (open question Q1 in `docs/tool-panes-plan.md`), not a code one, and it does not change
//! this surface.
//!
//! Shape follows `docs/ports-seams.md`. Owned by track `tool-panes` (Wave 0).

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(not(any(windows, target_os = "macos")))]
#[path = "linux.rs"]
mod platform;

/// A capability the OS gates behind an explicit user grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Right {
    /// Capture the screen — a screenshot pane, or sharing a pane's output.
    ScreenRecording,
    /// Read files the OS protects by default (`~/Library`, Mail, Messages, other apps'
    /// containers). Several CLI tools keep their history there.
    FullDiskAccess,
    /// Synthesize and observe input — global hotkeys, window placement.
    Accessibility,
    /// Post notifications when a long-running agent finishes.
    Notifications,
    /// Record audio (dictation into a pane).
    Microphone,
    /// Drive another application (AppleScript / `osascript`).
    Automation,
}

impl Right {
    /// Stable identifier for logs, settings, and the UI. Never localise this.
    pub fn id(self) -> &'static str {
        match self {
            Right::ScreenRecording => "screen-recording",
            Right::FullDiskAccess => "full-disk-access",
            Right::Accessibility => "accessibility",
            Right::Notifications => "notifications",
            Right::Microphone => "microphone",
            Right::Automation => "automation",
        }
    }

    /// Human-facing name, in the OS's own words where they differ (see the per-OS files).
    pub fn label(self) -> &'static str {
        platform::label(self)
    }

    /// Every right, in a stable order — for a settings page that lists them all.
    pub fn all() -> &'static [Right] {
        &[
            Right::ScreenRecording,
            Right::FullDiskAccess,
            Right::Accessibility,
            Right::Notifications,
            Right::Microphone,
            Right::Automation,
        ]
    }
}

/// What we know about a right. `Undetermined` means the OS gates it but we have not asked
/// (or cannot cheaply tell); `NotApplicable` means this OS has no such gate, and the caller
/// should treat the capability as simply available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    Granted,
    Denied,
    Undetermined,
    NotApplicable,
}

impl Grant {
    /// True when the caller should not bother offering a "Grant…" affordance — either it
    /// already has the right, or this OS never gates it.
    pub fn is_settled(self) -> bool {
        matches!(self, Grant::Granted | Grant::NotApplicable)
    }
}

/// What we currently know about `right` on this OS.
pub fn status(right: Right) -> Grant {
    platform::status(right)
}

/// Take the user to where they can grant `right` — a system prompt where the OS offers one,
/// otherwise the exact Settings pane, so nobody has to be told to "go find it".
///
/// `Ok(())` means we got them there, *not* that they granted anything; re-read [`status`]
/// afterwards. `Err` on an OS with no such gate, so a caller that ignores `NotApplicable`
/// still fails loudly rather than silently doing nothing.
pub fn request(right: Right) -> Result<(), String> {
    if status(right) == Grant::NotApplicable {
        return Err(format!(
            "{} is not a permission on this operating system",
            right.id()
        ));
    }
    platform::request(right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_right_has_a_unique_id_and_a_label() {
        let mut ids: Vec<&str> = Right::all().iter().map(|r| r.id()).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(before, ids.len(), "ids must be unique");
        for r in Right::all() {
            assert!(!r.label().is_empty(), "{:?}", r);
            assert!(!r.id().contains(' '), "{:?} ids are slugs", r);
        }
    }

    #[test]
    fn status_never_panics_and_never_claims_a_grant_we_have_not_checked() {
        for r in Right::all() {
            let g = status(*r);
            assert_ne!(
                g,
                Grant::Granted,
                "{:?}: nothing probes for real yet, so a Granted here would be a lie",
                r
            );
        }
    }

    #[test]
    fn a_right_this_os_does_not_gate_refuses_to_be_requested() {
        for r in Right::all() {
            if status(*r) == Grant::NotApplicable {
                let err = request(*r).unwrap_err();
                assert!(err.contains(r.id()), "{err}");
            }
        }
    }

    #[test]
    fn settled_means_no_affordance_is_needed() {
        assert!(Grant::Granted.is_settled());
        assert!(Grant::NotApplicable.is_settled());
        assert!(!Grant::Denied.is_settled());
        assert!(!Grant::Undetermined.is_settled());
    }
}
