//! **Cursor's conversations, and the path back into one.**
//!
//! Cursor keeps the same conversation in two places, and neither is sufficient:
//!
//! ```text
//! ~/.cursor/chats/<md5(cwd)>/<id>/meta.json   <- exact cwd, title, timestamps; no text
//! ~/.cursor/projects/<Encoded-Cwd>/agent-transcripts/<id>/<id>.jsonl   <- text; no cwd
//! ```
//!
//! The transcript tree is filed under the *same lossy encoding* Claude Code uses — `/`, `.`
//! and `_` all collapse to `-`, runs are not merged — and Cursor's JSONL records **no `cwd`
//! and no git branch at all**. So where Claude's provider reads a `cwd` out of the transcript,
//! this one reads it out of the session's own `meta.json`, and falls back to a filesystem
//! probe when Cursor has swept the chat entry away (it does; it leaves
//! `.agent-data-cleanup-<date>` markers behind).
//!
//! These tests are therefore about *provenance* first and counts second:
//!
//!   * A per-session `meta.json` `cwd` is proof — nothing lossy stands between it and the
//!     conversation.
//!   * A filesystem probe that finds a real directory re-encoding to the name is proof of the
//!     same kind, and recovers `what_is_light` where no substitution rule could.
//!   * Everything else — Cursor's literal `empty-window` sentinel included — is a label, and
//!     [`ResumePlan`](hyperpanes_core::tools::history::ResumePlan) must refuse to spawn on it.
//!
//! Fixtures throughout, except the two absence-tolerant checks at the bottom (one `#[ignore]`d,
//! because it reports numbers rather than asserting them).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hyperpanes_core::claude_history::{HistorySource, ProjectOrigin};
use hyperpanes_core::tools::history::cursor::{
    cursor_root, encode_project_dir_cursor, store_exists, TOOL_ID,
};
use hyperpanes_core::tools::history::{
    CursorProvider, ResumeBlocked, SessionProvider, ToolSession,
};

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hp-cursorprov-{}-{tag}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A real directory to point sessions at, under a *canonicalised* temp root. Canonical
/// because probing walks the filesystem from `/` down, and on macOS the uncanonicalised
/// temp path runs through the `/var -> /private/var` symlink — the probe resolves it either
/// way, but only the canonical form compares equal to what it returns.
fn real_project(tag: &str, leaf: &str) -> PathBuf {
    let base = std::fs::canonicalize(temp_dir(tag)).unwrap();
    let p = base.join(leaf);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// A transcript shaped like the real thing: Cursor wraps the human's prompt in
/// `<timestamp>`/`<user_query>` tags, splits content into typed blocks, and closes the turn
/// with a record that carries no text at all.
fn transcript(prompt: &str, reply: &str) -> String {
    format!(
        "{{\"role\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\
           \"text\":\"<timestamp>Friday, Aug 28, 2026, 11:06 AM (UTC-4)</timestamp>\\n\
           <user_query>\\n{prompt}\\n</user_query>\"}}]}}}}\n\
         {{\"role\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[\
           {{\"type\":\"text\",\"text\":\"{reply}\"}},\
           {{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{{\"command\":\"df -h\"}}}}]}}}}\n\
         {{\"type\":\"turn_ended\",\"status\":\"success\"}}\n"
    )
}

/// Write `store/projects/<dir>/agent-transcripts/<id>/<id>.jsonl`. `dir` is the encoded name
/// verbatim, so a test can hand over a name that decodes to nothing.
fn write_transcript(store: &Path, dir: &str, id: &str, body: &str) -> PathBuf {
    let d = store
        .join("projects")
        .join(dir)
        .join("agent-transcripts")
        .join(id);
    std::fs::create_dir_all(&d).unwrap();
    let path = d.join(format!("{id}.jsonl"));
    std::fs::write(&path, body).unwrap();
    path
}

/// Write `store/chats/<hash>/<id>/meta.json`. The real hash is `md5(cwd)`; nothing in the
/// provider recomputes it, so the fixture only needs it to be stable and distinct.
fn write_meta(store: &Path, id: &str, cwd: &Path, title: Option<&str>, updated: u64) {
    let d = store.join("chats").join(format!("h{}", &id[..2])).join(id);
    std::fs::create_dir_all(&d).unwrap();
    let title = title
        .map(|t| format!("\"title\":\"{t}\","))
        .unwrap_or_default();
    std::fs::write(
        d.join("meta.json"),
        format!(
            "{{\"schemaVersion\":1,\"createdAtMs\":1,{title}\"updatedAtMs\":{updated},\
              \"hasConversation\":true,\"cwd\":\"{}\"}}",
            cwd.to_string_lossy()
        ),
    )
    .unwrap();
}

/// The panel draws one heading per project and lists a project's sessions newest-first. That
/// only works if `scan` hands back rows already in that order — the same contract
/// `session_providers.rs` holds Claude's provider to.
fn assert_grouping_contract(rows: &[ToolSession]) {
    let mut headings: Vec<&PathBuf> = Vec::new();
    for row in rows {
        if headings.last() != Some(&&row.project) {
            assert!(
                !headings.contains(&&row.project),
                "project {} would get a second heading",
                row.project.display()
            );
            headings.push(&row.project);
        }
    }
    for w in rows.windows(2) {
        if w[0].project == w[1].project {
            assert!(
                w[0].started_at >= w[1].started_at,
                "sessions within a project must be newest-first"
            );
        } else {
            assert!(w[0].project < w[1].project, "projects must ascend");
        }
    }
}

fn id_for(n: u8) -> String {
    format!("{n:04x}{n:04x}-{n:04x}-{n:04x}-{n:04x}-{n:04x}{n:04x}{n:04x}")
}

// ===== 1. every project, in panel order =====

#[test]
fn the_provider_returns_every_project_in_panel_order() {
    let store = temp_dir("order");
    let alpha = real_project("order-a", "alpha");
    let beta = real_project("order-b", "beta");

    // Written out of order, two projects, three sessions.
    for (n, project, at) in [(1u8, &beta, 300u64), (2, &alpha, 100), (3, &alpha, 200)] {
        let id = id_for(n);
        write_meta(&store, &id, project, None, at);
        write_transcript(
            &store,
            &encode_project_dir_cursor(project),
            &id,
            &transcript(&format!("prompt {n}"), "on it"),
        );
    }

    let mut p = CursorProvider::with_root(&store);
    let rows = p.scan();
    assert_eq!(p.id(), "cursor-agent");
    assert_eq!(rows.len(), 3);
    assert_grouping_contract(&rows);

    // Every row is Cursor's, and the newest alpha session leads its group.
    assert!(rows.iter().all(|r| r.source == HistorySource::Cursor));
    let alpha_rows: Vec<&ToolSession> = rows.iter().filter(|r| r.project == alpha).collect();
    assert_eq!(alpha_rows.len(), 2);
    assert_eq!(alpha_rows[0].started_at, Some(200));
    // Cursor records no branch anywhere in either store.
    assert!(rows.iter().all(|r| r.branch.is_none()));
    // The prompt survived its wrapper tags, and the tool_use block did not become text.
    // Panel order puts the newest alpha session first.
    assert_eq!(rows[0].first_user, "prompt 3");
    assert!(rows[0].full_text.contains("on it"));
    assert!(!rows[0].full_text.contains("df -h"));
    assert_eq!(rows[0].message_count, 3);
}

// ===== 2. provenance: what the encoding destroyed, and how it comes back =====

#[test]
fn a_session_with_its_own_meta_needs_no_decoding() {
    let store = temp_dir("meta");
    // An underscore and a dot both encode to `-`, so this directory name is genuinely
    // ambiguous — and irrelevant, because the session carries its own cwd.
    let project = real_project("meta-p", "what_is_light");
    let id = id_for(7);
    write_meta(&store, &id, &project, Some("Refraction"), 42);
    write_transcript(
        &store,
        &encode_project_dir_cursor(&project),
        &id,
        &transcript("why is the sky blue", "scattering"),
    );

    let rows = CursorProvider::with_root(&store).scan();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].project, project);
    assert_eq!(rows[0].project_origin, ProjectOrigin::TranscriptExact);
    assert!(rows[0].project_origin.is_exact());
    // Cursor's own title outranks the opening prompt as the row's label.
    assert_eq!(rows[0].summary, "Refraction");
    assert_eq!(rows[0].first_user, "why is the sky blue");
}

