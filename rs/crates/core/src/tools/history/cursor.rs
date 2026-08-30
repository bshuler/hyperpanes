//! The Cursor Agent [`SessionProvider`] — `~/.cursor`, read the way `claude.rs` reads
//! `~/.claude/projects`.
//!
//! Cursor splits what Claude Code keeps in one place across two trees, and the split is the
//! whole design of this file:
//!
//! ```text
//! ~/.cursor/chats/<md5(cwd)>/<session-id>/meta.json   <- the exact cwd, a title, timestamps
//! ~/.cursor/projects/<Encoded-Cwd>/agent-transcripts/<session-id>/<session-id>.jsonl
//! ```
//!
//! `meta.json` is per-session and carries `cwd` verbatim, so a session that has one needs no
//! decoding at all. The transcript tree is filed under the *same lossy encoding* Claude uses
//! (`/Users/me/src/my_app` -> `Users-me-src-my-app`; `/`, `.` and `_` all become `-`, runs
//! not collapsed) with the leading separator's `-` dropped — so a session whose `chats` entry
//! has been pruned has to have its project recovered the same way Claude's are: a sibling's
//! `cwd` that re-encodes to the directory name, else a filesystem probe, else nothing we
//! would spawn in. [`ProjectOrigin`] records which, and `check_project` refuses the rest.
//!
//! The stores overlap but neither contains the other — on the machine this was written
//! against, 35 `chats` sessions and 45 distinct transcripts share 33 ids. So both are read
//! and the union is keyed by session id.
//!
//! Nothing here opens `store.db`. It is a content-addressed blob chain whose root id lives in
//! a `meta` row, and every conversation in it is also on disk as plain JSONL under
//! `projects/`; reverse-engineering the blob format to re-read text we already have would buy
//! nothing and break on the next Cursor release.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Deserialize;

use super::{
    check_project, resolve_program, sort_for_panel, ResumeBlocked, ResumeCommand, ResumePlan,
    SessionProvider, ToolSession,
};
use crate::claude_history::{
    decode_by_probing, decode_project_dir, encode_project_dir, project_dirs_in, HistorySource,
    ProjectOrigin,
};

/// The registry id this provider serves — Cursor's CLI is `cursor-agent`, not `cursor`.
pub const TOOL_ID: &str = "cursor-agent";

/// The same bounds `claude_history.rs` reads transcripts under. Re-stated rather than shared
/// because they are private there; the values are deliberately identical, so a row from
/// either provider is elided and searched to the same depth.
const SUMMARY_MAX: usize = 160;
const SUMMARY_SCAN_LINES: usize = 60;
const FULL_TEXT_MAX: usize = 32 * 1024;
const FULL_TEXT_SCAN_LINES: usize = 2000;

/// Cursor Agent's locally resumable conversations.
pub struct CursorProvider {
    /// `chats/*/*/meta.json`, by path.
    metas: HashMap<PathBuf, Cached<ChatMeta>>,
    /// `projects/*/agent-transcripts/*/*.jsonl`, by path.
    transcripts: HashMap<PathBuf, Cached<Transcript>>,
    last_scan_parsed: usize,
    overrides: BTreeMap<String, String>,
    /// A single `.cursor` store to scan instead of the machine's own — the test seam. Both
    /// trees are needed to recover a project path, so this is the store root, not one of the
    /// two directories under it.
    root: Option<PathBuf>,
}

impl Default for CursorProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorProvider {
    /// A provider with no per-tool path override — detection falls to `PATH` and the
    /// well-known install locations.
    pub fn new() -> Self {
        Self {
            metas: HashMap::new(),
            transcripts: HashMap::new(),
            last_scan_parsed: 0,
            overrides: BTreeMap::new(),
            root: None,
        }
    }

    /// A provider that honours the human's per-tool binary overrides (the settings page's
    /// `tool id -> path` map). Held rather than passed per call because
    /// [`SessionProvider::resume`] takes `&self`.
    pub fn with_overrides(overrides: BTreeMap<String, String>) -> Self {
        Self {
            overrides,
            ..Self::new()
        }
    }

