//! The always-on **Hyperpane** tab's working directory.
//!
//! The tab runs the user's coding CLI in [`paths::hyperpane_dir`], and everything that agent
//! knows about driving this app — the skill, its reference, the README a human reads — is a
//! file in that directory. Those files ship with the binary under
//! `resources/claude/hyperpane/`; this module copies them out to the durable location on every
//! start, so an upgraded app teaches its agent the new verbs without the user doing anything.
//!
//! The copy is one-directional and additive: files the app ships are overwritten, and nothing
//! else in the directory is ever touched. That split is the whole contract — the agent keeps
//! notes there and the user drops files in, and neither can be clobbered by an upgrade, while
//! a locally-edited `SKILL.md` is app-owned and *will* be replaced.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::persistence::paths;

/// The shipped copy of the Hyperpane directory, or `None` when the app is running from a tree
/// that doesn't carry it.
///
/// Same packaged layouts [`crate::shell_integration::shell_integration_dir`] handles —
/// exe-relative (which also covers a dev `cargo run`, since `build.rs` stages resources next to
/// the binary), the macOS `.app` `Contents/Resources`, and the FHS `share`/`lib` prefixes.
pub fn source_dir() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))?;
    let rel = Path::new("resources").join("claude").join("hyperpane");
    let mut candidates = vec![exe_dir.join(&rel)];
    if let Some(prefix) = exe_dir.parent() {
        candidates.push(prefix.join("Resources").join("claude").join("hyperpane"));
        candidates.push(prefix.join("share").join("hyperpanes").join(&rel));
        candidates.push(prefix.join("lib").join("hyperpanes").join(&rel));
    }
    candidates.into_iter().find(|c| c.is_dir())
}

/// Refresh [`paths::hyperpane_dir`] from the shipped tree and return it.
///
/// The directory is created even when nothing ships (a stripped build, a dev binary run from a
/// tree without its resources): the tab still needs a cwd, and an empty one is a working — if
/// unhelpful — starting point, which is strictly better than the tab failing to open.
pub fn materialize() -> io::Result<PathBuf> {
    let dest = paths::hyperpane_dir();
    fs::create_dir_all(&dest)?;
    if let Some(src) = source_dir() {
        copy_over(&src, &dest)?;
    }
    Ok(dest)
}

/// Recursively copy `src` onto `dest`, overwriting collisions and leaving everything else in
/// `dest` alone. Errors on individual entries are skipped rather than aborting the walk: a
/// single unreadable file should not cost the agent its whole skill set.
fn copy_over(src: &Path, dest: &Path) -> io::Result<()> {
    for entry in fs::read_dir(src)? {
        let Ok(entry) = entry else { continue };
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            if fs::create_dir_all(&to).is_ok() {
                let _ = copy_over(&from, &to);
            }
        } else {
            let _ = fs::copy(&from, &to);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_files_are_refreshed_and_local_ones_survive() {
        let tmp = std::env::temp_dir().join(format!("hp-materialize-{}", std::process::id()));
        let src = tmp.join("src");
        let dest = tmp.join("dest");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(src.join(".claude/skills/hyperpanes")).unwrap();
        fs::write(src.join(".claude/skills/hyperpanes/SKILL.md"), "new").unwrap();
        fs::create_dir_all(dest.join(".claude/skills/hyperpanes")).unwrap();
        fs::write(dest.join(".claude/skills/hyperpanes/SKILL.md"), "old").unwrap();
        fs::write(dest.join("notes.md"), "the agent's own notes").unwrap();

        copy_over(&src, &dest).unwrap();

        // App-owned file replaced, hidden `.claude/` subtree walked, user file untouched.
        let skill = fs::read_to_string(dest.join(".claude/skills/hyperpanes/SKILL.md")).unwrap();
        assert_eq!(skill, "new");
        assert_eq!(
            fs::read_to_string(dest.join("notes.md")).unwrap(),
            "the agent's own notes"
        );
        let _ = fs::remove_dir_all(&tmp);
    }
}
