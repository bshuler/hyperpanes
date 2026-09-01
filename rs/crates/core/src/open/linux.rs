//! Linux (and other non-macOS unix) half of the open seam.
//!
//! `xdg-open` is the portable entry point, with `gio open` as the fallback for the
//! desktops that ship GLib but not xdg-utils. Revealing a file has no xdg equivalent, so
//! it goes through the `org.freedesktop.FileManager1` D-Bus interface when `dbus-send` is
//! present, and degrades to opening the containing folder when it isn't.

use std::path::{Path, PathBuf};
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

pub fn open_path_with(launcher: &str, path: &Path) -> Result<(), String> {
    let arg = safe_arg(path);
    // The launcher is the absolute path of a desktop entry when we found one, and an
    // absolute binary otherwise. `gio launch` takes the file; `gtk-launch` wants the id,
    // which is the basename — one of the two is present on every desktop that has either.
    if launcher.ends_with(".desktop") {
        let first = detached(Command::new("gio").args(["launch", launcher, &arg]));
        if first.is_ok() {
            return first;
        }
        let id = Path::new(launcher)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| launcher.to_string());
        return detached(Command::new("gtk-launch").args([&id, &arg]));
    }
    detached(Command::new(launcher).arg(arg))
}

// ---- "Open With": which applications declare they can open this kind of file ----

/// The `applications` directories, in the order XDG searches them: the user's own first,
/// so a local override shadows the system copy of the same entry.
fn desktop_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = std::env::var("HOME").unwrap_or_default();
    match std::env::var("XDG_DATA_HOME") {
        Ok(d) if !d.is_empty() => dirs.push(PathBuf::from(d)),
        _ if !home.is_empty() => dirs.push(PathBuf::from(&home).join(".local/share")),
        _ => {}
    }
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    dirs.extend(
        system
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from),
    );
    dirs.into_iter().map(|d| d.join("applications")).collect()
}

/// The mime types `*.ext` maps to, read straight out of shared-mime-info's `globs` table.
///
/// A `xdg-mime query filetype` would need a file that exists, and the answer for a file
/// that does exist is content-sniffed — an empty `.py` comes back `text/plain` and the
/// menu loses every Python editor. The glob table is what we actually mean: what this
/// *name* is.
fn mimes_for(ext: &str) -> Vec<String> {
    let pattern = format!("*.{ext}");
    let mut out = Vec::new();
    for dir in ["/usr/share/mime", "/usr/local/share/mime"] {
        let Ok(body) = std::fs::read_to_string(Path::new(dir).join("globs")) else {
            continue;
        };
        for line in body.lines().filter(|l| !l.starts_with('#')) {
            if let Some((mime, glob)) = line.split_once(':') {
                if glob.eq_ignore_ascii_case(&pattern) && !out.iter().any(|m| m == mime) {
                    out.push(mime.to_string());
                }
            }
        }
    }
    out
}

/// One desktop entry's `Name` and `MimeType`, or `None` when it is not a launchable
/// application (hidden, or no types at all).
fn desktop_entry(file: &Path) -> Option<(String, Vec<String>)> {
    let body = std::fs::read_to_string(file).ok()?;
    let mut name = None;
    let mut mimes = Vec::new();
    // Only the `[Desktop Entry]` group counts; the action groups below it carry their own
    // `Name=` and would otherwise overwrite the application's.
    for line in body
        .lines()
        .skip_while(|l| l.trim() != "[Desktop Entry]")
        .skip(1)
        .take_while(|l| !l.trim_start().starts_with('['))
    {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Name" if name.is_none() => name = Some(value.trim().to_string()),
            "MimeType" => {
                mimes = value
                    .split(';')
                    .filter(|m| !m.is_empty())
                    .map(|m| m.trim().to_ascii_lowercase())
                    .collect()
            }
            "NoDisplay" | "Hidden" if value.trim().eq_ignore_ascii_case("true") => return None,
            "Type" if value.trim() != "Application" => return None,
            _ => {}
        }
    }
    let name = name?;
    (!mimes.is_empty()).then_some((name, mimes))
}

pub fn handlers_for_ext(ext: &str) -> Vec<super::HandlerApp> {
    let mimes = mimes_for(ext);
    if mimes.is_empty() {
        return Vec::new();
    }
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<super::HandlerApp> = Vec::new();
    for dir in desktop_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let file = e.path();
            if !file.extension().is_some_and(|x| x == "desktop") {
                continue;
            }
            let id = file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if !seen.insert(id.clone()) {
                continue; // an earlier, higher-priority directory already answered
            }
            let Some((name, declared)) = desktop_entry(&file) else {
                continue;
            };
            if !declared.iter().any(|m| mimes.contains(m)) {
                continue;
            }
            out.push(super::HandlerApp {
                id,
                name,
                launcher: file.to_string_lossy().into_owned(),
            });
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
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
    fn a_desktop_entry_yields_its_name_and_declared_types() {
        let dir = std::env::temp_dir().join("hp-open-linux-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let f = dir.join("editor.desktop");
        std::fs::write(
            &f,
            "[Desktop Entry]\nType=Application\nName=Editor\nMimeType=text/plain;text/x-python;\n\n[Desktop Action new]\nName=New Window\n",
        )
        .expect("write");
        let (name, mimes) = desktop_entry(&f).expect("an application");
        assert_eq!(name, "Editor");
        assert_eq!(mimes, vec!["text/plain", "text/x-python"]);

        // Hidden from the menus is hidden from ours too.
        std::fs::write(
            &f,
            "[Desktop Entry]\nType=Application\nName=Editor\nNoDisplay=true\nMimeType=text/plain;\n",
        )
        .expect("write");
        assert!(desktop_entry(&f).is_none());
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn every_known_browser_names_at_least_one_binary() {
        for (id, name, bins) in KNOWN {
            assert!(!id.is_empty() && !name.is_empty(), "{id}");
            assert!(!bins.is_empty(), "{id}");
        }
    }
}
