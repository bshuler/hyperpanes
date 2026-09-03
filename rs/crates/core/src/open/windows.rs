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

#[tracing::instrument(level = "debug", ret)]
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
#[tracing::instrument(level = "debug", ret)]
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

#[tracing::instrument(level = "debug", ret)]
pub fn open_url(url: &str) -> Result<(), String> {
    shell_start(url)
}

#[tracing::instrument(level = "debug", ret)]
pub fn open_url_with(launcher: &str, url: &str) -> Result<(), String> {
    // The launcher is an absolute .exe path we found ourselves (or the user chose), so it
    // is spawned directly — no shell, nothing to re-parse.
    let mut c = Command::new(launcher);
    c.arg(url);
    detached(&mut c)
}

#[tracing::instrument(level = "debug", ret)]
pub fn open_path(path: &Path) -> Result<(), String> {
    shell_start(&path.to_string_lossy())
}

#[tracing::instrument(level = "debug", ret)]
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

#[tracing::instrument(level = "debug", ret)]
pub fn open_path_with(launcher: &str, path: &Path) -> Result<(), String> {
    // The launcher is an absolute .exe path we read out of the registry ourselves, so it
    // is spawned directly — no shell, nothing to re-parse.
    let mut c = Command::new(launcher);
    c.arg(path);
    detached(&mut c)
}

// ---- "Open With": which applications declare they can open this kind of file ----

/// `reg query <key>`, as lines. `reg` is the supported command-line reader for the
/// registry and ships with every Windows; going through it keeps this crate free of a
/// registry binding it would otherwise need on one platform only.
#[tracing::instrument(level = "debug", ret)]
fn reg(args: &[&str]) -> Option<String> {
    let out = Command::new("reg")
        .arg("query")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.len() > (1 << 20) {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A value line is `<indent><name><spaces><TYPE><spaces><data>`. Returns (name, data).
#[tracing::instrument(level = "debug", ret)]
fn reg_value(line: &str) -> Option<(String, String)> {
    let mut parts = line.trim().splitn(3, "    ").map(str::trim);
    let name = parts.next()?.to_string();
    let ty = parts.next()?;
    if !ty.starts_with("REG_") {
        return None;
    }
    Some((name, parts.next().unwrap_or_default().to_string()))
}

/// The executable out of a `shell\open\command` template: `"C:\...\x.exe" "%1"` or
/// `C:\...\x.exe %1`. Everything after it is the argument template, which we replace with
/// the real path rather than substituting into.
#[tracing::instrument(level = "debug", ret)]
fn exe_of(command: &str) -> Option<String> {
    let c = command.trim();
    let exe = if let Some(rest) = c.strip_prefix('"') {
        rest.split_once('"').map(|(e, _)| e)?
    } else {
        c.split_whitespace().next()?
    };
    Path::new(exe).is_file().then(|| exe.to_string())
}

/// The friendly name a progid publishes, falling back to the executable's own file name.
#[tracing::instrument(level = "debug", ret)]
fn progid_name(progid: &str, exe: &str) -> String {
    let key = format!(r"HKCR\{progid}");
    let named = reg(&[&key]).and_then(|body| {
        body.lines().find_map(|l| match reg_value(l) {
            Some((n, data)) if n == "(Default)" && !data.is_empty() => Some(data),
            _ => None,
        })
    });
    named.unwrap_or_else(|| {
        Path::new(exe)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| progid.to_string())
    })
}

#[tracing::instrument(level = "debug", ret)]
pub fn handlers_for_ext(ext: &str) -> Vec<super::HandlerApp> {
    // The progids registered for the extension: the ones the user has picked from the
    // Open With dialog (per-user) and the ones installers declared (per-machine).
    let mut progids: Vec<String> = Vec::new();
    let user = format!(
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.{ext}\OpenWithProgids"
    );
    for key in [user, format!(r"HKCR\.{ext}\OpenWithProgids")] {
        let Some(body) = reg(&[&key]) else {
            continue;
        };
        for line in body.lines() {
            if let Some((name, _)) = reg_value(line) {
                if name != "(Default)" && !progids.iter().any(|p| *p == name) {
                    progids.push(name);
                }
            }
        }
    }
    // The type's own default handler, which is not repeated in OpenWithProgids.
    if let Some(body) = reg(&[&format!(r"HKCR\.{ext}")]) {
        if let Some(d) = body.lines().find_map(|l| match reg_value(l) {
            Some((n, data)) if n == "(Default)" && !data.is_empty() => Some(data),
            _ => None,
        }) {
            if !progids.iter().any(|p| *p == d) {
                progids.insert(0, d);
            }
        }
    }

    let mut out: Vec<super::HandlerApp> = Vec::new();
    for progid in progids {
        let Some(body) = reg(&[&format!(r"HKCR\{progid}\shell\open\command")]) else {
            continue;
        };
        let Some(cmd) = body.lines().find_map(|l| match reg_value(l) {
            Some((n, data)) if n == "(Default)" => Some(data),
            _ => None,
        }) else {
            continue;
        };
        let Some(exe) = exe_of(&cmd) else {
            continue;
        };
        if out.iter().any(|h| h.launcher.eq_ignore_ascii_case(&exe)) {
            continue;
        }
        out.push(super::HandlerApp {
            name: progid_name(&progid, &exe),
            id: progid,
            launcher: exe,
        });
    }
    out
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

#[tracing::instrument(level = "debug", ret)]
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
    fn a_command_template_yields_the_executable_and_not_its_arguments() {
        // A path that doesn't exist is no handler, however well-formed the template is.
        assert_eq!(exe_of(r#""C:\nope\x.exe" "%1""#), None);
        let me = std::env::current_exe().expect("this test's own binary");
        let p = me.to_string_lossy().into_owned();
        assert_eq!(
            exe_of(&format!("\"{p}\" \"%1\" %*")).as_deref(),
            Some(&p[..])
        );
        assert_eq!(exe_of(&format!("{p} %1")).as_deref(), Some(&p[..]));
    }

    #[test]
    fn a_registry_value_line_splits_into_its_name_and_data() {
        assert_eq!(
            reg_value("    (Default)    REG_SZ    Python.File"),
            Some(("(Default)".to_string(), "Python.File".to_string()))
        );
        assert_eq!(
            reg_value("    Python.File    REG_NONE"),
            Some(("Python.File".to_string(), String::new()))
        );
        assert_eq!(reg_value("HKEY_CLASSES_ROOT\\.py"), None);
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
