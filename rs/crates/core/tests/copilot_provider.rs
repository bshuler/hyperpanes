//! **GitHub Copilot CLI's conversations, and the path back into one.**
//!
//! Copilot's store is the opposite shape to Claude's, and the tests follow that. There is
//! no lossy directory encoding here to invert: `~/.copilot/session-store.db` holds one
//! `sessions` row per conversation with the working directory written down *verbatim*, so
//! the provenance question is not "can this path be reconstructed" but "did the row
//! actually say". So these tests are about the three places that can still go wrong:
//!
//!   * **Provenance.** An absolute `cwd` off the row is a record, not a reconstruction, and
//!     earns [`ProjectOrigin::TranscriptExact`]. A row that only knows its `owner/name`
//!     repository slug gets that slug as a *label* — `DecodedUnverified`, which
//!     `check_project` refuses to spawn in. A row with neither is dropped rather than
//!     guessed at.
//!   * **Order.** The panel draws a project heading wherever the project changes between
//!     adjacent rows, so the provider owes it `sort_for_panel`'s exact order.
//!   * **Cost.** The panel re-scans every 20 s. A re-scan must re-read no conversation text.
//!
//! Fixture stores throughout, built with `rusqlite` in a temp dir — nothing here reads the
//! machine's real `~/.copilot`, except the `#[ignore]`d reporter at the bottom, which
//! prints numbers rather than asserting them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hyperpanes_core::claude_history::{HistorySource, ProjectOrigin};
use hyperpanes_core::tools::history::copilot::{store_exists, CopilotProvider, TOOL_ID};
use hyperpanes_core::tools::history::{ResumeBlocked, SessionProvider, ToolSession};
use rusqlite::{params, Connection};

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hp-copilot-it-{}-{tag}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A `.copilot` directory holding a store with the real schema's columns. Written with a
/// plain read-write connection and closed before any scan, so the fixture never leaves a
/// WAL behind — the provider reads `immutable=1` and would not see one.
fn store(root: &Path) -> Connection {
    let conn = Connection::open(root.join("session-store.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, host_type TEXT, \
           branch TEXT, summary TEXT, created_at TEXT DEFAULT (datetime('now')), \
           updated_at TEXT DEFAULT (datetime('now')));
         CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, \
           turn_index INTEGER NOT NULL, user_message TEXT, assistant_response TEXT, \
           timestamp TEXT DEFAULT (datetime('now')), UNIQUE(session_id, turn_index));",
    )
    .unwrap();
    conn
}

/// One session row. `cwd` / `repository` are passed as-is (either may be empty) so the
/// provenance cases can be written literally.
fn session(conn: &Connection, id: &str, cwd: &str, repo: &str, at: &str) {
    conn.execute(
        "INSERT INTO sessions (id, cwd, repository, host_type, branch, summary, created_at, \
         updated_at) VALUES (?1, ?2, ?3, 'github', 'main', NULL, ?4, ?4)",
        params![id, cwd, repo, at],
    )
    .unwrap();
}

/// Append a prompt/response pair, and move the session's `updated_at` with it the way the
/// real CLI does — that stamp is the provider's whole cache fingerprint.
fn turn(conn: &Connection, id: &str, index: i64, user: &str, assistant: &str, at: &str) {
    conn.execute(
        "INSERT INTO turns (session_id, turn_index, user_message, assistant_response, timestamp) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, index, user, assistant, at],
    )
    .unwrap();
    conn.execute(
        "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
        params![id, at],
    )
    .unwrap();
}

const T0: &str = "2026-08-19T10:00:00.000Z";
const T1: &str = "2026-08-19T11:00:00.000Z";
const T2: &str = "2026-08-19T12:00:00.000Z";

// ===== 1. the project path, and how honestly we know it =====

