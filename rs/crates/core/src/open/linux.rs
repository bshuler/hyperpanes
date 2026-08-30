//! Linux (and other non-macOS unix) half of the open seam.
//!
//! `xdg-open` is the portable entry point, with `gio open` as the fallback for the
//! desktops that ship GLib but not xdg-utils. Revealing a file has no xdg equivalent, so
//! it goes through the `org.freedesktop.FileManager1` D-Bus interface when `dbus-send` is
//! present, and degrades to opening the containing folder when it isn't.

use std::path::Path;
use std::process::{Command, Stdio};

use super::BrowserApp;
use crate::tools::detect::on_path;

fn detached(cmd: &mut Command) -> Result<(), String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// `xdg-open <target>`, falling back to `gio open <target>` when xdg-utils isn't installed.
fn xdg(target: &str) -> Result<(), String> {
    let first = detached(Command::new("xdg-open").arg(target));
    if first.is_ok() {
        return first;
    }
    detached(Command::new("gio").args(["open", target])).map_err(|gio| {
        format!(
            "xdg-open failed ({}) and gio open failed ({gio})",
            first.unwrap_err()
        )
    })
}

/// A leading `-` would be read as an option by either handler.
fn safe_arg(path: &Path) -> String {
    let s = path.to_string_lossy().to_string();
    if s.starts_with('-') {
        format!("./{s}")
    } else {
        s
    }
}

pub fn open_url(url: &str) -> Result<(), String> {
    xdg(url)
}

pub fn open_url_with(launcher: &str, url: &str) -> Result<(), String> {
    detached(Command::new(launcher).arg(url))
}

pub fn open_path(path: &Path) -> Result<(), String> {
    xdg(&safe_arg(path))
}

pub fn reveal_path(path: &Path) -> Result<(), String> {
    if path.is_dir() {
        return xdg(&safe_arg(path));
    }
    if on_path("dbus-send").is_some() {
        let uri = format!("file://{}", path.to_string_lossy());
        let sent = detached(Command::new("dbus-send").args([
            "--session",
            "--dest=org.freedesktop.FileManager1",
            "--type=method_call",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1.ShowItems",
            &format!("array:string:{uri}"),
            "string:",
        ]));
        if sent.is_ok() {
            return sent;
        }
    }
    // No file manager interface — the containing folder is the honest degradation.
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => xdg(&safe_arg(dir)),
        _ => xdg(&safe_arg(path)),
    }
}

/// (our id, display name, [binaries to look for on PATH, in preference order])
const KNOWN: &[(&str, &str, &[&str])] = &[
    ("firefox", "Firefox", &["firefox", "firefox-esr"]),
    (
        "chrome",
        "Google Chrome",
        &["google-chrome", "google-chrome-stable"],
    ),
    ("chromium", "Chromium", &["chromium", "chromium-browser"]),
    (
        "edge",
        "Microsoft Edge",
        &["microsoft-edge", "microsoft-edge-stable"],
    ),
    ("brave", "Brave", &["brave-browser", "brave"]),
    ("vivaldi", "Vivaldi", &["vivaldi-stable", "vivaldi"]),
    ("opera", "Opera", &["opera"]),
    ("epiphany", "GNOME Web", &["epiphany", "epiphany-browser"]),
    ("falkon", "Falkon", &["falkon"]),
    ("librewolf", "LibreWolf", &["librewolf"]),
];

pub fn list_browsers() -> Vec<BrowserApp> {
    let mut out = Vec::new();
    for (id, name, bins) in KNOWN {
        if let Some(found) = bins.iter().find_map(|b| on_path(b)) {
            out.push(BrowserApp {
                id: (*id).to_string(),
                name: (*name).to_string(),
                launcher: found.to_string_lossy().to_string(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_starting_with_a_dash_is_not_read_as_a_flag() {
        assert_eq!(safe_arg(Path::new("-x")), "./-x");
        assert_eq!(safe_arg(Path::new("/tmp/x")), "/tmp/x");
    }

    #[test]
    fn every_known_browser_names_at_least_one_binary() {
        for (id, name, bins) in KNOWN {
            assert!(!id.is_empty() && !name.is_empty(), "{id}");
            assert!(!bins.is_empty(), "{id}");
        }
    }
}
