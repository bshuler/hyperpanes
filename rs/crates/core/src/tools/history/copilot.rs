//! The GitHub Copilot CLI [`SessionProvider`] — one SQLite table where Claude has a tree.
//!
//! Copilot keeps a single global store at `~/.copilot/session-store.db`: one `sessions` row
//! per conversation carrying `cwd`, `repository`, `branch`, `summary` and both timestamps,
//! plus one `turns` row per prompt/response pair. That deletes the hard half of `claude.rs`
//! — there is no lossy directory encoding to invert, because the working directory was
//! written down verbatim — and replaces it with a different hazard: the store is *live*.
//! The human can be mid-conversation while the panel re-scans every 20 s, so every read
//! here goes through `mode=ro&immutable=1` (see [`open_immutable`], which states what that
//! costs). The per-session `~/.copilot/session-state/<uuid>/` directories are deliberately
//! not touched: `events.jsonl` is ~80 KB apiece across ~900 sessions and carries nothing
//! the `sessions` row does not already say.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use super::{
    check_project, resolve_program, sort_for_panel, ResumeBlocked, ResumeCommand, ResumePlan,
    SessionProvider, ToolSession,
};
use crate::claude_history::{HistorySource, ProjectOrigin};

/// The registry id this provider serves. A constant so the trait's `&'static str` and the
/// `detect` lookup cannot drift apart.
pub const TOOL_ID: &str = "copilot";

/// The store's filename inside `~/.copilot`.
const STORE_FILE: &str = "session-store.db";

/// Max characters kept for a session's summary line — the same budget `claude_history.rs`
/// spends, so two tools' rows elide at the same width in one shared list.
const SUMMARY_MAX: usize = 160;

/// Cap on the bounded conversation extract that backs the slow path of search. Matches
/// `claude_history.rs`: a row's text is a search corpus, not a transcript.
const FULL_TEXT_MAX: usize = 32 * 1024;

/// GitHub Copilot CLI's locally resumable conversations.
pub struct CopilotProvider {
    /// Session id -> the row we built for it, keyed by the fingerprint it was built from.
    /// The SQLite analogue of `SessionCache`'s `(mtime, size)`: `sessions.updated_at` moves
    /// on every write to a conversation, so an unchanged stamp means unchanged text.
    cache: HashMap<String, Cached>,
    overrides: BTreeMap<String, String>,
    /// A `.copilot` directory to scan instead of the machine's real one — the test seam.
    root: Option<PathBuf>,
    last_scan_parsed: usize,
}

struct Cached {
    fingerprint: String,
    session: ToolSession,
}

impl Default for CopilotProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CopilotProvider {
    /// A provider with no per-tool path override — detection falls to `PATH` and the
    /// well-known install locations.
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            overrides: BTreeMap::new(),
            root: None,
            last_scan_parsed: 0,
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

    /// Scan `root` (one `.copilot` directory) rather than the real `~/.copilot`.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
            ..Self::new()
        }
    }

    /// Sessions whose text was re-read from the store by the last
    /// [`scan`](SessionProvider::scan) — the cache effectiveness the 20 s refresh lives or
    /// dies by, surfaced so it can be asserted on.
    pub fn last_scan_parsed(&self) -> usize {
        self.last_scan_parsed
    }

    /// The database this provider reads, or `None` when no home directory is known.
    pub fn store_path(&self) -> Option<PathBuf> {
        match &self.root {
            Some(r) => Some(r.join(STORE_FILE)),
            None => copilot_root().map(|r| r.join(STORE_FILE)),
        }
    }
}

/// `~/.copilot`, the store's home. Windows first because a `HOME` set by a POSIX-ish shell
/// there is not where the CLI writes — same order `claude_history.rs` uses.
pub fn copilot_root() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".copilot"))
}

/// Whether `root` (a `.copilot` directory) holds a store worth scanning. Used by callers
/// that want to skip the whole provider when the tool has never run on this machine.
pub fn store_exists(root: &Path) -> bool {
    root.join(STORE_FILE).is_file()
}