#[test]
fn an_absolute_cwd_off_the_row_is_a_record_not_a_reconstruction() {
    let root = temp_dir("origin-exact");
    let conn = store(&root);
    session(&conn, "aaaa-1111", "/w/repo", "me/repo", T0);
    turn(&conn, "aaaa-1111", 0, "make it faster", "on it", T0);
    drop(conn);

    let mut p = CopilotProvider::with_root(&root);
    let rows = p.scan();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].project, PathBuf::from("/w/repo"));
    assert_eq!(rows[0].project_origin, ProjectOrigin::TranscriptExact);
    assert!(rows[0].project_origin.is_exact());
    assert_eq!(rows[0].source, HistorySource::Copilot);
    assert_eq!(rows[0].branch.as_deref(), Some("main"));
    assert_eq!(rows[0].first_user, "make it faster");
    assert_eq!(
        rows[0].summary, "make it faster",
        "no summary column, so the prompt labels it"
    );
    assert_eq!(rows[0].message_count, 2);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_repository_slug_is_a_label_and_can_never_be_spawned_in() {
    let root = temp_dir("origin-label");
    let conn = store(&root);
    // No cwd: all we know is which repo it was. That names the row; it does not locate it.
    session(&conn, "bbbb-2222", "", "me/repo", T0);
    turn(&conn, "bbbb-2222", 0, "hello", "hi", T0);
    // A relative cwd is no better than none — resolving it would mean inventing a base.
    session(&conn, "cccc-3333", "docs/plans", "me/other", T0);
    turn(&conn, "cccc-3333", 0, "hello", "hi", T0);
    drop(conn);

    let mut p = CopilotProvider::with_root(&root);
    let rows = p.scan();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert_eq!(r.project_origin, ProjectOrigin::DecodedUnverified);
        assert!(!r.project_origin.is_exact());
        assert!(matches!(
            p.resume(r).blocked(),
            Some(ResumeBlocked::ProjectUnverified { .. })
        ));
    }
    let labels: Vec<&str> = rows.iter().map(|r| r.project.to_str().unwrap()).collect();
    assert_eq!(labels, vec!["me/other", "me/repo"]);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_row_with_no_cwd_and_no_repository_is_dropped_not_guessed_at() {
    let root = temp_dir("origin-none");
    let conn = store(&root);
    session(&conn, "dddd-4444", "", "", T0);
    turn(&conn, "dddd-4444", 0, "hello", "hi", T0);
    session(&conn, "eeee-5555", "/w/real", "me/real", T0);
    turn(&conn, "eeee-5555", 0, "hello", "hi", T0);
    drop(conn);

    let mut p = CopilotProvider::with_root(&root);
    let rows = p.scan();
    // Nothing to group it under and no path to resume into: a row that could only ever be
    // greyed with an empty heading is noise, not information.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "eeee-5555");

    let _ = std::fs::remove_dir_all(&root);
}

// ===== 2. the order the panel's grouping depends on =====

#[test]
fn projects_ascend_sessions_descend_and_no_heading_repeats() {
    let root = temp_dir("order");
    let conn = store(&root);
    session(&conn, "1111-aaaa", "/w/beta", "me/beta", T1);
    session(&conn, "2222-bbbb", "/w/alpha", "me/alpha", T0);
    session(&conn, "3333-cccc", "/w/beta", "me/beta", T2);
    session(&conn, "4444-dddd", "/w/alpha", "me/alpha", T2);
    for id in ["1111-aaaa", "2222-bbbb", "3333-cccc", "4444-dddd"] {
        turn(&conn, id, 0, "hello", "hi", T0);
    }
    // `turn` moved every `updated_at` to T0; restore the intended spread.
    for (id, at) in [
        ("1111-aaaa", T1),
        ("2222-bbbb", T0),
        ("3333-cccc", T2),
        ("4444-dddd", T2),
    ] {
        conn.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![id, at],
        )
        .unwrap();
    }
    drop(conn);

    let mut p = CopilotProvider::with_root(&root);
    let rows = p.scan();
    let seen: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| (r.project.to_str().unwrap(), r.id.as_str()))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("/w/alpha", "4444-dddd"),
            ("/w/alpha", "2222-bbbb"),
            ("/w/beta", "3333-cccc"),
            ("/w/beta", "1111-aaaa"),
        ]
    );

    // The panel emits a heading on every project change and does no grouping of its own.
    let mut headings: Vec<&Path> = Vec::new();
    for r in &rows {
        if headings.last() != Some(&r.project.as_path()) {
            headings.push(&r.project);
        }
    }
    assert_eq!(headings.len(), 2, "a project heading repeated");

    let _ = std::fs::remove_dir_all(&root);
}

