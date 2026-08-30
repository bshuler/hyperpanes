//! Windows half of the open seam.
//!
//! URLs and files go through `cmd /C start "" "<target>"` (the shell's own default-handler
//! lookup), folders and reveals go through `explorer`. Every target is passed with
//! `raw_arg` and hand-quoted, because `cmd` re-parses its command line and would otherwise
//! treat `&`, `|`, or `^` inside a query string as a command separator. `super::is_openable_url`
//! rejects a URL containing `"`, and `"` is not a legal Windows path character, so the
//! quoting can never be broken out of — the one place that could is guarded below.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use super::BrowserApp;

/// Don't flash a console window (same constant as `paths::NoWindow`).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn detached(cmd: &mut Command) -> Result<(), String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// `cmd /C start "" "<target>"` — the empty string is the window-title argument, without
/// which `start` would consume our quoted target as the title and open nothing.
fn shell_start(target: &str) -> Result<(), String> {
    if target.contains('"') {
        return Err(format!("refusing to open {target:?}: contains a quote"));
    }
    let mut c = Command::new("cmd");
    c.raw_arg("/C")
        .raw_arg("start")
        .raw_arg("\"\"")
        .raw_arg(format!("\"{target}\""));
    detached(&mut c)
}

pub fn open_url(url: &str) -> Result<(), String> {
    shell_start(url)
}

pub fn open_url_with(launcher: &str, url: &str) -> Result<(), String> {
    // The launcher is an absolute .exe path we found ourselves (or the user chose), so it
    // is spawned directly — no shell, nothing to re-parse.
    let mut c = Command::new(launcher);
    c.arg(url);
    detached(&mut c)
}

pub fn open_path(path: &Path) -> Result<(), String> {
    shell_start(&path.to_string_lossy())
}

pub fn reveal_path(path: &Path) -> Result<(), String> {
    let p = path.to_string_lossy().to_string();
    if p.contains('"') {
        return Err(format!("refusing to reveal {p:?}: contains a quote"));
    }
    let mut c = Command::new("explorer");
    if path.is_dir() {
        c.raw_arg(format!("\"{p}\""));
    } else {
        // `/select,` and its argument are one token to explorer.
        c.raw_arg(format!("/select,\"{p}\""));
    }
    // explorer exits non-zero even on success; we never wait on it, so it doesn't matter.
    detached(&mut c)
}

/// (our id, display name, [(env var naming a root, path under it)])
const KNOWN: &[(&str, &str, &[(&str, &str)])] = &[
    (
        "edge",
        "Microsoft Edge",
        &[
            (
                "ProgramFiles(x86)",
                r"Microsoft\Edge\Application\msedge.exe",
            ),
            ("ProgramFiles", r"Microsoft\Edge\Application\msedge.exe"),
        ],
    ),
    (
        "chrome",
        "Google Chrome",
        &[
            ("ProgramFiles", r"Google\Chrome\Application\chrome.exe"),
            ("ProgramFiles(x86)", r"Google\Chrome\Application\chrome.exe"),
            ("LOCALAPPDATA", r"Google\Chrome\Application\chrome.exe"),
        ],
    ),
    (
        "firefox",
        "Firefox",
        &[
            ("ProgramFiles", r"Mozilla Firefox\firefox.exe"),
            ("ProgramFiles(x86)", r"Mozilla Firefox\firefox.exe"),
        ],
    ),
    (
        "brave",
        "Brave",
        &[
            (
                "ProgramFiles",
                r"BraveSoftware\Brave-Browser\Application\brave.exe",
            ),
            (
                "ProgramFiles(x86)",
                r"BraveSoftware\Brave-Browser\Application\brave.exe",
            ),
            (
                "LOCALAPPDATA",
                r"BraveSoftware\Brave-Browser\Application\brave.exe",
            ),
        ],
    ),
    (
        "vivaldi",
        "Vivaldi",
        &[
            ("LOCALAPPDATA", r"Vivaldi\Application\vivaldi.exe"),
            ("ProgramFiles", r"Vivaldi\Application\vivaldi.exe"),
        ],
    ),
    (
        "opera",
        "Opera",
        &[("LOCALAPPDATA", r"Programs\Opera\opera.exe")],
    ),
    (
        "chromium",
        "Chromium",
        &[("LOCALAPPDATA", r"Chromium\Application\chrome.exe")],
    ),
];

pub fn list_browsers() -> Vec<BrowserApp> {
    let mut out = Vec::new();
    for (id, name, candidates) in KNOWN {
        for (root, rel) in *candidates {
            let Ok(base) = std::env::var(root) else {
                continue;
            };
            if base.is_empty() {
                continue;
            }
            let full = Path::new(&base).join(rel);
            if full.is_file() {
                out.push(BrowserApp {
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    launcher: full.to_string_lossy().to_string(),
                });
                break; // first hit wins; one entry per browser
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quoted_target_is_refused_rather_than_shell_injected() {
        let err = shell_start("https://e.com/\" & calc").unwrap_err();
        assert!(err.contains("quote"), "{err}");
    }

    #[test]
    fn every_known_browser_has_at_least_one_candidate() {
        for (id, name, candidates) in KNOWN {
            assert!(!id.is_empty() && !name.is_empty(), "{id}");
            assert!(!candidates.is_empty(), "{id}");
            for (root, rel) in *candidates {
                assert!(!root.is_empty() && rel.ends_with(".exe"), "{id}");
            }
        }
    }
}