#[test]
fn a_probe_recovers_what_the_encoding_destroyed() {
    let store = temp_dir("probe");
    // Cursor sweeps `projects/` and prunes `chats/`; a transcript can outlive its metadata.
    // Then the only thing left is the lossy directory name, and the filesystem is the inverse.
    let project = real_project("probe-p", "what_is_light");
    let encoded = encode_project_dir_cursor(&project);
    assert!(
        encoded.ends_with("what-is-light"),
        "the underscore must really be gone: {encoded}"
    );
    let id = id_for(9);
    write_transcript(&store, &encoded, &id, &transcript("hello", "hi"));

    let rows = CursorProvider::with_root(&store).scan();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].project, project,
        "the probe must find the real path"
    );
    assert_eq!(rows[0].project_origin, ProjectOrigin::ProbedExact);
    assert!(rows[0].project_origin.is_exact());
    // With no chats entry there is no title, so the opening prompt is the label.
    assert_eq!(rows[0].summary, "hello");
}

#[test]
fn a_folderless_window_is_a_label_the_resume_gate_refuses() {
    let store = temp_dir("sentinel");
    // Cursor files a window with no folder open under a literal `empty-window`. Nothing
    // decodes it and nothing probes to it, which is correct: there is no project.
    let id = id_for(11);
    write_transcript(&store, "empty-window", &id, &transcript("hello", "hi"));

    let mut p = CursorProvider::with_root(&store);
    let rows = p.scan();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].project_origin, ProjectOrigin::DecodedUnverified);
    assert!(!rows[0].project_origin.is_exact());

    let plan = p.resume(&rows[0]);
    let blocked = plan.blocked().expect("an unverified path must not spawn");
    assert!(matches!(blocked, ResumeBlocked::ProjectUnverified { .. }));
    assert!(blocked.reason().contains("is a guess"));
}