    /// Scan `root` (one `.cursor` directory) rather than the machine's own.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
            ..Self::new()
        }
    }

    /// Files re-parsed by the last [`scan`](SessionProvider::scan) — the cache effectiveness
    /// the 20 s panel refresh lives or dies by, surfaced so it can be asserted on.
    pub fn last_scan_parsed(&self) -> usize {
        self.last_scan_parsed
    }

    /// The store to read: the test seam if one was given, else this machine's `~/.cursor`.
    fn store(&self) -> Option<PathBuf> {
        match &self.root {
            Some(r) => Some(r.clone()),
            None => cursor_root(),
        }
    }

    /// Index `chats/<hash>/<id>/meta.json` by session id. Cheap — the files are one short
    /// line each — but still fingerprinted, because a live session rewrites its `meta.json`
    /// on every turn and the panel re-scans every 20 s.
    fn read_chats(&mut self, chats: &Path) -> BTreeMap<String, ChatMeta> {
        let mut out = BTreeMap::new();
        let mut live: HashSet<PathBuf> = HashSet::new();
        for hash_dir in child_dirs(chats) {
            for session_dir in child_dirs(&hash_dir) {
                let Some(id) = dir_name(&session_dir) else {
                    continue;
                };
                let path = session_dir.join("meta.json");
                live.insert(path.clone());
                if let Some(meta) = cached(
                    &mut self.metas,
                    &path,
                    &mut self.last_scan_parsed,
                    read_chat_meta,
                ) {
                    out.insert(id, meta);
                }
            }
        }
        self.metas.retain(|k, _| live.contains(k));
        out
    }

    /// One project directory's transcripts, as `(session id, transcript)` in id order.
    ///
    /// Only `agent-transcripts/<id>/<id>.jsonl` counts. The sibling `subagents/*.jsonl` are
    /// a delegated agent's own trace, not a conversation anyone resumes, and naming the file
    /// after its directory is the deterministic way to tell them apart.
    fn read_transcripts(&mut self, project_dir: &Path, live: &mut HashSet<PathBuf>) -> Vec<Found> {
        let mut out = Vec::new();
        for session_dir in child_dirs(&project_dir.join("agent-transcripts")) {
            let Some(id) = dir_name(&session_dir) else {
                continue;
            };
            let path = session_dir.join(format!("{id}.jsonl"));
            live.insert(path.clone());
            if let Some(t) = cached(
                &mut self.transcripts,
                &path,
                &mut self.last_scan_parsed,
                read_transcript,
            ) {
                out.push(Found { id, t });
            }
        }
        out
    }
}

/// One transcript and the id it was filed under.
struct Found {
    id: String,
    t: Transcript,
}

impl SessionProvider for CursorProvider {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn scan(&mut self) -> Vec<ToolSession> {
        self.last_scan_parsed = 0;
        let Some(store) = self.store() else {
            return Vec::new();
        };
        let metas = self.read_chats(&store.join("chats"));

        // Keyed by session id: the same transcript is filed under every window that had the
        // conversation open, so `5a0986b6…` really does appear under both `Users-bshuler-code`
        // and the `empty-window` sentinel. Identical content, different provenance — keep the
        // reading whose project path we can stand behind.
        let mut best: BTreeMap<String, ToolSession> = BTreeMap::new();
        let mut live: HashSet<PathBuf> = HashSet::new();
        for dir in project_dirs_in(&store.join("projects")) {
            let found = self.read_transcripts(&dir, &mut live);
            if found.is_empty() {
                continue;
            }
            // The encoded name is only decodable with help; a sibling session's `cwd` that
            // re-encodes to it is the strongest help available (and the only kind that is
            // proof rather than a guess).
            let cwds: Vec<PathBuf> = found
                .iter()
                .filter_map(|f| metas.get(&f.id))
                .map(|m| m.cwd.clone())
                .collect();
            let resolved = resolve_dir(&dir, &cwds);
            for f in found {
                // A session with its own `meta.json` needs none of that: `cwd` is recorded
                // verbatim, per session, in the session's own directory. Nothing lossy stands
                // between it and the conversation.
                let (project, origin) = match metas.get(&f.id) {
                    Some(m) => (m.cwd.clone(), ProjectOrigin::TranscriptExact),
                    None => resolved.clone(),
                };
                let row = row(&f.id, metas.get(&f.id), Some(&f.t), project, origin);
                keep_stronger(&mut best, row);
            }
        }
        self.transcripts.retain(|k, _| live.contains(k));

