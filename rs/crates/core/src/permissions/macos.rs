//! macOS half of the rights seam. Every one of these is a TCC gate.
//!
//! `request` opens the exact System Settings pane via an `x-apple.systempreferences:` URL.
//! These are compile-time constants, not caller input, which is why they go straight to
//! `/usr/bin/open` instead of through `core::open` (whose scheme allow-list deliberately
//! refuses everything but http/https/mailto).
//!
//! No API-level prompt is triggered here on purpose: on macOS the *first* API call is what
//! raises the one-shot prompt, and once the user dismisses it the OS never asks again — so
//! burning that prompt from a settings page, away from the feature that needs it, would be
//! the worst possible moment to spend it. The feature raises its own prompt; this is the
//! recovery path for when the answer was no.

use std::process::{Command, Stdio};

use super::{Grant, Right};

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

pub fn status(_right: Right) -> Grant {
    // macOS gates all six. Answering honestly until a real probe lands (see mod.rs).
    Grant::Undetermined
}

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
}