#[test]
fn the_same_conversation_under_two_windows_is_one_row() {
    let store = temp_dir("dupe");
    let project = real_project("dupe-p", "shared");
    let id = id_for(13);
    let body = transcript("hello", "hi");
    // Cursor writes the transcript under every window that had the conversation open — the
    // real store has ids living under both a project directory and `empty-window`.
    write_transcript(&store, "empty-window", &id, &body);
    write_transcript(&store, &encode_project_dir_cursor(&project), &id, &body);

    let rows = CursorProvider::with_root(&store).scan();
    assert_eq!(rows.len(), 1, "one conversation is one row");
    // …and the reading that can be stood behind is the one that survives.
    assert_eq!(rows[0].project, project);
    assert_eq!(rows[0].project_origin, ProjectOrigin::ProbedExact);
}

#[test]
fn a_subagents_trace_is_not_a_conversation() {
    let store = temp_dir("subagent");
    let project = real_project("subagent-p", "app");
    let id = id_for(15);
    write_transcript(
        &store,
        &encode_project_dir_cursor(&project),
        &id,
        &transcript("hello", "hi"),
    );
    // A delegated agent's own trace, filed beside the transcript. Nobody resumes it.
    let subs = store
        .join("projects")
        .join(encode_project_dir_cursor(&project))
        .join("agent-transcripts")
        .join(&id)
        .join("subagents");
    std::fs::create_dir_all(&subs).unwrap();
    std::fs::write(
        subs.join(format!("{}.jsonl", id_for(16))),
        transcript("sub", "sub"),
    )
    .unwrap();

    let rows = CursorProvider::with_root(&store).scan();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
}

