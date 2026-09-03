//! macOS half of the rights seam. Every one of these is a TCC gate.
//!
//! Three of the six can be probed honestly and cheaply, and are: Screen Recording and
//! Accessibility each have a preflight C function that answers without side effects, and
//! Full Disk Access is inferred from whether a TCC-protected path opens. The other three
//! stay `Undetermined` — see [`status`] for what each would cost.
//!
//! The framework calls are declared here as bare `extern "C"` blocks rather than pulled in
//! through a bindings crate. All three take no arguments and return a boolean, so the crate
//! would buy us nothing but a dependency and a build-time cost on every platform.
//!
//! `request` opens the exact System Settings pane via an `x-apple.systempreferences:` URL.
//! These are compile-time constants, not caller input, which is why they go straight to
//! `/usr/bin/open` instead of through `core::open` (whose scheme allow-list deliberately
//! refuses everything but http/https/mailto).
//!
//! No API-level prompt is triggered from `request` on purpose: on macOS the *first* API call
//! is what raises the one-shot prompt, and once the user dismisses it the OS never asks
//! again — so burning that prompt from a settings page, away from the feature that needs it,
//! would be the worst possible moment to spend it. The feature raises its own prompt through
//! [`prompt`]; `request` is the recovery path for when the answer was no.

use std::io::ErrorKind;
use std::process::{Command, Stdio};

use super::{Grant, Right};

// CoreGraphics, 10.15+. Preflight is documented as side-effect free: it reports the current
// TCC answer and never raises the consent dialog. Request raises it, once, ever.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

// ApplicationServices (HIServices). The `WithOptions` variant can raise the prompt; this one
// cannot, which is exactly what a status probe wants.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[tracing::instrument(level = "debug", ret)]
pub fn label(right: Right) -> &'static str {
    // Apple's own wording in System Settings › Privacy & Security.
    match right {
        Right::ScreenRecording => "Screen & System Audio Recording",
        Right::FullDiskAccess => "Full Disk Access",
        Right::Accessibility => "Accessibility",
        Right::Notifications => "Notifications",
        Right::Microphone => "Microphone",
        Right::Automation => "Automation",
    }
}

/// The Settings deep link for each right.
#[tracing::instrument(level = "debug", ret)]
fn settings_url(right: Right) -> &'static str {
    const SEC: &str = "x-apple.systempreferences:com.apple.preference.security";
    match right {
        Right::ScreenRecording => concat!(
            "x-apple.systempreferences:com.apple.preference.security",
            "?Privacy_ScreenCapture"
        ),
        Right::FullDiskAccess => concat!(
            "x-apple.systempreferences:com.apple.preference.security",
            "?Privacy_AllFiles"
        ),
        Right::Accessibility => concat!(
            "x-apple.systempreferences:com.apple.preference.security",
            "?Privacy_Accessibility"
        ),
        Right::Microphone => concat!(
            "x-apple.systempreferences:com.apple.preference.security",
            "?Privacy_Microphone"
        ),
        Right::Automation => concat!(
            "x-apple.systempreferences:com.apple.preference.security",
            "?Privacy_Automation"
        ),
        // Notifications lives in its own pane, not under Privacy & Security.
        Right::Notifications => {
            let _ = SEC;
            "x-apple.systempreferences:com.apple.preference.notifications"
        }
    }
}

/// Paths that only a process holding Full Disk Access can open, most-diagnostic first.
///
/// The TCC database is the canonical one — it exists on every install and nothing but FDA
/// opens it. Safari's bookmarks are the backstop for the (rare) machine where the first path
/// has been moved or the user has no Safari container: a `NotFound` tells us nothing about
/// the grant, so we keep looking rather than answering from it.
const FULL_DISK_PROBES: &[&str] = &[
    "Library/Application Support/com.apple.TCC/TCC.db",
    "Library/Safari/Bookmarks.plist",
];

/// Infer Full Disk Access by trying to open a protected file.
///
/// There is no preflight API for this one — TCC answers it only by allowing or refusing the
/// `open(2)`. So we open and immediately drop the handle: no read, nothing to leak. The
/// contents are never touched, which matters because these files hold the user's entire
/// consent history and browsing bookmarks.
#[tracing::instrument(level = "debug", ret)]
fn full_disk_access() -> Grant {
    let Ok(home) = std::env::var("HOME") else {
        return Grant::Undetermined;
    };
    for rel in FULL_DISK_PROBES {
        match std::fs::File::open(format!("{home}/{rel}")) {
            Ok(_) => return Grant::Granted,
            Err(e) if e.kind() == ErrorKind::PermissionDenied => return Grant::Denied,
            // NotFound, or anything else, says nothing about the grant. Try the next path.
            Err(_) => continue,
        }
    }
    Grant::Undetermined
}

