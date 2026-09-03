//! Claude Desktop's own copy of a Claude Code conversation, and the deep link that raises it.
//!
//! The desktop app hosts the same `claude` binary the CLI does, and when it does it keeps a
//! record of the conversation under its own id. That record is the only bridge between the
//! two worlds: the transcript store (`~/.claude/projects/**/<uuid>.jsonl`) knows a session by
//! its **CLI uuid**, while every desktop route — including the deep link that brings a
//! conversation to the front — knows it by a **`local_…` id** the app minted for itself.
//!
//! So this module answers exactly one question, and answers it from disk rather than by
//! asking the app: *given a CLI session uuid, does Claude Desktop hold that conversation,
//! and under what id?* The left panel uses the answer to decide that a click on a resumable
//! row should raise the desktop app instead of starting a second copy of the conversation in
//! a pane — two live `claude --resume` processes on one transcript is the thing worth
//! avoiding here.
//!
//! What this does **not** claim: that the desktop app is running, or that the conversation is
//! on screen in it. The store is a record of what the app knows about, which is the strongest
//! signal available without talking to the app itself. Opening the deep link is harmless when
//! the app is closed — it launches it — so the weaker claim is enough to act on.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The directory Claude Desktop keeps its session records in, when this platform has one.
///
/// Per-platform because the app follows each platform's own convention; a platform we have
/// not confirmed returns `None` rather than a guess, and every caller degrades to "not in
/// the desktop app", which is the same answer an empty store gives.
#[tracing::instrument(level = "debug", ret)]
pub fn store_dir() -> Option<PathBuf> {
    let rel = "Claude/claude-code-sessions";
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library/Application Support")
                .join(rel),
        )
    }
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("APPDATA")?;
        Some(PathBuf::from(base).join(rel))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join(rel))
    }
}

/// A record is JSON of a known small shape (a few KB). Anything past this is not one of
/// ours — read the bound, not the file, so a stray huge file in the store cannot stall the
/// scan thread.
const MAX_RECORD_BYTES: u64 = 1 << 20;

/// The store is `<root>/<a>/<b>/local_<uuid>.json` — two levels of sharding and no deeper.
/// Fixed rather than a recursive walk so a symlink loop in the store cannot spin the scan.
const SHARD_DEPTH: usize = 2;

/// `cli uuid -> local_ id`, for every conversation Claude Desktop holds and has not archived.
///
/// Archived records are dropped: the human removed the conversation from the app's list, so
/// raising the app onto it is not what a click means any more — starting it in a pane is.
#[tracing::instrument(level = "debug", ret)]
pub fn scan() -> HashMap<String, String> {
    match store_dir() {
        Some(root) => scan_in(&root),
        None => HashMap::new(),
    }
}

/// [`scan`] against an explicit root, so the mapping can be tested without a Claude install.
#[tracing::instrument(level = "debug", ret)]
pub fn scan_in(root: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    visit(root, 0, &mut out);
    out
}

#[tracing::instrument(level = "debug", ret)]
fn visit(dir: &Path, depth: usize, out: &mut HashMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if depth < SHARD_DEPTH {
                visit(&path, depth + 1, out);
            }
            continue;
        }
        // Only the shard leaves hold records; a file at the root is not one.
        if depth != SHARD_DEPTH {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("local_") || !name.ends_with(".json") {
            continue;
        }
        if entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_RECORD_BYTES {
            continue;
        }
        if let Some((cli, local)) = read_record(&path) {
            out.insert(cli, local);
        }
    }
}

/// One record's `cliSessionId -> sessionId`, or `None` when it is archived, unparseable, or
/// carries an id we would not put in a URL.
#[tracing::instrument(level = "debug", ret)]
fn read_record(path: &Path) -> Option<(String, String)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v.get("isArchived")
        .and_then(|a| a.as_bool())
        .unwrap_or(false)
    {
        return None;
    }
    let local = v.get("sessionId")?.as_str()?.to_string();
    let cli = v.get("cliSessionId")?.as_str()?.to_string();
    if !valid_local_id(&local) || cli.is_empty() {
        return None;
    }
    Some((cli, local))
}