/// Open the store the only way a background scanner may open somebody's live database.
///
/// `immutable=1` is the load-bearing half: it tells SQLite the file cannot change under it,
/// so it takes **no locks**, creates no `-shm`, and has no path by which it could checkpoint
/// or truncate the human's WAL. A plain `mode=ro` open does none of those things
/// *deliberately* either, but it does materialise the shared-memory file and take read
/// locks against a database another process is writing — and this scan runs every 20 s.
///
/// The price is real and worth stating: with `immutable=1` SQLite reads the main database
/// file **only**, so conversations that Copilot has committed to the WAL but not yet
/// checkpointed are invisible until it does. On the machine this was written against that
/// was one session out of 921 — the newest one. A row that shows up a checkpoint late beats
/// a scanner that touches a running agent's store.
fn open_immutable(db: &Path) -> Option<Connection> {
    if !db.is_file() {
        return None;
    }
    Connection::open_with_flags(
        store_uri(db),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()
}

/// A `file:` URI for `db` in the one form SQLite accepts on every platform
/// (`file:///Users/me/...`, `file:///C:/Users/me/...`). Percent-encodes everything outside
/// the unreserved set so a `?` or `#` in a directory name cannot terminate the path and
/// silently turn into query parameters.
fn store_uri(db: &Path) -> String {
    let raw = db.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file:///");
    for b in raw.trim_start_matches('/').bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out.push_str("?mode=ro&immutable=1");
    out
}

/// One `sessions` row, before its text is attached.
struct SessionRow {
    id: String,
    cwd: String,
    repository: String,
    branch: String,
    summary: String,
    created_at: String,
    updated_at: String,
}

impl SessionRow {
    /// What makes a cached row stale. `updated_at` moves on every write to the conversation;
    /// `created_at` joins it so a session id reused after a store wipe cannot look warm.
    fn fingerprint(&self) -> String {
        format!("{}|{}", self.created_at, self.updated_at)
    }
}

/// Read every `sessions` row. A store whose schema we do not recognise (an older or newer
/// Copilot) yields nothing rather than an error the panel has no way to act on.
fn read_rows(conn: &Connection) -> Vec<SessionRow> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, COALESCE(cwd,''), COALESCE(repository,''), COALESCE(branch,''), \
         COALESCE(summary,''), COALESCE(created_at,''), COALESCE(updated_at,'') FROM sessions",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok(SessionRow {
            id: r.get(0)?,
            cwd: r.get(1)?,
            repository: r.get(2)?,
            branch: r.get(3)?,
            summary: r.get(4)?,
            created_at: r.get(5)?,
            updated_at: r.get(6)?,
        })
    }) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

/// The conversation text for one session: the bounded search extract, the opening prompt,
/// and a message count.
#[derive(Default)]
struct Body {
    first_user: String,
    full_text: String,
    message_count: usize,
}

/// Pull `session_id`'s turns in order and fold them into a [`Body`].
///
/// The `substr` is not a nicety. A Copilot prompt is whatever the human piped in, and on the
/// machine this was written against `turns` holds 37 MB across 917 rows — one message of
/// 232 KB. All of it past [`FULL_TEXT_MAX`] is thrown away a few lines below, so bounding it
/// in SQL means the bytes are never read, never allocated, and never whitespace-collapsed.
/// One character past the cap keeps the truncation decision where it already lives.
fn read_body(conn: &Connection, id: &str) -> Body {
    let mut body = Body::default();
    let Ok(mut stmt) = conn.prepare_cached(
        "SELECT COALESCE(substr(user_message, 1, ?2), ''), \
         COALESCE(substr(assistant_response, 1, ?2), '') \
         FROM turns WHERE session_id = ?1 ORDER BY turn_index",
    ) else {
        return body;
    };
    let cap = (FULL_TEXT_MAX + 1) as i64;
    let Ok(rows) = stmt.query_map(rusqlite::params![id, cap], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    }) else {
        return body;
    };
    for (user, assistant) in rows.flatten() {
        // Counted per message, not per turn, so the number means the same thing next to a
        // Claude row (which counts transcript records).
        if !user.trim().is_empty() {
            if body.first_user.is_empty() {
                body.first_user = clean_line(&user);
            }
            body.message_count += 1;
            push_full_text(&mut body.full_text, &user);
        }
        if !assistant.trim().is_empty() {
            body.message_count += 1;
            push_full_text(&mut body.full_text, &assistant);
        }
    }
    body
}