// ===== 3. resume: a plan, or an honest refusal =====

#[test]
fn resume_is_the_resume_flag_in_the_sessions_own_cwd() {
    let store = temp_dir("resume");
    let project = real_project("resume-p", "app");
    let id = id_for(21);
    write_meta(&store, &id, &project, Some("Work"), 5);
    write_transcript(
        &store,
        &encode_project_dir_cursor(&project),
        &id,
        &transcript("hello", "hi"),
    );

    let mut p = CursorProvider::with_root(&store);
    let rows = p.scan();
    assert_eq!(rows.len(), 1);

    // Resume goes through a provider carrying the human's per-tool path override, which it
    // must honour instead of a bare binary name. The two constructors do not compose, and
    // they do not need to: `root` only affects scanning, `overrides` only resuming.
    let mut ov = BTreeMap::new();
    ov.insert(TOOL_ID.to_string(), "/opt/tools/cursor-agent".to_string());
    let plan = CursorProvider::with_overrides(ov).resume(&rows[0]);
    let cmd = plan
        .command()
        .expect("a real project + a resolvable binary");
    assert_eq!(cmd.program, PathBuf::from("/opt/tools/cursor-agent"));
    assert_eq!(cmd.args, vec!["--resume", &id]);
    // `chats/` is keyed by a hash of the cwd, so the id resumes nothing anywhere else.
    assert_eq!(cmd.cwd, project);
    assert!(cmd.shell_line().ends_with(&format!("--resume {id}")));
}

#[test]
fn a_deleted_project_blocks_instead_of_spawning() {
    let store = temp_dir("gone");
    let project = real_project("gone-p", "app");
    let id = id_for(23);
    write_meta(&store, &id, &project, None, 5);
    write_transcript(
        &store,
        &encode_project_dir_cursor(&project),
        &id,
        &transcript("hello", "hi"),
    );

    let mut p = CursorProvider::with_root(&store);
    let rows = p.scan();
    assert!(rows[0].project_origin.is_exact());
    // The worktree goes away; its transcripts do not.
    std::fs::remove_dir_all(&project).unwrap();

    let plan = p.resume(&rows[0]);
    let blocked = plan.blocked().expect("a vanished project must not spawn");
    assert!(matches!(blocked, ResumeBlocked::ProjectMissing { .. }));
    assert!(blocked.reason().contains("no longer exists"));
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

    // …and a conversation from another harness is not this provider's to resume.
    sess.id = id_for(31);
    sess.source = HistorySource::Claude;
    assert!(matches!(
        p.resume(&sess).blocked(),
        Some(ResumeBlocked::Unsupported { tool_id: "claude" })
    ));
}

#[test]
fn the_binary_is_resolved_rather_than_assumed() {
    let project = real_project("notool", "app");
    let sess = ToolSession {
        id: id_for(33),
        source: HistorySource::Cursor,
        project,
        project_origin: ProjectOrigin::TranscriptExact,
        branch: None,
        started_at: None,
        summary: String::new(),
        first_user: String::new(),
        message_count: 0,
        full_text: String::new(),
    };
    // An override pointing at a path that does not exist is still an override (detect takes
    // it at face value), so to see ToolNotInstalled the tool has to be genuinely absent —
    // which we cannot guarantee on a dev box that has `cursor-agent` installed. Assert the
    // shape instead: either a ready plan whose program is a resolved path, or the one blocked
    // reason that can apply here.
    let p = CursorProvider::new();
    let plan = p.resume(&sess);
    match plan.blocked() {
        Some(ResumeBlocked::ToolNotInstalled { tool_id }) => assert_eq!(*tool_id, "cursor-agent"),
        Some(other) => panic!("unexpected block: {}", other.reason()),
        None => {
            let cmd = plan.command().unwrap();
            assert!(
                cmd.program.is_absolute(),
                "never a bare binary name on a command line"
            );
        }
    }
}

