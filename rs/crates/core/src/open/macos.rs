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

pub fn open_path_with(launcher: &str, path: &Path) -> Result<(), String> {
    let arg = safe_arg(path);
    // Same split as `open_url_with`: a dotted launcher is a bundle id, anything else is a
    // plain application name.
    if launcher.contains('.') {
        spawn(&["-b", launcher, &arg])
    } else {
        spawn(&["-a", launcher, &arg])
    }
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
    ("brave", "Brave", "com.brave.Browser", "Brave Browser.app"),
    ("arc", "Arc", "company.thebrowser.Browser", "Arc.app"),
    ("vivaldi", "Vivaldi", "com.vivaldi.Vivaldi", "Vivaldi.app"),
    ("opera", "Opera", "com.operasoftware.Opera", "Opera.app"),
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

// ---- "Open With": which applications declare they can open this kind of file ----

/// One application's document-type declarations, as read from its `Info.plist`.
struct AppTypes {
    name: String,
    bundle_id: String,
    exts: Vec<String>,
    utis: Vec<String>,
}

/// An extension carries no type on its own, so an editor that declares only UTIs —
/// TextEdit, Xcode, most of Apple's own — would never match one. This is the conformance
/// walk Launch Services does, flattened to the family a text file belongs to, and
/// deliberately stopping short of `public.data`/`public.item`/`public.content`: every
/// archiver and hex editor claims those, and "Open With" would become a list of every
/// application installed.
const TEXT_UTIS: &[&str] = &[
    "public.plain-text",
    "public.utf8-plain-text",
    "public.text",
    "public.source-code",
    "public.script",
    "public.shell-script",
];

/// The extensions we are willing to call plain text, and therefore to match against
/// [`TEXT_UTIS`]. A list rather than a heuristic because being wrong in the generous
/// direction offers an application that will show the user mojibake.
const TEXT_EXTS: &[&str] = &[
    "bash",
    "c",
    "cc",
    "cfg",
    "cnf",
    "conf",
    "cpp",
    "cs",
    "css",
    "csv",
    "diff",
    "env",
    "fish",
    "go",
    "gradle",
    "h",
    "hpp",
    "htm",
    "html",
    "ini",
    "java",
    "js",
    "json",
    "jsx",
    "kt",
    "log",
    "lua",
    "m",
    "markdown",
    "md",
    "mm",
    "patch",
    "php",
    "pl",
    "properties",
    "py",
    "r",
    "rb",
    "rs",
    "scss",
    "sh",
    "sql",
    "swift",
    "text",
    "toml",
    "ts",
    "tsv",
    "tsx",
    "txt",
    "xml",
    "yaml",
    "yml",
    "zsh",
];

/// (extension, the type it *is*), for the kinds with a name of their own. Everything in
/// [`TEXT_EXTS`] additionally inherits [`TEXT_UTIS`].
const OWN_UTI: &[(&str, &str)] = &[
    ("c", "public.c-source"),
    ("cpp", "public.c-plus-plus-source"),
    ("css", "public.css"),
    ("csv", "public.comma-separated-values-text"),
    ("h", "public.c-header"),
    ("hpp", "public.c-plus-plus-header"),
    ("htm", "public.html"),
    ("html", "public.html"),
    ("java", "com.sun.java-source"),
    ("js", "com.netscape.javascript-source"),
    ("json", "public.json"),
    ("log", "public.log"),
    ("m", "public.objective-c-source"),
    ("markdown", "net.daringfireball.markdown"),
    ("md", "net.daringfireball.markdown"),
    ("pdf", "com.adobe.pdf"),
    ("php", "public.php-script"),
    ("pl", "public.perl-script"),
    ("py", "public.python-script"),
    ("rb", "public.ruby-script"),
    ("rtf", "public.rtf"),
    ("sh", "public.shell-script"),
    ("swift", "public.swift-source"),
    ("txt", "public.plain-text"),
    ("xml", "public.xml"),
];

/// Every type identifier that answers for `ext`.
fn utis_for(ext: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = OWN_UTI
        .iter()
        .filter(|(e, _)| *e == ext)
        .map(|(_, u)| *u)
        .collect();
    if TEXT_EXTS.contains(&ext) {
        out.extend_from_slice(TEXT_UTIS);
    }
    out
}

/// Every `.app` bundle worth reading: the application roots, plus one level of folders
/// inside them — `/Applications/Utilities` is where Terminal lives, and people file their
/// own applications into folders of their own.
fn app_bundles() -> Vec<PathBuf> {
    let mut roots = app_dirs();
    roots.push(PathBuf::from("/System/Applications"));
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let is_app = p.extension().is_some_and(|x| x == "app");
            if is_app {
                out.push(p);
            } else if p.is_dir() {
                let Ok(inner) = std::fs::read_dir(&p) else {
                    continue;
                };
                out.extend(
                    inner
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|x| x == "app")),
                );
            }
        }
    }
    out
}