/// The project a row belongs to, and how honestly we know it.
///
/// `sessions.cwd` is the working directory Copilot recorded at session start, verbatim —
/// nothing was encoded, so nothing has to be decoded, and an absolute one is
/// [`ProjectOrigin::TranscriptExact`] for the same reason Claude's re-encoded `cwd` is:
/// it is a record, not a reconstruction. When it is missing or relative we fall back to the
/// `owner/name` repository slug, which is a *label* — [`ProjectOrigin::DecodedUnverified`],
/// so [`check_project`] refuses to spawn in it and the panel greys the row. A session with
/// neither has nothing to group under and no path to resume into, so it is dropped.
fn project_for(row: &SessionRow) -> Option<(PathBuf, ProjectOrigin)> {
    let cwd = Path::new(row.cwd.trim());
    if !row.cwd.trim().is_empty() && cwd.is_absolute() {
        return Some((cwd.to_path_buf(), ProjectOrigin::TranscriptExact));
    }
    if !row.repository.trim().is_empty() {
        return Some((
            PathBuf::from(row.repository.trim()),
            ProjectOrigin::DecodedUnverified,
        ));
    }
    None
}

/// Widen one row plus its text into a panel row.
fn to_session(
    row: &SessionRow,
    project: PathBuf,
    origin: ProjectOrigin,
    body: Body,
) -> ToolSession {
    let summary = {
        let s = clean_line(&row.summary);
        if s.is_empty() {
            body.first_user.clone()
        } else {
            s
        }
    };
    ToolSession {
        id: row.id.clone(),
        source: HistorySource::Copilot,
        project,
        project_origin: origin,
        // The branch the conversation happened on, not the one the tree is on now.
        branch: Some(row.branch.trim().to_string()).filter(|b| !b.is_empty()),
        // `updated_at`, not `created_at`, despite the field's name: the panel orders
        // most-recently-touched first, and `claude.rs` fills this slot with the transcript's
        // mtime — last activity — for exactly that reason.
        started_at: epoch_ms(&row.updated_at).or_else(|| epoch_ms(&row.created_at)),
        summary,
        first_user: body.first_user,
        message_count: body.message_count,
        full_text: body.full_text,
    }
}

impl SessionProvider for CopilotProvider {
    fn id(&self) -> &'static str {
        TOOL_ID
    }

    fn scan(&mut self) -> Vec<ToolSession> {
        self.last_scan_parsed = 0;
        let Some(db) = self.store_path() else {
            return Vec::new();
        };
        let Some(conn) = open_immutable(&db) else {
            return Vec::new();
        };

        let rows = read_rows(&conn);
        // Rebuilt rather than updated in place, so sessions the human deleted from the store
        // drop out of the cache instead of accumulating for the life of the process.
        let mut fresh: HashMap<String, Cached> = HashMap::with_capacity(rows.len());
        let mut out: Vec<ToolSession> = Vec::with_capacity(rows.len());

        for row in &rows {
            let Some((project, origin)) = project_for(row) else {
                continue;
            };
            let fingerprint = row.fingerprint();
            let session = match self.cache.get(&row.id) {
                Some(c) if c.fingerprint == fingerprint => c.session.clone(),
                _ => {
                    self.last_scan_parsed += 1;
                    to_session(row, project, origin, read_body(&conn, &row.id))
                }
            };
            out.push(session.clone());
            fresh.insert(
                row.id.clone(),
                Cached {
                    fingerprint,
                    session,
                },
            );
        }

        self.cache = fresh;
        sort_for_panel(&mut out);
        out
    }

    fn resume(&self, session: &ToolSession) -> ResumePlan {
        if session.source != HistorySource::Copilot {
            return ResumePlan::Blocked(ResumeBlocked::Unsupported {
                tool_id: session.source.tool_id(),
            });
        }
        // Copilot session ids are UUIDs and the store is a file the user (or anything
        // running as them) can write, so the id is re-validated before it can land on a
        // command line — the same gate `claude.rs` puts in front of `--resume`.
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
        ResumePlan::Ready(ResumeCommand {
            program,
            // `-r, --resume[=value]` — an *optional*-value option, so whether a
            // space-separated id binds to it was worth checking rather than assuming.
            // It does: `copilot --resume <uuid> mcp --help` runs the `mcp` subcommand,
            // where the same line without `--resume` treats the uuid as a command.
            args: vec!["--resume".to_string(), session.id.clone()],
            cwd,
        })
    }
}