// ===== 4. the cache the 20 s refresh depends on =====

#[test]
fn a_rescan_reuses_the_cache() {
    let store = temp_dir("cache");
    let project = real_project("cache-p", "app");
    let encoded = encode_project_dir_cursor(&project);
    let mut paths = Vec::new();
    for n in 41u8..44 {
        let id = id_for(n);
        write_meta(&store, &id, &project, None, n as u64);
        paths.push(write_transcript(
            &store,
            &encoded,
            &id,
            &transcript("hello", "hi"),
        ));
    }

    let mut p = CursorProvider::with_root(&store);
    let first = p.scan();
    assert_eq!(first.len(), 3);
    assert_eq!(
        p.last_scan_parsed(),
        6,
        "a cold scan parses every meta and every transcript"
    );

    let second = p.scan();
    assert_eq!(second, first, "a warm scan must return the same rows");
    assert_eq!(
        p.last_scan_parsed(),
        0,
        "the panel re-scans every 20 s — it must stay O(new files)"
    );

    // One conversation gets another turn; only that file is re-read.
    std::fs::write(
        &paths[0],
        transcript("hello", "hi") + &transcript("again", "sure"),
    )
    .unwrap();
    let third = p.scan();
    assert_eq!(third.len(), 3);
    assert_eq!(
        p.last_scan_parsed(),
        1,
        "only the changed file is re-parsed"
    );
}

// ===== 5. the real store, absence-tolerant =====

fn real_store() -> Option<PathBuf> {
    cursor_root().filter(|p| store_exists(p))
}

#[test]
fn the_real_store_scans_without_panicking() {
    let Some(root) = real_store() else {
        return; // Cursor has never run here.
    };
    let mut p = CursorProvider::with_root(&root);
    let rows = p.scan();
    assert_grouping_contract(&rows);
    // Ids are what land on a command line; every row must already be safe to resume by id.
    for r in &rows {
        assert!(!r.id.is_empty());
        assert_eq!(r.source, HistorySource::Cursor);
    }
    // And a warm re-scan of the real store parses nothing.
    let again = p.scan();
    assert_eq!(again.len(), rows.len());
    assert_eq!(p.last_scan_parsed(), 0);
}

/// Not an assertion — a report. Run with:
/// `cargo test -p hyperpanes-core --test cursor_provider -- --ignored --nocapture`
#[test]
#[ignore = "reports numbers from the machine's real Cursor store"]
fn report_real_store_numbers() {
    let Some(root) = real_store() else {
        println!("no ~/.cursor store on this machine");
        return;
    };

    let mut p = CursorProvider::with_root(&root);
    let t0 = std::time::Instant::now();
    let rows = p.scan();
    let cold = t0.elapsed();
    let cold_parsed = p.last_scan_parsed();

    let t1 = std::time::Instant::now();
    let warm_rows = p.scan();
    let warm = t1.elapsed();

    let mut projects: Vec<&PathBuf> = rows.iter().map(|r| &r.project).collect();
    projects.dedup();
    let resumable = rows.iter().filter(|r| p.resume(r).is_ready()).count();
    let mut histogram: BTreeMap<String, usize> = BTreeMap::new();
    for r in &rows {
        *histogram
            .entry(format!("{:?}", r.project_origin))
            .or_default() += 1;
    }
    let no_text = rows.iter().filter(|r| r.message_count == 0).count();

    println!("store              {}", root.display());
    println!("projects           {}", projects.len());
    println!("sessions           {}", rows.len());
    println!("  chats-only       {no_text} (transcript pruned)");
    println!("resumable          {resumable}");
    for (origin, n) in &histogram {
        println!("  {origin:<18} {n}");
    }
    println!(
        "cold scan {cold:?} ({cold_parsed} parsed) -> warm scan {warm:?} ({} parsed, {} rows)",
        p.last_scan_parsed(),
        warm_rows.len()
    );
}