        // Chats whose transcript has been pruned (Cursor sweeps `projects/` — it leaves
        // `.agent-data-cleanup-<date>` markers behind). Still resumable, so still a row; its
        // own `hasConversation` is what says whether there is anything to resume into.
        for (id, m) in &metas {
            if !best.contains_key(id) && m.has_conversation {
                let row = row(
                    id,
                    Some(m),
                    None,
                    m.cwd.clone(),
                    ProjectOrigin::TranscriptExact,
                );
                keep_stronger(&mut best, row);
            }
        }

        let mut rows: Vec<ToolSession> = best.into_values().collect();
        unify_origins(&mut rows);
        sort_for_panel(&mut rows);
        rows
    }

    fn resume(&self, session: &ToolSession) -> ResumePlan {
        if session.source != HistorySource::Cursor {
            return ResumePlan::Blocked(ResumeBlocked::Unsupported {
                tool_id: session.source.tool_id(),
            });
        }
        // Ids come off a disk the user (or anything running as them) can write, and this one
        // lands on a command line.
        if !crate::claude_panes::valid_session_id(&session.id) {
            return ResumePlan::Blocked(ResumeBlocked::BadSessionId {
                id: session.id.clone(),
            });
        }
        let cwd = match check_project(session) {
            Ok(p) => p,
            Err(b) => return ResumePlan::Blocked(b),
        };
        let program = match resolve_program(TOOL_ID, &self.overrides) {
            Ok(p) => p,
            Err(b) => return ResumePlan::Blocked(b),
        };
        // `cursor-agent --resume [chatId]`, verified against `--help` on 2026.08.25-3e8eec8.
        // The cwd is not decoration: `chats/` is keyed by a hash of it, so the same id
        // resumes nothing anywhere else.
        ResumePlan::Ready(ResumeCommand {
            program,
            args: vec!["--resume".to_string(), session.id.clone()],
            cwd,
        })
    }
}

/// `~/.cursor` for the current user, or `None` if no home directory is known.
pub fn cursor_root() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".cursor"))
}

/// Whether `path` looks like a `.cursor` store worth scanning. Either tree alone is enough —
/// a machine that has only ever run the IDE has `projects/` and no `chats/`.
pub fn store_exists(path: &Path) -> bool {
    path.join("chats").is_dir() || path.join("projects").is_dir()
}

/// Cursor's name for `project`'s transcript directory: Claude's encoding with the leading
/// separator's `-` dropped (`/Users/me/src` -> `Users-me-src`).
pub fn encode_project_dir_cursor(project: &Path) -> String {
    let e = encode_project_dir(project);
    e.strip_prefix('-').map(str::to_string).unwrap_or(e)
}

/// Put the leading separator back, so the shared inverses ([`decode_by_probing`],
/// [`decode_project_dir`]) run on Cursor names unchanged rather than being reimplemented.
fn as_claude_encoded(name: &str) -> String {
    #[cfg(windows)]
    {
        // `C:\src` encodes to `C--src`, which never had a leading separator to lose.
        let b = name.as_bytes();
        if b.len() >= 3 && b[0].is_ascii_alphanumeric() && &name[1..3] == "--" {
            return name.to_string();
        }
    }
    format!("-{name}")
}

/// Name the project behind one `projects/<encoded>` directory, given the `cwd`s its sessions
/// reported. The same ladder `claude_history::resolve_project` climbs, over Cursor's names:
/// a `cwd` that re-encodes to the directory is proof, a filesystem probe is proof of the same
/// kind, a `cwd` that does not re-encode is a real directory but not provably this one's, and
/// the lossy substitution is a label only. `empty-window` — Cursor's sentinel for a window
/// with no folder open — lands on that last rung, which is exactly right: there is no project.
fn resolve_dir(dir: &Path, cwds: &[PathBuf]) -> (PathBuf, ProjectOrigin) {
    let Some(encoded) = dir_name(dir) else {
        return (dir.to_path_buf(), ProjectOrigin::DecodedUnverified);
    };
    let mut fallback: Option<&PathBuf> = None;
    for cwd in cwds {
        if encode_project_dir_cursor(cwd) == encoded {
            return (cwd.clone(), ProjectOrigin::TranscriptExact);
        }
        fallback.get_or_insert(cwd);
    }
    let rooted = as_claude_encoded(&encoded);
    if let Some(probed) = decode_by_probing(&rooted) {
        return (probed, ProjectOrigin::ProbedExact);
    }
    match fallback {
        Some(cwd) => (cwd.clone(), ProjectOrigin::TranscriptCwd),
        None => (
            decode_project_dir(&rooted),
            ProjectOrigin::DecodedUnverified,
        ),
    }
}

