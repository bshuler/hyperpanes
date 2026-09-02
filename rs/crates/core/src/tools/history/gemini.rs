//! Gemini CLI's on-disk session layout — the paths only, not (yet) a [`SessionProvider`].
//!
//! Gemini 0.58 writes every conversation to a chat JSONL under its home:
//!
//! ```text
//! ~/.gemini/tmp/<project dir>/chats/session-<YYYY-MM-DD>T<HH-MM>-<first 8 of session id>.jsonl
//! ```
//!
//! Read off a real install on this machine — `gemini -p` turns driven against a local stub
//! model, so the files were written by gemini itself rather than described by its docs.
//!
//! `<project dir>` is the thing worth knowing about, because it looks derivable and is not.
//! It is the *basename* of the working directory, with a `-1`, `-2`, … suffix appended when
//! an earlier directory already claimed that basename — allocated in first-seen order and
//! recorded in `~/.gemini/projects.json`. Two checkouts both called `api` therefore map to
//! `api` and `api-1` depending on which one gemini saw first, which is a property of that
//! machine's history and of nothing else. So [`chat_for_session`] searches the project dirs
//! and confirms the hit by reading the full session id out of the file, rather than
//! computing a path and hoping.
//!
//! The timestamp in the filename is the session's start and is not knowable from a session
//! id either, which is the same reason [`crate::tools::history::codex`] searches.

use std::path::{Path, PathBuf};

/// The registry id this layout belongs to.
pub const TOOL_ID: &str = "gemini";

/// `$GEMINI_CLI_HOME/.gemini`, else `~/.gemini`.
///
/// Note the shape of gemini's own override: `GEMINI_CLI_HOME` replaces the **home
/// directory**, and `.gemini` is still appended to it. (`GEMINI_DIR` appears in the bundle
/// and is tempting, but it is a compile-time constant holding the string `.gemini`, not an
/// environment variable — reading it as one would send us to the wrong tree.) `USERPROFILE`
/// before `HOME` because gemini asks Node for `os.homedir()`, which on Windows is
/// `USERPROFILE` and not the `HOME` a POSIX-ish shell may have set.
pub fn gemini_root() -> Option<PathBuf> {
    let home = std::env::var_os("GEMINI_CLI_HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| std::env::var_os("HOME"))?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".gemini"))
}

/// The chat file for `session_id` under `root` (a `.gemini` directory), or `None` if no such
/// session has been written there.
///
/// The filename embeds only the first 8 characters of the id, so a name match is a
/// *candidate*: the answer is confirmed by reading the full `sessionId` out of the file's
/// first line. Both gates matter — the name narrows hundreds of files to about one without
/// opening any of them, and the header settles which one it is.
///
/// The id is matched against directory *entries*, never joined into the path, so a hostile
/// value can name nothing outside the tree even if one reached here.
pub fn chat_for_session(root: &Path, session_id: &str) -> Option<PathBuf> {
    let prefix = session_prefix(session_id)?;
    let needle = format!("-{prefix}.jsonl");
    for project in project_dirs(&root.join("tmp")) {
        for entry in std::fs::read_dir(project.join("chats"))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with("session-") || !name.ends_with(&needle) {
                continue;
            }
            let path = entry.path();
            if header_session_id(&path).as_deref() == Some(session_id) {
                return Some(path);
            }
        }
    }
    None
}

/// The first 8 characters of a session id, as the chat filename spells them.
///
/// Rejects anything that is not plain lowercase-hex-or-dash, which is what a gemini session
/// id is: it keeps a path separator out of the needle, and it means a caller passing
/// something that was never a session id gets `None` rather than a scan.
fn session_prefix(session_id: &str) -> Option<String> {
    if session_id.len() < 8
        || !session_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
    {
        return None;
    }
    Some(session_id[..8].to_string())
}

/// The `sessionId` on a chat file's first line.
///
/// Gemini writes a header record there — `{"sessionId":…,"projectHash":…,"kind":"main"}` —
/// before any message. Only that line is read: the file is an append-only log that grows
/// for as long as the conversation does, and this runs on the tailer's timer.
fn header_session_id(path: &Path) -> Option<String> {
    use std::io::{BufRead, Read};
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    // Bounded so a huge single-line file cannot be pulled into memory by a path that is
    // only ever meant to read a short header.
    std::io::BufReader::new(file)
        .take(64 * 1024)
        .read_line(&mut line)
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    v.get("sessionId")?.as_str().map(str::to_string)
}