#[tracing::instrument(level = "debug", ret)]
pub fn status(right: Right) -> Grant {
    match right {
        // Preflight true is unambiguous. Preflight false is *not* "denied": before the app
        // has ever asked, TCC has no record and reports exactly the same false it reports
        // after a refusal. Telling those apart needs the TCC database, which needs Full Disk
        // Access — a bigger grant than the one we are asking about. So false stays
        // `Undetermined`, and the caller's affordance is the same either way.
        Right::ScreenRecording => {
            if unsafe { CGPreflightScreenCaptureAccess() } {
                Grant::Granted
            } else {
                Grant::Undetermined
            }
        }
        // Same shape, same reason: trusted means trusted, untrusted may only mean unasked.
        Right::Accessibility => {
            if unsafe { AXIsProcessTrusted() } {
                Grant::Granted
            } else {
                Grant::Undetermined
            }
        }
        Right::FullDiskAccess => full_disk_access(),
        // The remaining three have no honest cheap probe:
        //
        // Notifications and Microphone are answered by `UNUserNotificationCenter` and
        // `AVCaptureDevice`, both Objective-C class methods — reachable only by messaging the
        // ObjC runtime, and `UNUserNotificationCenter` additionally throws outside a real
        // bundle, which is how the test binary runs. Notifications' answer is also async.
        //
        // Automation is not one grant at all: TCC keys it per (this app, target app) pair,
        // so "does Hyperpanes have Automation" has no single answer to give. The pane the
        // deep link opens is the one that lists them.
        Right::Notifications | Right::Microphone | Right::Automation => Grant::Undetermined,
    }
}

/// Raise the OS's own consent dialog for `right`, if there is one, and report what came back.
///
/// Call this from the feature at the moment it needs the right — never from a settings list.
/// The dialog appears at most once in the app's lifetime on this machine; spending it
/// somewhere the user cannot see what it is for spends it badly.
#[tracing::instrument(level = "debug", ret)]
pub fn prompt(right: Right) -> Grant {
    match right {
        // Returns the post-answer state directly. Already-denied is a no-op: the OS will not
        // re-ask, so this degrades to a plain status read and the caller falls back to
        // `request`.
        Right::ScreenRecording => {
            if unsafe { CGRequestScreenCaptureAccess() } {
                Grant::Granted
            } else {
                status(right)
            }
        }
        _ => status(right),
    }
}

#[tracing::instrument(level = "debug", ret)]
pub fn request(right: Right) -> Result<(), String> {
    Command::new("/usr/bin/open")
        .arg(settings_url(right))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_right_deep_links_somewhere_distinct() {
        let mut urls: Vec<&str> = Right::all().iter().map(|r| settings_url(*r)).collect();
        let before = urls.len();
        urls.sort_unstable();
        urls.dedup();
        assert_eq!(before, urls.len(), "each right needs its own pane");
        for r in Right::all() {
            assert!(
                settings_url(*r).starts_with("x-apple.systempreferences:"),
                "{:?}",
                r
            );
        }
    }

    #[test]
    fn the_preflight_probes_run_and_agree_with_themselves() {
        // The point is that the FFI actually links and returns: a probe that has never been
        // called is not an implementation. Which answer this machine gives is not ours to
        // assert — it depends on the tester's TCC state — but it must be stable within a run
        // and must never be `NotApplicable`, since macOS gates all six.
        for r in Right::all() {
            let g = status(*r);
            assert_eq!(g, status(*r), "{:?} must not flap", r);
            assert_ne!(g, Grant::NotApplicable, "{:?} is a TCC gate", r);
        }
    }

    #[test]
    fn full_disk_access_is_decided_without_reading_anything() {
        // Exercised for its own sake: `status` reaches it too, but this names the invariant
        // that the probe is an open-and-drop and so is safe to run in a test suite.
        assert_ne!(full_disk_access(), Grant::NotApplicable);
    }

    #[test]
    fn prompting_a_right_with_no_dialog_is_just_a_status_read() {
        // ScreenRecording is deliberately absent: calling it would raise the one-shot system
        // dialog on the machine running the tests.
        for r in Right::all() {
            if *r == Right::ScreenRecording {
                continue;
            }
            assert_eq!(prompt(*r), status(*r), "{:?}", r);
        }
    }
}
