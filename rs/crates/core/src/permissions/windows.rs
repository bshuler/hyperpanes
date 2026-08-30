//! Windows half of the rights seam.
//!
//! Windows has no TCC. Screen capture and file access are governed by the user's own token
//! and by UAC at install time, not by a per-app runtime grant, so both answer
//! `NotApplicable` — a caller that checks before capturing must not be left waiting for a
//! grant that will never arrive. What *is* gated lives under Settings › Privacy, reachable
//! through the `ms-settings:` protocol handler.

use std::process::{Command, Stdio};

use super::{Grant, Right};

pub fn label(right: Right) -> &'static str {
    match right {
        Right::ScreenRecording => "Screen recording",
        Right::FullDiskAccess => "File system access",
        Right::Accessibility => "Accessibility",
        Right::Notifications => "Notifications",
        Right::Microphone => "Microphone",
        Right::Automation => "Automation",
    }
}

/// `None` for the rights Windows does not gate.
fn settings_url(right: Right) -> Option<&'static str> {
    match right {
        Right::Notifications => Some("ms-settings:notifications"),
        Right::Microphone => Some("ms-settings:privacy-microphone"),
        // Desktop apps are not gated on these; there is no pane to send anyone to.
        Right::ScreenRecording
        | Right::FullDiskAccess
        | Right::Accessibility
        | Right::Automation => None,
    }
}

pub fn status(right: Right) -> Grant {
    match settings_url(right) {
        Some(_) => Grant::Undetermined,
        None => Grant::NotApplicable,
    }
}

pub fn request(right: Right) -> Result<(), String> {
    let url = settings_url(right)
        .ok_or_else(|| format!("{} is not gated on Windows", right.id()))?;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    use std::os::windows::process::CommandExt;
    Command::new("cmd")
        .raw_arg("/C")
        .raw_arg("start")
        .raw_arg("\"\"")
        .raw_arg(format!("\"{url}\""))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gated_rights_are_exactly_the_ones_with_a_settings_pane() {
        for r in Right::all() {
            let gated = settings_url(*r).is_some();
            assert_eq!(gated, status(*r) == Grant::Undetermined, "{:?}", r);
            assert_eq!(!gated, status(*r) == Grant::NotApplicable, "{:?}", r);
        }
    }
}
