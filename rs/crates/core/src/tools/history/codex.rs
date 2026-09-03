//! Codex CLI's on-disk session layout — the paths only, not (yet) a [`SessionProvider`].
//!
//! Codex 0.151 writes every conversation to a rollout JSONL under its home:
//!
//! ```text
//! $CODEX_HOME (or ~/.codex)/sessions/<YYYY>/<MM>/<DD>/rollout-<ISO8601>-<session id>.jsonl
//! ```
//!
//! Read off a real install on this machine — a `codex exec` turn driven against a local
//! stub model, so the file was written by codex itself rather than described by its docs.
//! The `thread_history_*.sqlite` beside it is **not** the primary record: its `thread_turns`
//! rows carry `rollout_ordinal` and `rollout_byte_offset`, i.e. it is a projection *over*
//! the JSONL. The JSONL is canonical, which is what makes it tailable.
//!
//! The timestamp in the filename is the session's start, dash-separated (`21-26-39`, not
//! `21:26:39`) so the name is portable. It is not knowable from a session id, which is why
//! [`rollout_for_session`] searches rather than derives.

use std::path::{Path, PathBuf};

/// The registry id this layout belongs to.
pub const TOOL_ID: &str = "codex";

/// `$CODEX_HOME`, else `~/.codex`. `CODEX_HOME` first because it is codex's own override
/// and a human who set it means it; `USERPROFILE` before `HOME` for the same reason
/// `copilot_root` does — a `HOME` set by a POSIX-ish shell on Windows is not where the CLI
/// writes.
#[tracing::instrument(level = "debug", ret)]
pub fn codex_root() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(v));
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".codex"))
}

/// The rollout file for `session_id` under `root` (a codex home), or `None` if no such
/// session has been written there.
///
/// Searched newest-first — years, then months, then days, each descending, returning at the
/// first hit — because the caller is a live pane's tailer polling on a timer, and a
/// conversation a pane is *in right now* is nearly always in today's directory. That makes
/// the hot path four directory reads, and the cold path bounded by the tree rather than by
/// the number of sessions.
///
/// The id is matched against directory *entries*, never joined into the path, so a hostile
/// value can name nothing outside the tree even if one reached here.
#[tracing::instrument(level = "debug", ret)]
pub fn rollout_for_session(root: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    let suffix = format!("-{session_id}.jsonl");
    for year in numeric_dirs_desc(&root.join("sessions")) {
        for month in numeric_dirs_desc(&year) {
            for day in numeric_dirs_desc(&month) {
                if let Some(hit) = rollout_in_day(&day, &suffix) {
                    return Some(hit);
                }
            }
        }
    }
    None
}

/// The `rollout-*` file in one day directory whose name ends in `suffix`.
#[tracing::instrument(level = "debug", ret)]
fn rollout_in_day(day: &Path, suffix: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(day).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("rollout-") && name.ends_with(suffix) {
            return Some(entry.path());
        }
    }
    None
}

/// `dir`'s subdirectories whose names are all digits, newest (highest) first.
///
/// Non-numeric entries are skipped rather than sorted in: the tree is `YYYY/MM/DD`, and a
/// stray file or a `.DS_Store` there must not become a directory the walk descends into.
#[tracing::instrument(level = "debug", ret)]
fn numeric_dirs_desc(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .collect();
    // Zero-padded fixed-width components, so a lexical sort IS the chronological one.
    names.sort_unstable_by(|a, b| b.cmp(a));
    names.into_iter().map(|n| dir.join(n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hp-codex-paths-{}-{tag}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_rollout(root: &Path, date: &str, stamp: &str, sid: &str) -> PathBuf {
        let (y, rest) = date.split_at(4);
        let (m, d) = (&rest[1..3], &rest[4..6]);
        let dir = root.join("sessions").join(y).join(m).join(d);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("rollout-{stamp}-{sid}.jsonl"));
        std::fs::write(&p, "").unwrap();
        p
    }

    #[test]
    fn finds_a_rollout_by_its_session_id() {
        let root = scratch("find");
        let want = write_rollout(
            &root,
            "2026-09-01",
            "2026-09-01T21-26-39",
            "01a05fb9-acd1-7293-bdba-ae814721095b",
        );
        assert_eq!(
            rollout_for_session(&root, "01a05fb9-acd1-7293-bdba-ae814721095b"),
            Some(want)
        );
        assert_eq!(rollout_for_session(&root, "no-such-session"), None);
        // A bare id must never be joined into the path: an empty or traversing value names
        // nothing rather than naming the tree itself.
        assert_eq!(rollout_for_session(&root, ""), None);
        assert_eq!(rollout_for_session(&root, "../../etc/passwd"), None);
    }

    #[test]
    fn searches_newest_day_first_and_still_reaches_older_ones() {
        let root = scratch("order");
        // Two sessions, months apart. Both must be findable; the newest tree is walked
        // first, which is the whole point of the descending sort.
        let old = write_rollout(&root, "2025-12-31", "2025-12-31T23-59-59", "old-session-id");
        let new = write_rollout(&root, "2026-09-01", "2026-09-01T09-00-00", "new-session-id");
        assert_eq!(rollout_for_session(&root, "new-session-id"), Some(new));
        assert_eq!(rollout_for_session(&root, "old-session-id"), Some(old));
    }

    #[test]
    fn a_non_numeric_entry_in_the_date_tree_is_skipped() {
        let root = scratch("junk");
        std::fs::create_dir_all(root.join("sessions").join("archive")).unwrap();
        std::fs::write(root.join("sessions").join(".DS_Store"), "x").unwrap();
        let want = write_rollout(&root, "2026-09-01", "2026-09-01T09-00-00", "sid-1");
        assert_eq!(rollout_for_session(&root, "sid-1"), Some(want));
    }

    #[test]
    fn codex_home_overrides_the_default_root_and_the_tailer_resolves_through_it() {
        // Two assertions in one test on purpose: `CODEX_HOME` is process-global, so the
        // fewer tests that move it the fewer can race. Codex's own override wins over HOME
        // (a human who set it gets the tree they pointed at, not a second empty one), and
        // the speech tailer's "codex" arm has to reach a real file through it — the wiring
        // between this module and `speech::tailer` is otherwise untested.
        let root = scratch("home");
        let want = write_rollout(&root, "2026-09-01", "2026-09-01T21-26-39", "sid-wired");
        let prev = std::env::var_os("CODEX_HOME");
        std::env::set_var("CODEX_HOME", &root);
        assert_eq!(codex_root().as_deref(), Some(root.as_path()));
        let got = crate::speech::tailer::tool_transcript(TOOL_ID, "sid-wired", "/tmp/proj");
        // A session codex never wrote resolves to nothing rather than to a guessed path.
        let missing = crate::speech::tailer::tool_transcript(TOOL_ID, "sid-absent", "/tmp/proj");
        match prev {
            Some(v) => std::env::set_var("CODEX_HOME", v),
            None => std::env::remove_var("CODEX_HOME"),
        }
        let got = got.expect("codex arm resolves a rollout under CODEX_HOME");
        assert_eq!(got.path, want);
        assert_eq!(
            got.format,
            crate::speech::tailer::TranscriptFormat::CodexRollout
        );
        assert!(missing.is_none());
    }
}
