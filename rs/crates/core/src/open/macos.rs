//! macOS half of the open seam: everything routes through `/usr/bin/open`.
//!
//! `open -R` reveals-and-selects in Finder, `open -b <bundle-id>` targets one specific
//! app. Detection is a bundle-directory probe rather than a Launch Services query so it
//! stays a pure filesystem read with no ObjC bridge and no subprocess.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::BrowserApp;

/// Spawn detached — we never wait on the child, and we never want its stdio wired to ours.
fn spawn(args: &[&str]) -> Result<(), String> {
    Command::new("/usr/bin/open")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// `open` reads a leading `-` as a flag. Absolute paths can't start with one; a relative
/// path can, so give it an explicit `./` prefix.
fn safe_arg(path: &Path) -> String {
    let s = path.to_string_lossy().to_string();
    if s.starts_with('-') {
        format!("./{s}")
    } else {
        s
    }
}

pub fn open_url(url: &str) -> Result<(), String> {
    spawn(&[url])
}

pub fn open_url_with(launcher: &str, url: &str) -> Result<(), String> {
    // A launcher with a dot is a bundle id (`com.apple.Safari`); anything else is treated
    // as an application name, which is what a hand-typed override will look like.
    if launcher.contains('.') {
        spawn(&["-b", launcher, url])
    } else {
        spawn(&["-a", launcher, url])
    }
}

pub fn open_path(path: &Path) -> Result<(), String> {
    spawn(&[&safe_arg(path)])
}

pub fn reveal_path(path: &Path) -> Result<(), String> {
    let arg = safe_arg(path);
    // A folder is what the user asked to open; a file gets selected inside its parent.
    if path.is_dir() {
        spawn(&[&arg])
    } else {
        spawn(&["-R", &arg])
    }
}

/// (our id, display name, bundle id, `.app` directory name)
const KNOWN: &[(&str, &str, &str, &str)] = &[
    ("safari", "Safari", "com.apple.Safari", "Safari.app"),
    (
        "chrome",
        "Google Chrome",
        "com.google.Chrome",
        "Google Chrome.app",
    ),
    ("firefox", "Firefox", "org.mozilla.firefox", "Firefox.app"),
    (
        "edge",
        "Microsoft Edge",
        "com.microsoft.edgemac",
        "Microsoft Edge.app",
    ),
    (
        "brave",
        "Brave",
        "com.brave.Browser",
        "Brave Browser.app",
    ),
    (
        "arc",
        "Arc",
        "company.thebrowser.Browser",
        "Arc.app",
    ),
    (
        "vivaldi",
        "Vivaldi",
        "com.vivaldi.Vivaldi",
        "Vivaldi.app",
    ),
    (
        "opera",
        "Opera",
        "com.operasoftware.Opera",
        "Opera.app",
    ),
    (
        "chromium",
        "Chromium",
        "org.chromium.Chromium",
        "Chromium.app",
    ),
    ("orion", "Orion", "com.kagi.kagimacOS", "Orion.app"),
    ("zen", "Zen Browser", "app.zen-browser.zen", "Zen.app"),
];

fn app_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/Applications")];
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            dirs.push(PathBuf::from(&home).join("Applications"));
        }
    }
    dirs
}

pub fn list_browsers() -> Vec<BrowserApp> {
    let dirs = app_dirs();
    KNOWN
        .iter()
        .filter(|(_, _, _, bundle)| dirs.iter().any(|d| d.join(bundle).is_dir()))
        .map(|(id, name, bundle_id, _)| BrowserApp {
            id: (*id).to_string(),
            name: (*name).to_string(),
            launcher: (*bundle_id).to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_starting_with_a_dash_is_not_read_as_a_flag() {
        assert_eq!(safe_arg(Path::new("-R")), "./-R");
        assert_eq!(safe_arg(Path::new("/tmp/x")), "/tmp/x");
    }

    #[test]
    fn every_known_browser_has_a_bundle_id() {
        for (id, name, bundle, app) in KNOWN {
            assert!(!id.is_empty() && !name.is_empty(), "{id}");
            assert!(bundle.contains('.'), "{bundle} must look like a bundle id");
            assert!(app.ends_with(".app"), "{app}");
        }
    }
}
