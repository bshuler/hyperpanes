//! The OS-rights seam: what Hyperpanes may need permission for, whether it has it, and
//! how to take the user to the place where they can grant it.
//!
//! Nothing in the app needs any of these on day one. The point of landing the seam early
//! is that the day a feature does need one — a screen-capture pane, a tool that reads
//! `~/Library`, a global hotkey — the ask is a two-line call rather than a per-OS research
//! project bolted on under deadline.
//!
//! Deliberately conservative: [`status`] answers `NotApplicable` where the OS has no such
//! concept, a real answer where the OS can be asked without side effects, and `Undetermined`
//! everywhere else — never a guess. A *wrong* "granted" is worse than an honest "don't know",
//! and it costs the caller nothing to be told "don't know", because the fallback for
//! `Undetermined` and `Denied` is the same: offer [`request`].
//!
//! macOS is the only one of the three with a real per-app permission database, and it is the
//! only one that answers `Granted` for anything: `CGPreflightScreenCaptureAccess`,
//! `AXIsProcessTrusted`, and an open-and-drop of a TCC-protected file. Windows and Linux gate
//! almost none of this for a desktop app, and inventing a probe there would only manufacture
//! a "denied" nobody can act on.
//!
//! Two ways to ask, and they are not interchangeable. [`prompt`] raises the OS's own one-shot
//! consent dialog and belongs at the moment the feature needs the right; [`request`] takes
//! the user to the Settings pane and is the recovery path for when the dialog is spent.
//!
//! On macOS a grant is keyed to the *signing identity*, which is why `packaging/macos/
//! bundle.sh` signs with a Developer ID when one is available: an ad-hoc signature changes
//! on every rebuild and takes every grant with it.
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

/// Raise the OS's own consent dialog for `right` and report the answer.
///
/// For the caller this is the *first* thing to try, and only from the feature that needs the
/// right, at the moment it needs it — macOS shows each of these dialogs once ever, so one
/// raised from a settings list is one the feature will never get. Where the OS has no such
/// dialog this is just [`status`], which is why a caller can reach for it unconditionally.
pub fn prompt(right: Right) -> Grant {
    platform::prompt(right)
}

/// Take the user to where they can grant `right` — the exact Settings pane, so nobody has to
/// be told to "go find it". This is the path for after [`prompt`] has been spent.
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
    fn status_never_panics_and_settles_on_one_answer() {
        // Every answer is legal now that three of the macOS probes are real — what is not
        // legal is a status that changes under repeated reads, which would mean the probe is
        // observing something other than the grant.
        for r in Right::all() {
            let g = status(*r);
            assert_eq!(g, status(*r), "{:?} must not flap", r);
        }
    }

    #[test]
    fn a_right_this_os_never_gates_cannot_be_prompted_into_existence() {
        // `prompt` is safe to call blind, so it must not invent a grant on an OS that has no
        // dialog to raise — it degrades to `status` and keeps `NotApplicable` intact.
        for r in Right::all() {
            if status(*r) == Grant::NotApplicable {
                assert_eq!(prompt(*r), Grant::NotApplicable, "{:?}", r);
            }
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
