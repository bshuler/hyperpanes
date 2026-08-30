//! The OS-open seam: one place that hands a path, a folder, or a URL to the operating
//! system, and one place that knows which browsers are installed.
//!
//! Before this module the same launch logic existed three times — `paths::os_open`,
//! the terminal-widget's private `open_url`, and the `RevealPaneCwd` arm in the app's
//! command dispatch — and the three had drifted: the third branched `#[cfg(windows)]`
//! to `explorer` and `xdg-open` otherwise, so "Open Folder" was silently dead on macOS
//! (there is no `xdg-open` there). Folding them together fixes that by construction.
//!
//! Shape follows `docs/ports-seams.md`: a shared, testable front half here, and a
//! `#[path]`-selected `platform` module for the half that actually spawns. Three files
//! rather than core's usual windows/unix split, because macOS (`open`) and Linux
//! (`xdg-open`) genuinely differ — a `#[cfg(unix)]` file would have to re-branch inside.
//!
//! Owned by track `tool-panes` (Wave 0).

use std::path::Path;

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(not(any(windows, target_os = "macos")))]
#[path = "linux.rs"]
mod platform;

/// A browser we found installed, in the form the OS wants it launched.
///
/// `launcher` is deliberately opaque and per-OS — a bundle id on macOS, an absolute
/// `.exe` path on Windows, an absolute binary path on Linux. Callers persist `id`
/// (stable, ours) and re-resolve the launcher on each run, so a moved or upgraded
/// install doesn't strand a saved preference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserApp {
    pub id: String,
    pub name: String,
    pub launcher: String,
}

/// URL schemes we're willing to hand to the OS. Everything else is refused: the OS
/// handler for an arbitrary scheme is an arbitrary program, and the URLs reaching here
/// come from terminal output — i.e. from whatever the pane's process chose to print.
pub const ALLOWED_SCHEMES: &[&str] = &["http", "https", "mailto"];

/// True when `url` is safe to pass to the OS handler: an allowed scheme, no control
/// characters or whitespace (which could split a command line), and no `"` (which would
/// break out of the quoting `windows.rs` relies on — a legal URL never contains one).
pub fn is_openable_url(url: &str) -> bool {
    if url.is_empty() || url.len() > 8192 {
        return false;
    }
    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '"')
    {
        return false;
    }
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    let scheme = scheme.to_ascii_lowercase();
    ALLOWED_SCHEMES.contains(&scheme.as_str())
}

/// Open a URL in the user's default browser. `Err` carries the spawn failure; a refused
/// URL reports as `Err` too, naming the reason, so callers can surface it rather than
/// silently doing nothing.
pub fn open_url(url: &str) -> Result<(), String> {
    if !is_openable_url(url) {
        return Err(format!("refusing to open {url:?}: not an http/https/mailto URL"));
    }
    platform::open_url(url)
}

/// Open a URL in one specific browser (the `launcher` from a [`BrowserApp`]), rather
/// than the OS default. Same refusal rules as [`open_url`].
pub fn open_url_with(launcher: &str, url: &str) -> Result<(), String> {
    if !is_openable_url(url) {
        return Err(format!("refusing to open {url:?}: not an http/https/mailto URL"));
    }
    if launcher.trim().is_empty() {
        return Err("no browser launcher given".to_string());
    }
    platform::open_url_with(launcher, url)
}

/// Hand a file or folder to its default OS handler — a folder opens in the file manager,
/// a file opens in whatever owns its type.
///
/// Note this does *not* screen executable extensions; `paths::open_os_default` still owns
/// that policy for clicked terminal tokens, because it is the caller that knows the path
/// came from untrusted output.
pub fn open_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("empty path".to_string());
    }
    platform::open_path(path)
}

/// Show a path in the file manager — selected inside its parent when it is a file,
/// opened when it is a folder.
pub fn reveal_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("empty path".to_string());
    }
    platform::reveal_path(path)
}

/// Every browser we can find installed, in a stable order (the OS's own default handler
/// is not included — that is [`open_url`], and it is offered separately in the UI).
pub fn list_browsers() -> Vec<BrowserApp> {
    platform::list_browsers()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_urls_are_openable() {
        assert!(is_openable_url("https://example.com"));
        assert!(is_openable_url("http://localhost:3000/a?b=1&c=2#d"));
        assert!(is_openable_url("HTTPS://EXAMPLE.COM"));
        assert!(is_openable_url("mailto:someone@example.com"));
    }

    #[test]
    fn a_scheme_we_do_not_allow_is_refused() {
        // `file:` and custom schemes route to arbitrary local handlers.
        assert!(!is_openable_url("file:///etc/passwd"));
        assert!(!is_openable_url("javascript:alert(1)"));
        assert!(!is_openable_url("vscode://foo"));
        assert!(!is_openable_url("example.com")); // no scheme at all
        assert!(!is_openable_url("https:")); // scheme, nothing after it
    }

    #[test]
    fn command_splitting_characters_are_refused() {
        // The Windows path quotes the URL into a `cmd` line; a `"` would break out of it,
        // and whitespace/controls could split the argument on any OS.
        assert!(!is_openable_url("https://e.com/\" & calc"));
        assert!(!is_openable_url("https://e.com/a b"));
        assert!(!is_openable_url("https://e.com/\nhttps://evil"));
        assert!(!is_openable_url(""));
    }

    #[test]
    fn a_refused_url_never_reaches_the_platform() {
        // No spawn happens, and the caller gets a reason rather than silence.
        let err = open_url("javascript:alert(1)").unwrap_err();
        assert!(err.contains("refusing"), "{err}");
        let err = open_url_with("Safari", "file:///etc/passwd").unwrap_err();
        assert!(err.contains("refusing"), "{err}");
    }

    #[test]
    fn an_empty_target_is_an_error_not_a_spawn() {
        assert!(open_path(Path::new("")).is_err());
        assert!(reveal_path(Path::new("")).is_err());
        assert!(open_url_with("  ", "https://example.com").is_err());
    }

    #[test]
    fn listing_browsers_never_panics_and_is_deduplicated() {
        let found = list_browsers();
        let mut ids: Vec<&str> = found.iter().map(|b| b.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "browser ids must be unique");
        for b in &found {
            assert!(!b.name.is_empty(), "{b:?}");
            assert!(!b.launcher.is_empty(), "{b:?}");
        }
    }
}