/// Insert `row`, or replace an existing reading of the same session with a better-evidenced
/// one. `ProjectOrigin`'s declaration order is its strength order, so this is a `min`.
fn keep_stronger(best: &mut BTreeMap<String, ToolSession>, row: ToolSession) {
    match best.get(&row.id) {
        Some(prev) if prev.project_origin <= row.project_origin => {}
        _ => {
            best.insert(row.id.clone(), row);
        }
    }
}

/// Give every row for one project path the strongest provenance any of them reached. Two
/// sessions in the same directory can arrive by different rungs of [`resolve_dir`], and the
/// panel draws one heading per project — a heading whose rows disagree about whether the path
/// is real would be a bug in the label, not in the data.
fn unify_origins(rows: &mut [ToolSession]) {
    let mut strongest: HashMap<PathBuf, ProjectOrigin> = HashMap::new();
    for r in rows.iter() {
        let e = strongest
            .entry(r.project.clone())
            .or_insert(r.project_origin);
        *e = (*e).min(r.project_origin);
    }
    for r in rows.iter_mut() {
        if let Some(o) = strongest.get(&r.project) {
            r.project_origin = *o;
        }
    }
}

/// Widen one session into a panel row. `meta` is the `chats` entry when there is one,
/// `transcript` the JSONL when there is one; at least one of the two exists or there is no
/// session to draw.
fn row(
    id: &str,
    meta: Option<&ChatMeta>,
    transcript: Option<&Transcript>,
    project: PathBuf,
    origin: ProjectOrigin,
) -> ToolSession {
    let first_user = transcript.map(|t| t.first_user.clone()).unwrap_or_default();
    // Cursor's own title first — it is a label the tool chose for the conversation, and it
    // outranks a truncated opening prompt exactly as Claude's `summary` record does.
    let summary = meta
        .and_then(|m| m.title.clone())
        .map(|t| clean_summary(&t))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| first_user.clone());
    ToolSession {
        id: id.to_string(),
        source: HistorySource::Cursor,
        project,
        project_origin: origin,
        // Cursor records no branch anywhere in either store — not in `meta.json`, not in the
        // transcript. `None` is the honest answer; guessing from the tree's branch *now*
        // would label the conversation with something it never ran on.
        branch: None,
        // `updatedAtMs` is last activity, the same thing Claude's mtime ordering means. A
        // pruned-chats session falls back to its transcript's mtime, which is the same clock.
        started_at: meta
            .and_then(|m| m.updated_at_ms.or(m.created_at_ms))
            .or_else(|| transcript.and_then(|t| t.mtime_ms)),
        summary,
        first_user,
        message_count: transcript.map(|t| t.message_count).unwrap_or(0),
        full_text: transcript.map(|t| t.full_text.clone()).unwrap_or_default(),
    }
}

// ===== the two file formats =====

/// `chats/<hash>/<id>/meta.json`, verified against the live store: every field but `title`
/// was present on all 35 sessions, and `cwd` on all of them.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatMeta {
    cwd: PathBuf,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    created_at_ms: Option<u64>,
    #[serde(default)]
    updated_at_ms: Option<u64>,
    /// Cursor's own flag for "this session got past the prompt". False on a window that was
    /// opened and abandoned.
    #[serde(default)]
    has_conversation: bool,
}

fn read_chat_meta(path: &Path) -> Option<ChatMeta> {
    let text = fs::read_to_string(path).ok()?;
    let meta: ChatMeta = serde_json::from_str(&text).ok()?;
    // A relative or empty `cwd` is not a directory anything should be spawned in.
    meta.cwd.is_absolute().then_some(meta)
}

/// What one bounded prefix read of a transcript recovers.
#[derive(Debug, Clone, Default)]
struct Transcript {
    first_user: String,
    full_text: String,
    message_count: usize,
    mtime_ms: Option<u64>,
}