/// `Info.plist` is usually a *binary* plist, so it is read through `plutil` rather than
/// parsed here — the format converter ships with the OS and we already depend on
/// `serde_json` for the far side. One short-lived subprocess per application, once per
/// run: a hundred-odd applications come in well under a second, measured.
fn plist_json(app: &Path) -> Option<serde_json::Value> {
    let out = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(app.join("Contents/Info.plist"))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.len() > (4 << 20) {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn strings_at(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

fn read_app(app: &Path) -> Option<AppTypes> {
    let v = plist_json(app)?;
    let bundle_id = v.get("CFBundleIdentifier")?.as_str()?.to_string();
    if !bundle_id.contains('.') {
        return None;
    }
    let name = v
        .get("CFBundleDisplayName")
        .or_else(|| v.get("CFBundleName"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .or_else(|| app.file_stem().map(|s| s.to_string_lossy().into_owned()))?;
    let mut exts = Vec::new();
    let mut utis = Vec::new();
    for t in v
        .get("CFBundleDocumentTypes")
        .and_then(|x| x.as_array())
        .into_iter()
        .flatten()
    {
        // Role "None" is a declaration that the application *cannot* open the type — it
        // is claiming the icon, or the type's definition, and nothing more.
        if t.get("CFBundleTypeRole")
            .and_then(|r| r.as_str())
            .is_some_and(|r| r.eq_ignore_ascii_case("None"))
        {
            continue;
        }
        // `*` means "anything", which is a claim, not an answer.
        exts.extend(
            strings_at(t, "CFBundleTypeExtensions")
                .into_iter()
                .filter(|e| e != "*"),
        );
        utis.extend(strings_at(t, "LSItemContentTypes"));
    }
    if exts.is_empty() && utis.is_empty() {
        return None;
    }
    Some(AppTypes {
        name,
        bundle_id,
        exts,
        utis,
    })
}

/// Built once per run. A newly installed application therefore doesn't appear until the
/// next launch — the price of never paying the scan twice, and the scan is the only thing
/// here that costs anything.
fn index() -> &'static [AppTypes] {
    static INDEX: std::sync::OnceLock<Vec<AppTypes>> = std::sync::OnceLock::new();
    INDEX.get_or_init(|| {
        let mut seen = std::collections::HashSet::new();
        let mut apps: Vec<AppTypes> = app_bundles()
            .iter()
            .filter_map(|p| read_app(p))
            .filter(|a| seen.insert(a.bundle_id.clone()))
            .collect();
        apps.sort_by_cached_key(|a| a.name.to_lowercase());
        apps
    })
}

pub fn handlers_for_ext(ext: &str) -> Vec<super::HandlerApp> {
    let utis = utis_for(ext);
    index()
        .iter()
        .filter(|a| {
            a.exts.iter().any(|e| e == ext) || a.utis.iter().any(|u| utis.contains(&u.as_str()))
        })
        .map(|a| super::HandlerApp {
            id: a.bundle_id.clone(),
            name: a.name.clone(),
            launcher: a.bundle_id.clone(),
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
    fn a_source_extension_answers_with_its_own_type_and_the_text_family() {
        let py = utis_for("py");
        assert!(py.contains(&"public.python-script"));
        assert!(py.contains(&"public.plain-text"));
        // Not a text file, so it inherits nothing generic.
        let pdf = utis_for("pdf");
        assert_eq!(pdf, vec!["com.adobe.pdf"]);
        assert!(utis_for("zzz").is_empty());
    }

    #[test]
    fn every_named_type_is_a_type_identifier() {
        for (ext, uti) in OWN_UTI {
            assert!(!ext.is_empty() && uti.contains('.'), "{ext} -> {uti}");
        }
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