/// `dir`'s subdirectories — gemini's per-project temp dirs.
///
/// Unordered, because unlike codex's dated tree there is nothing chronological in these
/// names to sort by, and the full-id check makes the answer independent of visit order.
fn project_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hp-gemini-paths-{}-{tag}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A chat file as gemini writes one: header line first, then messages.
    fn write_chat(root: &Path, project: &str, stamp: &str, sid: &str) -> PathBuf {
        let dir = root.join("tmp").join(project).join("chats");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("session-{stamp}-{}.jsonl", &sid[..8]));
        std::fs::write(
            &p,
            format!(
                "{{\"sessionId\":\"{sid}\",\"projectHash\":\"cb3c\",\"kind\":\"main\"}}\n\
                 {{\"type\":\"gemini\",\"content\":\"hi\"}}\n"
            ),
        )
        .unwrap();
        p
    }

    #[test]
    fn finds_a_chat_by_its_session_id() {
        let root = scratch("find");
        let want = write_chat(
            &root,
            "gemproj",
            "2026-09-02T09-39",
            "70c6bdeb-e601-4fe5-8349-45d82a818ea7",
        );
        assert_eq!(
            chat_for_session(&root, "70c6bdeb-e601-4fe5-8349-45d82a818ea7"),
            Some(want)
        );
        assert_eq!(
            chat_for_session(&root, "deadbeef-0000-0000-0000-000000000000"),
            None
        );
        // A bare id must never be joined into the path: an empty, short or traversing value
        // names nothing rather than naming the tree itself.
        assert_eq!(chat_for_session(&root, ""), None);
        assert_eq!(chat_for_session(&root, "../../etc/passwd"), None);
        assert_eq!(chat_for_session(&root, "70c6bde"), None);
    }

    #[test]
    fn two_sessions_sharing_a_filename_prefix_are_told_apart_by_the_header() {
        // The filename carries only 8 characters of the id, so a name match is a candidate
        // and nothing more. Returning the wrong one here would have a pane speak another
        // conversation's replies — quietly, and forever.
        let root = scratch("prefix");
        let a = "70c6bdeb-e601-4fe5-8349-45d82a818ea7";
        let b = "70c6bdeb-ffff-4fe5-8349-45d82a818ea7";
        let want_a = write_chat(&root, "one", "2026-09-02T09-39", a);
        let want_b = write_chat(&root, "two", "2026-09-02T10-01", b);
        assert_eq!(chat_for_session(&root, a), Some(want_a));
        assert_eq!(chat_for_session(&root, b), Some(want_b));
    }

    #[test]
    fn a_chat_is_found_whichever_project_dir_gemini_filed_it_under() {
        // Gemini names project dirs `<basename>`, `<basename>-1`, … in first-seen order, so
        // the dir a given cwd maps to is a fact about that machine's history. The search
        // must not assume the un-suffixed one.
        let root = scratch("collide");
        std::fs::create_dir_all(root.join("tmp").join("gemproj").join("chats")).unwrap();
        std::fs::write(root.join("tmp").join(".DS_Store"), "x").unwrap();
        let want = write_chat(
            &root,
            "gemproj-1",
            "2026-09-02T09-40",
            "70fcbcab-e76e-4a63-973f-342fd0da7dee",
        );
        assert_eq!(
            chat_for_session(&root, "70fcbcab-e76e-4a63-973f-342fd0da7dee"),
            Some(want)
        );
    }

    #[test]
    fn a_file_with_no_readable_header_is_not_the_answer() {
        // Gemini creates the file before it has written anything to it. An empty or
        // half-written header must resolve to nothing, not to an unverified path.
        let root = scratch("header");
        let dir = root.join("tmp").join("p").join("chats");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session-2026-09-02T09-39-70c6bdeb.jsonl"), "").unwrap();
        std::fs::write(
            dir.join("session-2026-09-02T09-40-70c6bdec.jsonl"),
            "{\"sessionId\":",
        )
        .unwrap();
        assert_eq!(
            chat_for_session(&root, "70c6bdeb-e601-4fe5-8349-45d82a818ea7"),
            None
        );
        assert_eq!(
            chat_for_session(&root, "70c6bdec-e601-4fe5-8349-45d82a818ea7"),
            None
        );
    }

    #[test]
    fn gemini_cli_home_overrides_the_default_root_and_the_tailer_resolves_through_it() {
        // Two assertions in one test on purpose: `GEMINI_CLI_HOME` is process-global, so the
        // fewer tests that move it the fewer can race. Note what the override means — it
        // replaces the *home*, so `.gemini` is still appended. And the speech tailer's
        // "gemini" arm has to reach a real file through it; the wiring between this module
        // and `speech::tailer` is otherwise untested.
        let home = scratch("home");
        let root = home.join(".gemini");
        let want = write_chat(
            &root,
            "gemproj",
            "2026-09-02T09-39",
            "70c6bdeb-e601-4fe5-8349-45d82a818ea7",
        );
        let prev = std::env::var_os("GEMINI_CLI_HOME");
        std::env::set_var("GEMINI_CLI_HOME", &home);
        let rooted = gemini_root();
        let got = crate::speech::tailer::tool_transcript(
            TOOL_ID,
            "70c6bdeb-e601-4fe5-8349-45d82a818ea7",
            "/tmp/proj",
        );
        // A session gemini never wrote resolves to nothing rather than to a guessed path.
        let missing = crate::speech::tailer::tool_transcript(
            TOOL_ID,
            "deadbeef-0000-0000-0000-000000000000",
            "/tmp/proj",
        );
        match prev {
            Some(v) => std::env::set_var("GEMINI_CLI_HOME", v),
            None => std::env::remove_var("GEMINI_CLI_HOME"),
        }
        assert_eq!(rooted.as_deref(), Some(root.as_path()));
        let got = got.expect("gemini arm resolves a chat under GEMINI_CLI_HOME");
        assert_eq!(got.path, want);
        assert_eq!(
            got.format,
            crate::speech::tailer::TranscriptFormat::GeminiChat
        );
        assert!(missing.is_none());
    }
}
