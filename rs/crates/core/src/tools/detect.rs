//! Finding a tool's binary — a pure in-process `PATH` walk, never a subprocess.
//!
//! Two probes already existed when this was written and neither fits: `paths.rs`'s
//! VS Code check shells out to `which`/`where` (slow, and on Windows it needs
//! `CREATE_NO_WINDOW` or it flashes a console), and `speech::engine::on_path` is a
//! clean walk but is `#[cfg(unix)]`-only. This is that walk, made cross-platform.
//!
//! Resolution order is fixed and has **no silent fallback** — [`Resolution::source`]
//! records which of the three answered, so a surprising result is explainable in the
//! settings page rather than mysterious.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::registry::ToolDef;

/// Which step of the resolution order produced a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The human typed or picked this path in settings. Always wins, never second-guessed.
    UserOverride,
    /// Found by walking `PATH`.
    Path,
    /// Found in a well-known install location that is not on `PATH`.
    WellKnown,
}

/// Where a tool's binary is, and how we know.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub path: PathBuf,
    pub source: Source,
}

/// The user's home directory. Cross-platform, unlike the private helper in
/// `persistence::paths` which is `#[cfg(not(windows))]`.
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let var = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let var = std::env::var_os("HOME");
    if let Some(v) = var {
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

/// The suffixes an executable may carry. On Windows a bare `claude` is usually
/// `claude.cmd` (an npm shim) or `claude.exe`, so `PATHEXT` has to be honoured or the
/// walk finds nothing at all.
#[cfg(windows)]
fn exe_suffixes() -> Vec<String> {
    let raw = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let mut v = vec![String::new()]; // an explicit `foo.exe` argument still resolves
    v.extend(
        raw.split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase()),
    );
    v
}

#[cfg(not(windows))]
fn exe_suffixes() -> Vec<String> {
    vec![String::new()]
}

/// Whether `p` is a file we could actually launch. On Unix that means the execute bit
/// is set — a non-executable file with the right name is not the tool.
fn is_executable(p: &Path) -> bool {
    let Ok(md) = std::fs::metadata(p) else {
        return false;
    };
    if !md.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Look for `cmd` in `dir`, trying each executable suffix.
fn in_dir(dir: &Path, cmd: &str) -> Option<PathBuf> {
    for suffix in exe_suffixes() {
        let candidate = if suffix.is_empty() {
            dir.join(cmd)
        } else {
            dir.join(format!("{cmd}{suffix}"))
        };
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// The first directory on `PATH` holding an executable named `cmd`.
pub fn on_path(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| in_dir(&dir, cmd))
}

/// Install locations these tools commonly land in that are *not* always on `PATH` —
/// notably a GUI app's environment, which does not inherit a login shell's `PATH` on
/// macOS and so routinely misses `~/.local/bin` and Homebrew.
fn well_known_dirs() -> Vec<PathBuf> {
    #[allow(unused_mut)]
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = home_dir() {
        #[cfg(windows)]
        {
            dirs.push(home.join("AppData\\Local\\Programs"));
            dirs.push(home.join("AppData\\Roaming\\npm"));
            dirs.push(home.join(".local\\bin"));
        }
        #[cfg(not(windows))]
        {
            dirs.push(home.join(".local/bin"));
            dirs.push(home.join("bin"));
            dirs.push(home.join(".bun/bin"));
            dirs.push(home.join(".cargo/bin"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/home/linuxbrew/.linuxbrew/bin"));
    }
    dirs
}

/// Resolve one tool: user override, then `PATH`, then well-known locations.
///
/// A user override is taken at face value — if the human pointed us at a path, we do
/// not overrule them because the file happens to be missing right now (a network
/// mount, a version manager mid-switch). Callers that care can check whether
/// [`Resolution::path`] exists.
pub fn resolve(tool: &ToolDef, overrides: &BTreeMap<String, String>) -> Option<Resolution> {
    if let Some(p) = overrides.get(tool.id) {
        if !p.is_empty() {
            return Some(Resolution {
                path: PathBuf::from(p),
                source: Source::UserOverride,
            });
        }
    }
    for bin in tool.candidate_bins() {
        if let Some(path) = on_path(bin) {
            return Some(Resolution {
                path,
                source: Source::Path,
            });
        }
    }
    for dir in well_known_dirs() {
        for bin in tool.candidate_bins() {
            if let Some(path) = in_dir(&dir, bin) {
                return Some(Resolution {
                    path,
                    source: Source::WellKnown,
                });
            }
        }
    }
    None
}

/// Resolve every tool in the registry, keyed by tool id. Tools that are not installed
/// are simply absent from the map.
pub fn resolve_all(overrides: &BTreeMap<String, String>) -> BTreeMap<&'static str, Resolution> {
    super::registry::TOOLS
        .iter()
        .filter_map(|t| resolve(t, overrides).map(|r| (t.id, r)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry;

    #[test]
    fn user_override_wins_and_is_taken_at_face_value() {
        let mut ov = BTreeMap::new();
        ov.insert("claude".to_string(), "/nowhere/at/all/claude".to_string());
        let t = registry::by_id("claude").unwrap();
        let r = resolve(t, &ov).unwrap();
        assert_eq!(r.source, Source::UserOverride);
        assert_eq!(r.path, PathBuf::from("/nowhere/at/all/claude"));
    }

    #[test]
    fn an_empty_override_is_not_an_override() {
        let mut ov = BTreeMap::new();
        ov.insert("claude".to_string(), String::new());
        let t = registry::by_id("claude").unwrap();
        // Falls through to PATH/well-known, which may or may not find anything on the
        // machine running the test; what matters is it is never called an override.
        assert!(resolve(t, &ov).map(|r| r.source) != Some(Source::UserOverride));
    }

    #[test]
    fn a_directory_is_not_an_executable() {
        assert!(!is_executable(&std::env::temp_dir()));
    }

    #[test]
    fn resolving_everything_never_panics() {
        let _ = resolve_all(&BTreeMap::new());
    }
}