/// Read one `<id>.jsonl`: count every record, and within a bounded prefix recover the opening
/// user prompt and the lowercased user+assistant text search falls back to. Records past the
/// bounds are counted blind, never JSON-parsed — the same deal `claude_history` strikes, for
/// the same reason.
///
/// Cursor's records are `{"role":"user"|"assistant","message":{"content":[…]}}`, plus a
/// trailing `{"type":"turn_ended",…}` that carries no text.
fn read_transcript(path: &Path) -> Option<Transcript> {
    use std::io::{BufRead, BufReader};
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut out = Transcript {
        mtime_ms: fingerprint(path).map(|(m, _)| m),
        ..Transcript::default()
    };
    let mut first_user: Option<String> = None;

    for (i, line) in reader.lines().enumerate() {
        let Ok(line) = line else { break };
        out.message_count += 1;
        let summary_done = i >= SUMMARY_SCAN_LINES || first_user.is_some();
        let full_done = i >= FULL_TEXT_SCAN_LINES || out.full_text.len() >= FULL_TEXT_MAX;
        if summary_done && full_done {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let Some(text) = message_text(&v) else {
            continue;
        };
        match v.get("role").and_then(|r| r.as_str()) {
            Some("user") => {
                let query = user_query(&text);
                if query.trim().is_empty() {
                    continue;
                }
                if !summary_done {
                    first_user = Some(clean_summary(query));
                }
                if !full_done {
                    push_full_text(&mut out.full_text, query);
                }
            }
            Some("assistant") if !full_done => push_full_text(&mut out.full_text, &text),
            _ => {}
        }
    }
    out.first_user = first_user.unwrap_or_default();
    Some(out)
}

/// The human's prompt out of a `user` record. Cursor wraps it as
/// `<timestamp>…</timestamp>\n<user_query>\n…\n</user_query>` — literal, fixed tags, so this
/// is a slice rather than a guess. Text carrying no `<user_query>` is returned whole.
fn user_query(text: &str) -> &str {
    const OPEN: &str = "<user_query>";
    const CLOSE: &str = "</user_query>";
    let Some(start) = text.find(OPEN) else {
        return text;
    };
    let rest = &text[start + OPEN.len()..];
    match rest.find(CLOSE) {
        Some(end) => &rest[..end],
        None => rest,
    }
}

/// The human-visible text of a record's `message.content`: every `{"type":"text"}` block,
/// space-joined. `tool_use` blocks are skipped — they are the machine's half of the turn, and
/// a shell command line is not what someone searching for a conversation is looking for.
fn message_text(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let mut out = String::new();
    for block in content.as_array()? {
        if block.get("type").and_then(|t| t.as_str()) != Some("text") {
            continue;
        }
        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(t);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Append one message to the full-text extract: whitespace-collapsed, lowercased (search is
/// case-insensitive and never re-lowers the stored text), and cut at a char boundary once
/// [`FULL_TEXT_MAX`] is reached.
fn push_full_text(full: &mut String, text: &str) {
    if full.len() >= FULL_TEXT_MAX {
        return;
    }
    let remaining = FULL_TEXT_MAX - full.len();
    if !full.is_empty() {
        full.push(' ');
    }
    let collapsed = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if collapsed.len() <= remaining {
        full.push_str(&collapsed);
        return;
    }
    let mut cut = remaining;
    while cut > 0 && !collapsed.is_char_boundary(cut) {
        cut -= 1;
    }
    full.push_str(&collapsed[..cut]);
}

/// Collapse all whitespace to single spaces, trim, and truncate to [`SUMMARY_MAX`] chars.
fn clean_summary(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= SUMMARY_MAX {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(SUMMARY_MAX - 1).collect();
    out.push('…');
    out
}

// ===== fingerprint-keyed file cache =====

struct Cached<T> {
    mtime_ms: u64,
    size: u64,
    value: T,
}

/// `(mtime epoch ms, size bytes)` for `path`, or `None` when stat fails.
fn fingerprint(path: &Path) -> Option<(u64, u64)> {
    let m = fs::metadata(path).ok()?;
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)?;
    Some((mtime, m.len()))
}

/// The cached value for `path` when its `(mtime, size)` is unchanged, else `parse`d and
/// cached. Counts a parse in `parsed` only when one happened, so "warm scan parsed nothing"
/// stays a statement about work done, not about files that failed to open.
fn cached<T: Clone>(
    map: &mut HashMap<PathBuf, Cached<T>>,
    path: &Path,
    parsed: &mut usize,
    parse: impl FnOnce(&Path) -> Option<T>,
) -> Option<T> {
    let fp = fingerprint(path);
    if let (Some(c), Some((mtime, size))) = (map.get(path), fp) {
        if c.mtime_ms == mtime && c.size == size {
            return Some(c.value.clone());
        }
    }
    let value = parse(path)?;
    *parsed += 1;
    let (mtime_ms, size) = fp.unwrap_or((0, 0));
    map.insert(
        path.to_path_buf(),
        Cached {
            mtime_ms,
            size,
            value: value.clone(),
        },
    );
    Some(value)
}

/// The subdirectories of `dir`, sorted, so an ambiguous name resolves the same way on every
/// scan. `is_dir()` on the path rather than the entry's file type: a symlinked component is
/// ordinary in the directories people keep code in.
fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    out.sort();
    out
}

fn dir_name(dir: &Path) -> Option<String> {
    dir.file_name().map(|n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hp-cursor-{}-{tag}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_cursor_name_is_the_claude_encoding_minus_its_leading_separator() {
        let p = PathBuf::from("/Users/me/code/test_1");
        assert_eq!(encode_project_dir(&p), "-Users-me-code-test-1");
        assert_eq!(encode_project_dir_cursor(&p), "Users-me-code-test-1");
        assert_eq!(
            as_claude_encoded("Users-me-code-test-1"),
            encode_project_dir(&p)
        );
    }

    #[test]
    fn a_user_record_is_sliced_out_of_its_tags() {
        let wrapped = "<timestamp>Friday, Aug 28, 2026, 11:06 AM (UTC-4)</timestamp>\n\
                       <user_query>\nrecover some disk space\n</user_query>";
        assert_eq!(user_query(wrapped).trim(), "recover some disk space");
        // An unwrapped message is passed through rather than dropped.
        assert_eq!(user_query("plain text"), "plain text");
        // A half-written record still yields what it has.
        assert_eq!(user_query("<user_query>\nhalf").trim(), "half");
    }

    #[test]
    fn empty_window_is_a_sentinel_not_a_path() {
        // Cursor files a folder-less window under a literal name. Nothing decodes it, so the
        // row must be a label the resume gate refuses — not `/empty/window` presented as real.
        let (path, origin) = resolve_dir(Path::new("/store/projects/empty-window"), &[]);
        assert_eq!(origin, ProjectOrigin::DecodedUnverified);
        assert!(!origin.is_exact());
        assert!(path.to_string_lossy().contains("empty"));
    }

    #[test]
    fn a_sibling_cwd_that_re_encodes_proves_the_directory() {
        let dir = Path::new("/store/projects/Users-me-code-test-1");
        let cwds = vec![PathBuf::from("/Users/me/code/test_1")];
        let (path, origin) = resolve_dir(dir, &cwds);
        assert_eq!(origin, ProjectOrigin::TranscriptExact);
        assert_eq!(path, PathBuf::from("/Users/me/code/test_1"));

        // A cwd that does NOT re-encode is a real directory, but not provably this one's.
        let (path, origin) = resolve_dir(dir, &[PathBuf::from("/Users/me/elsewhere")]);
        assert_eq!(origin, ProjectOrigin::TranscriptCwd);
        assert_eq!(path, PathBuf::from("/Users/me/elsewhere"));
        assert!(!origin.is_exact());
    }

    #[test]
    fn a_malformed_session_id_never_reaches_a_command_line() {
        let mut sess = ToolSession {
            id: "; rm -rf /".into(),
            source: HistorySource::Cursor,
            project: std::env::temp_dir(),
            project_origin: ProjectOrigin::TranscriptExact,
            branch: None,
            started_at: None,
            summary: String::new(),
            first_user: String::new(),
            message_count: 0,
            full_text: String::new(),
        };
        let p = CursorProvider::new();
        assert!(matches!(
            p.resume(&sess).blocked(),
            Some(ResumeBlocked::BadSessionId { .. })
        ));

        // …and a Claude conversation is not this provider's to resume.
        sess.id = "aaaa-bbbb".into();
        sess.source = HistorySource::Claude;
        assert!(matches!(
            p.resume(&sess).blocked(),
            Some(ResumeBlocked::Unsupported { tool_id: "claude" })
        ));
    }

    #[test]
    fn a_missing_store_scans_to_nothing() {
        let missing = std::env::temp_dir().join(format!("hp-cursor-none-{}", uuid::Uuid::new_v4()));
        assert!(!store_exists(&missing));
        let mut p = CursorProvider::with_root(&missing);
        assert!(p.scan().is_empty());
        assert_eq!(p.last_scan_parsed(), 0);
    }

    #[test]
    fn the_title_outranks_the_opening_prompt() {
        let meta = ChatMeta {
            cwd: PathBuf::from("/w/x"),
            title: Some("  Disk   Space Check ".into()),
            created_at_ms: Some(1),
            updated_at_ms: Some(9),
            has_conversation: true,
        };
        let t = Transcript {
            first_user: "check disk space".into(),
            ..Transcript::default()
        };
        let r = row(
            "id-1234",
            Some(&meta),
            Some(&t),
            PathBuf::from("/w/x"),
            ProjectOrigin::TranscriptExact,
        );
        assert_eq!(r.summary, "Disk Space Check");
        assert_eq!(r.first_user, "check disk space");
        assert_eq!(r.started_at, Some(9));
        assert_eq!(r.branch, None);

        // With no title, the opening prompt is the label.
        let untitled = ChatMeta {
            title: None,
            ..meta.clone()
        };
        let r = row(
            "id-1234",
            Some(&untitled),
            Some(&t),
            PathBuf::from("/w/x"),
            ProjectOrigin::TranscriptExact,
        );
        assert_eq!(r.summary, "check disk space");
    }

    #[test]
    fn a_project_heading_never_shows_two_provenances() {
        let mk = |origin| ToolSession {
            id: format!("{origin:?}"),
            source: HistorySource::Cursor,
            project: PathBuf::from("/w/x"),
            project_origin: origin,
            branch: None,
            started_at: None,
            summary: String::new(),
            first_user: String::new(),
            message_count: 0,
            full_text: String::new(),
        };
        let mut rows = vec![
            mk(ProjectOrigin::ProbedExact),
            mk(ProjectOrigin::TranscriptCwd),
        ];
        unify_origins(&mut rows);
        assert!(rows
            .iter()
            .all(|r| r.project_origin == ProjectOrigin::ProbedExact));
    }

    #[test]
    fn a_pruned_transcript_still_leaves_a_row() {
        let store = temp_root("pruned");
        let project = temp_root("pruned-proj");
        write_chat(
            &store,
            &project,
            "aaaa-bbbb-cccc-dddd",
            Some("Only Metadata"),
            true,
        );
        // …but an abandoned window with no conversation is not a row at all.
        write_chat(&store, &project, "eeee-ffff-1111-2222", None, false);

        let mut p = CursorProvider::with_root(&store);
        let rows = p.scan();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary, "Only Metadata");
        assert_eq!(rows[0].message_count, 0);
        assert_eq!(rows[0].project, project);
        assert!(rows[0].project_origin.is_exact());

        let _ = std::fs::remove_dir_all(&store);
        let _ = std::fs::remove_dir_all(&project);
    }

    /// Write `chats/<dir>/<id>/meta.json`. The hash directory is Cursor's `md5(cwd)`; nothing
    /// here recomputes it, so the fixture can name it anything stable.
    fn write_chat(store: &Path, cwd: &Path, id: &str, title: Option<&str>, has: bool) {
        let dir = store
            .join("chats")
            .join(format!("h{:x}", cwd.to_string_lossy().len()))
            .join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let title = title
            .map(|t| format!("\"title\":\"{t}\","))
            .unwrap_or_default();
        std::fs::write(
            dir.join("meta.json"),
            format!(
                "{{\"schemaVersion\":1,\"createdAtMs\":1,{title}\"updatedAtMs\":2,\
                  \"hasConversation\":{has},\"cwd\":\"{}\"}}",
                cwd.to_string_lossy()
            ),
        )
        .unwrap();
    }
}