// ===== 3. the cost of the 20 s refresh =====

#[test]
fn a_rescan_reads_no_conversation_text_but_a_changed_session_does() {
    let root = temp_dir("warm");
    let conn = store(&root);
    for (i, id) in ["1111-aaaa", "2222-bbbb", "3333-cccc"].iter().enumerate() {
        session(&conn, id, &format!("/w/p{i}"), "me/repo", T0);
        turn(&conn, id, 0, "hello", "hi", T0);
    }
    drop(conn);

    let mut p = CopilotProvider::with_root(&root);
    let cold = p.scan();
    assert_eq!(p.last_scan_parsed(), 3);
    assert_eq!(p.scan(), cold);
    assert_eq!(
        p.last_scan_parsed(),
        0,
        "an unchanged store must cost no reads"
    );

    // One conversation continues. Only that one is re-read.
    let conn = Connection::open(root.join("session-store.db")).unwrap();
    turn(&conn, "2222-bbbb", 1, "and now this", "done", T2);
    drop(conn);

    let warm = p.scan();
    assert_eq!(
        p.last_scan_parsed(),
        1,
        "only the changed session is re-read"
    );
    let changed = warm.iter().find(|r| r.id == "2222-bbbb").unwrap();
    assert_eq!(changed.message_count, 4);
    assert!(changed.full_text.contains("and now this"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_session_deleted_from_the_store_leaves_the_cache_too() {
    let root = temp_dir("prune");
    let conn = store(&root);
    session(&conn, "1111-aaaa", "/w/one", "me/repo", T0);
    turn(&conn, "1111-aaaa", 0, "hello", "hi", T0);
    session(&conn, "2222-bbbb", "/w/two", "me/repo", T0);
    turn(&conn, "2222-bbbb", 0, "hello", "hi", T0);
    drop(conn);

    let mut p = CopilotProvider::with_root(&root);
    assert_eq!(p.scan().len(), 2);

    let conn = Connection::open(root.join("session-store.db")).unwrap();
    conn.execute("DELETE FROM turns WHERE session_id = '1111-aaaa'", [])
        .unwrap();
    conn.execute("DELETE FROM sessions WHERE id = '1111-aaaa'", [])
        .unwrap();
    drop(conn);

    let rows = p.scan();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "2222-bbbb");
    assert_eq!(p.last_scan_parsed(), 0);

    let _ = std::fs::remove_dir_all(&root);
}

// ===== 4. every blocked reason this provider can actually reach =====

#[test]
fn resume_is_the_resume_flag_in_the_sessions_own_cwd() {
    let root = temp_dir("resume");
    let project = temp_dir("resume-proj");
    let conn = store(&root);
    session(
        &conn,
        "aaaa-bbbb-cccc",
        &project.to_string_lossy(),
        "me/repo",
        T0,
    );
    turn(&conn, "aaaa-bbbb-cccc", 0, "hello", "hi", T0);
    drop(conn);

    let mut ov = BTreeMap::new();
    ov.insert(TOOL_ID.to_string(), "/opt/tools/copilot".to_string());
    let p = CopilotProvider::with_overrides(ov);
    // `with_overrides` and `with_root` are separate constructors; the field is private, so
    // the test seam is reached by scanning the fixture through a second provider and
    // resuming the row it produced — the plan depends on the row, not on how it was found.
    let rows = CopilotProvider::with_root(&root).scan();
    assert_eq!(rows.len(), 1);
    let plan = p.resume(&rows[0]);
    let cmd = plan
        .command()
        .expect("a real project and a resolvable binary");
    assert_eq!(cmd.program, PathBuf::from("/opt/tools/copilot"));
    assert_eq!(cmd.args, vec!["--resume", "aaaa-bbbb-cccc"]);
    assert_eq!(cmd.cwd, project);
    assert_eq!(
        cmd.shell_line(),
        "/opt/tools/copilot --resume aaaa-bbbb-cccc"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_deleted_project_blocks_instead_of_spawning() {
    let root = temp_dir("gone");
    let project = temp_dir("gone-proj");
    let conn = store(&root);
    session(
        &conn,
        "aaaa-bbbb",
        &project.to_string_lossy(),
        "me/repo",
        T0,
    );
    turn(&conn, "aaaa-bbbb", 0, "hello", "hi", T0);
    drop(conn);

    let mut p = CopilotProvider::with_root(&root);
    let rows = p.scan();
    // The worktree goes away; the store outlives it.
    std::fs::remove_dir_all(&project).unwrap();

    let plan = p.resume(&rows[0]);
    assert!(matches!(
        plan.blocked(),
        Some(ResumeBlocked::ProjectMissing { .. })
    ));
    assert!(plan
        .blocked()
        .unwrap()
        .reason()
        .contains("no longer exists"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_malformed_id_and_a_foreign_session_never_reach_a_command_line() {
    let mut sess = ToolSession {
        id: "; rm -rf /".into(),
        source: HistorySource::Copilot,
        project: std::env::temp_dir(),
        project_origin: ProjectOrigin::TranscriptExact,
        branch: None,
        started_at: None,
        summary: String::new(),
        first_user: String::new(),
        message_count: 0,
        full_text: String::new(),
    };
    let p = CopilotProvider::new();
    assert!(matches!(
        p.resume(&sess).blocked(),
        Some(ResumeBlocked::BadSessionId { .. })
    ));

    // Source is checked before the id, so a Claude row is refused as another tool's, not as
    // a malformed one.
    sess.source = HistorySource::Claude;
    assert!(matches!(
        p.resume(&sess).blocked(),
        Some(ResumeBlocked::Unsupported { tool_id: "claude" })
    ));

    // `ToolNotInstalled` is the one variant no fixture can force: an override is taken at
    // face value by `tools::detect`, so reaching it means genuinely having no `copilot` on
    // the machine — which is a property of the machine, not of a test.
}

// ===== 5. absence, and the real store =====

#[test]
fn a_machine_without_copilot_scans_to_nothing() {
    let missing = std::env::temp_dir().join(format!("hp-copilot-none-{}", uuid::Uuid::new_v4()));
    assert!(!store_exists(&missing));
    let mut p = CopilotProvider::with_root(&missing);
    assert!(p.scan().is_empty());
    assert_eq!(p.last_scan_parsed(), 0);

    // An empty directory is the same answer: a `.copilot` with no store is not a store.
    let empty = temp_dir("empty");
    assert!(!store_exists(&empty));
    assert!(CopilotProvider::with_root(&empty).scan().is_empty());
    let _ = std::fs::remove_dir_all(&empty);
}

/// Numbers, not assertions — run it deliberately:
/// `cargo test -p hyperpanes-core --test copilot_provider -- --ignored --nocapture`
#[test]
#[ignore = "reports numbers from the machine's real ~/.copilot store"]
fn report_real_store_numbers() {
    let Some(root) = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".copilot")) else {
        eprintln!("no home directory — nothing to report");
        return;
    };
    if !store_exists(&root) {
        eprintln!("no ~/.copilot/session-store.db on this machine — nothing to report");
        return;
    }

    let mut p = CopilotProvider::with_root(&root);
    let started = std::time::Instant::now();
    let rows = p.scan();
    let cold = started.elapsed();
    let cold_parsed = p.last_scan_parsed();
    let started = std::time::Instant::now();
    let again = p.scan();
    let warm = started.elapsed();

    let mut by_origin: BTreeMap<String, usize> = BTreeMap::new();
    let mut projects: Vec<&Path> = Vec::new();
    for r in &rows {
        *by_origin
            .entry(format!("{:?}", r.project_origin))
            .or_default() += 1;
        if projects.last() != Some(&r.project.as_path()) {
            projects.push(&r.project);
        }
    }
    let ready = rows.iter().filter(|r| p.resume(r).is_ready()).count();

    println!("sessions                      : {}", rows.len());
    println!("projects                      : {}", projects.len());
    println!("  resumable right now         : {ready}");
    println!("project path provenance       : {by_origin:?}");
    println!(
        "cold scan {cold:?} ({cold_parsed} read) -> warm scan {warm:?} ({} read)",
        p.last_scan_parsed()
    );
    assert_eq!(again.len(), rows.len());
}