// ===== small local mirrors of `claude_history.rs`'s private text helpers =====

/// Collapse all whitespace to single spaces, trim, truncate to [`SUMMARY_MAX`] characters.
fn clean_line(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= SUMMARY_MAX {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(SUMMARY_MAX - 1).collect();
    out.push('…');
    out
}

/// Append one message to the bounded, lowercased, whitespace-collapsed search extract.
/// Stored lowercased because search never re-lowers it.
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
    } else {
        let mut cut = remaining;
        while cut > 0 && !collapsed.is_char_boundary(cut) {
            cut -= 1;
        }
        full.push_str(&collapsed[..cut]);
    }
}

// ===== timestamps =====

/// Epoch milliseconds from the two shapes this store actually holds, both UTC: Copilot's own
/// `2026-08-19T11:52:05.443Z`, and `2026-08-19 11:52:05` — what the columns' own
/// `DEFAULT (datetime('now'))` writes when the CLI omits the value. Parsed by position
/// rather than inferred: anything else is `None`, and a row with no usable stamp sorts last
/// instead of sorting wrong.
fn epoch_ms(s: &str) -> Option<u64> {
    let b = s.trim().as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |from: usize, len: usize| -> Option<i64> {
        let slice = b.get(from..from + len)?;
        if !slice.iter().all(|c| c.is_ascii_digit()) {
            return None;
        }
        std::str::from_utf8(slice).ok()?.parse().ok()
    };
    if b[4] != b'-'
        || b[7] != b'-'
        || (b[10] != b'T' && b[10] != b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (h, mi, sec) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Fractional seconds are optional and Copilot writes exactly three digits.
    let millis = if b.get(19) == Some(&b'.') {
        num(20, 3).unwrap_or(0)
    } else {
        0
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3_600 + mi * 60 + sec;
    if secs < 0 {
        return None;
    }
    u64::try_from(secs * 1_000 + millis).ok()
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
/// Hand-rolled because the crate has no date dependency and is not getting one for this.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hp-copilot-{}-{tag}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A store with the columns the real one has, created the only way a fixture can: by
    /// writing it. Nothing here ever runs against `~/.copilot`.
    fn make_store(root: &Path) -> Connection {
        let conn = Connection::open(root.join(STORE_FILE)).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, \
               host_type TEXT, branch TEXT, summary TEXT, created_at TEXT, updated_at TEXT);
             CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, \
               turn_index INTEGER NOT NULL, user_message TEXT, assistant_response TEXT, \
               timestamp TEXT, UNIQUE(session_id, turn_index));",
        )
        .unwrap();
        conn
    }

    fn insert(conn: &Connection, id: &str, cwd: &str, branch: &str, at: &str, prompt: &str) {
        conn.execute(
            "INSERT INTO sessions (id, cwd, repository, host_type, branch, summary, created_at, \
             updated_at) VALUES (?1, ?2, 'me/repo', 'github', ?3, NULL, ?4, ?4)",
            rusqlite::params![id, cwd, branch, at],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO turns (session_id, turn_index, user_message, assistant_response, \
             timestamp) VALUES (?1, 0, ?2, 'on it', ?3)",
            rusqlite::params![id, prompt, at],
        )
        .unwrap();
    }

    #[test]
    fn scans_every_project_and_sorts_for_the_panel() {
        let root = temp_root("scan");
        let conn = make_store(&root);
        insert(
            &conn,
            "1111-2222-3333",
            "/w/beta",
            "main",
            "2026-08-19T11:00:00.000Z",
            "beta one",
        );
        insert(
            &conn,
            "4444-5555-6666",
            "/w/alpha",
            "dev",
            "2026-08-19T10:00:00.000Z",
            "alpha old",
        );
        insert(
            &conn,
            "7777-8888-9999",
            "/w/alpha",
            "dev",
            "2026-08-19T12:00:00.000Z",
            "alpha new",
        );
        drop(conn);

        let mut p = CopilotProvider::with_root(&root);
        let rows = p.scan();
        assert_eq!(p.id(), "copilot");
        assert_eq!(rows.len(), 3);

        let seen: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.project.to_str().unwrap(), r.id.as_str()))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("/w/alpha", "7777-8888-9999"),
                ("/w/alpha", "4444-5555-6666"),
                ("/w/beta", "1111-2222-3333"),
            ]
        );
        assert!(rows.iter().all(|r| r.source == HistorySource::Copilot));
        assert!(rows
            .iter()
            .all(|r| r.project_origin == ProjectOrigin::TranscriptExact));
        assert_eq!(rows[0].branch.as_deref(), Some("dev"));
        // No `summary` column value, so the opening prompt becomes the label.
        assert_eq!(rows[0].summary, "alpha new");
        assert_eq!(rows[0].message_count, 2);
        assert!(rows[0].full_text.contains("alpha new on it"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rescan_reads_no_conversation_text() {
        let root = temp_root("cache");
        let conn = make_store(&root);
        insert(
            &conn,
            "1111-2222",
            "/w/one",
            "main",
            "2026-08-19T10:00:00.000Z",
            "hello",
        );
        insert(
            &conn,
            "3333-4444",
            "/w/two",
            "main",
            "2026-08-19T10:00:00.000Z",
            "hello",
        );
        drop(conn);

        let mut p = CopilotProvider::with_root(&root);
        let first = p.scan();
        assert_eq!(
            p.last_scan_parsed(),
            2,
            "cold scan reads every session's turns"
        );
        let second = p.scan();
        assert_eq!(second, first);
        assert_eq!(
            p.last_scan_parsed(),
            0,
            "the 20 s refresh must stay O(new work)"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resume_is_the_verified_flag_in_the_sessions_own_cwd() {
        let root = temp_root("resume");
        let project = temp_root("proj");
        let conn = make_store(&root);
        insert(
            &conn,
            "aaaa-bbbb-cccc",
            &project.to_string_lossy(),
            "main",
            "2026-08-19T10:00:00.000Z",
            "hi",
        );
        drop(conn);

        let mut ov = BTreeMap::new();
        ov.insert(TOOL_ID.to_string(), "/opt/tools/copilot".to_string());
        let mut p = CopilotProvider {
            root: Some(root.clone()),
            ..CopilotProvider::with_overrides(ov)
        };
        let rows = p.scan();
        let cmd = p
            .resume(&rows[0])
            .command()
            .cloned()
            .expect("a real project");
        assert_eq!(cmd.program, PathBuf::from("/opt/tools/copilot"));
        assert_eq!(cmd.args, vec!["--resume", "aaaa-bbbb-cccc"]);
        assert_eq!(cmd.cwd, project);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn a_missing_store_scans_to_nothing() {
        let root = temp_root("empty");
        assert!(!store_exists(&root));
        let mut p = CopilotProvider::with_root(&root);
        assert!(p.scan().is_empty());
        assert_eq!(p.last_scan_parsed(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn timestamps_parse_both_shapes_the_store_holds() {
        // Copilot's own ISO-8601 with milliseconds and a Z.
        assert_eq!(
            epoch_ms("2026-08-19T11:52:05.443Z"),
            Some(1_787_140_325_443)
        );
        // What `DEFAULT (datetime('now'))` writes.
        assert_eq!(epoch_ms("2026-08-19 11:52:05"), Some(1_787_140_325_000));
        assert_eq!(epoch_ms("1970-01-01T00:00:00.000Z"), Some(0));
        // Leap day, to prove the civil-days arithmetic rather than assume it.
        assert_eq!(epoch_ms("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
        for bad in [
            "",
            "not a date",
            "2026-13-01T00:00:00Z",
            "2026-08-19T99:00:00Z",
        ] {
            assert_eq!(epoch_ms(bad), None, "{bad} should not parse");
        }
    }

    #[test]
    fn the_store_uri_never_lets_a_path_become_a_query() {
        let uri = store_uri(Path::new("/tmp/what?now/session-store.db"));
        assert!(uri.starts_with("file:///tmp/what%3Fnow/"));
        assert!(uri.ends_with("?mode=ro&immutable=1"));
        // Exactly one `?`: the one that starts our own parameters.
        assert_eq!(uri.matches('?').count(), 1);
    }
}