/// Whether `id` is a desktop session id we are willing to hand to the OS as part of a URL.
///
/// The desktop app validates the same shape at the far end, but that is its check, not ours:
/// these ids come off disk, and a file in the store is a file anyone can write. Validating
/// before the id reaches a URL is the same discipline `session_mark::valid_session_id`
/// applies to a resume argument.
#[tracing::instrument(level = "debug", ret)]
pub fn valid_local_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("local_") else {
        return false;
    };
    !rest.is_empty()
        && rest.len() <= 64
        && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The `claude://` URL that raises Claude Desktop and puts `local_id`'s conversation on top.
///
/// `None` for an id that fails [`valid_local_id`] — a caller that cannot build a link has to
/// fall back to opening the session in a pane, which is a better outcome than a URL built
/// out of an id we did not vet.
#[tracing::instrument(level = "debug", ret)]
pub fn deep_link(local_id: &str) -> Option<String> {
    valid_local_id(local_id).then(|| format!("claude://code/continue?session={local_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, shard: &str, name: &str, body: &str) {
        let dir = root.join(shard);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("hp-desktop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn a_record_maps_the_cli_uuid_to_the_desktop_id() {
        let root = temp_root("map");
        write(
            &root,
            "aa/bb",
            "local_1111-2222.json",
            r#"{"sessionId":"local_1111-2222","cliSessionId":"cd54b0e3-b688","cwd":"/tmp"}"#,
        );
        let map = scan_in(&root);
        assert_eq!(
            map.get("cd54b0e3-b688").map(String::as_str),
            Some("local_1111-2222")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_archived_conversation_is_not_in_the_desktop_app_any_more() {
        // The human took it out of the app's list. Raising the app onto it is no longer
        // what a click means, so it must not shadow the open-it-in-a-pane path.
        let root = temp_root("archived");
        write(
            &root,
            "aa/bb",
            "local_dead.json",
            r#"{"sessionId":"local_dead","cliSessionId":"gone","isArchived":true}"#,
        );
        assert!(scan_in(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn junk_in_the_store_is_skipped_rather_than_believed() {
        let root = temp_root("junk");
        write(&root, "aa/bb", "local_bad.json", "not json at all");
        write(
            &root,
            "aa/bb",
            "notes.txt",
            r#"{"sessionId":"local_x","cliSessionId":"y"}"#,
        );
        // Right shape, wrong depth: a record loose at the root is not a record.
        write(
            &root,
            ".",
            "local_loose.json",
            r#"{"sessionId":"local_z","cliSessionId":"w"}"#,
        );
        assert!(scan_in(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_a_vetted_id_ever_reaches_a_url() {
        assert!(valid_local_id("local_fa3065ed-7764-4d69-9912-ad0faea670fd"));
        assert!(!valid_local_id("local_"));
        assert!(!valid_local_id("fa3065ed"));
        // The shapes that make a URL mean something else than it reads.
        assert!(!valid_local_id("local_a&cmd=x"));
        assert!(!valid_local_id("local_a b"));
        assert!(!valid_local_id("local_../../etc"));
        assert!(!valid_local_id(&format!("local_{}", "a".repeat(65))));

        assert_eq!(
            deep_link("local_abc").as_deref(),
            Some("claude://code/continue?session=local_abc")
        );
        assert_eq!(deep_link("local_a?x"), None);
    }

    #[test]
    fn a_store_that_is_not_there_is_just_an_empty_answer() {
        // Every platform without a confirmed store path, and every machine with no Claude
        // Desktop install, lands here — it must be quiet, not an error.
        assert!(scan_in(Path::new("/nonexistent/hyperpanes/desktop/store")).is_empty());
    }
}
