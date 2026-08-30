//! **Every locally resumable conversation, and the path back into one.**
//!
//! The session view (F6 in `docs/tool-panes-plan.md`) asks for *all* of a tool's
//! conversations, not the ones under a project root it was handed. For Claude Code that
//! means naming directories nobody gave us a root for, and the directory names are an
//! encoding that **cannot be inverted** — `/`, `.`, `_` and spaces all become `-`, runs are
//! not collapsed. `-Users-bshuler--pane` reads equally well as `/Users/bshuler/.pane` (the
//! truth on the machine this was written against) and `/Users/bshuler//pane`.
//!
//! So the tests here are about *provenance*, not just counts:
//!
//!   * A `cwd` recovered from inside the transcript, re-encoded and checked against the
//!     directory holding it, is proof — encoding is a function.
//!   * A filesystem probe that finds a real directory re-encoding to the same name is proof
//!     of the same kind, and recovers `.claude` and `what_is_light` where no substitution
//!     rule could.
//!   * Anything else is a label, and [`ResumePlan`] must refuse to spawn a pane on it.
//!
//! Fixtures rather than the machine's real `~/.claude/projects` throughout, except for two
//! deliberately absence-tolerant checks at the bottom (one of them `#[ignore]`d, because it
//! reports numbers rather than asserting them).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hyperpanes_core::claude_history::{
    all_projects_in, decode_by_probing, decode_project_dir, encode_path_str, encode_project_dir,
    read_session_file, HistorySource, ProjectOrigin, SessionCache,
};
use hyperpanes_core::tools::history::{
    ClaudeProvider, ResumeBlocked, SessionProvider, ToolSession,
};

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hp-sessprov-{}-{tag}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A transcript shaped like the real thing: line 1 is a `summary` record with **no** `cwd`
/// (that is what Claude Code actually writes), and the `cwd`/`gitBranch` pair rides on a
/// later record.
fn transcript(cwd: &str, branch: &str, prompt: &str) -> String {
    format!(
        "{{\"type\":\"summary\",\"summary\":\"an earlier conversation\",\"leafUuid\":\"x\"}}\n\
         {{\"type\":\"mode\",\"mode\":\"normal\"}}\n\
         {{\"type\":\"user\",\"cwd\":\"{cwd}\",\"gitBranch\":\"{branch}\",\
           \"message\":{{\"role\":\"user\",\"content\":\"{prompt}\"}}}}\n\
         {{\"type\":\"assistant\",\"cwd\":\"{cwd}\",\"gitBranch\":\"{branch}\",\
           \"message\":{{\"role\":\"assistant\",\"content\":\"on it\"}}}}\n"
    )
}

/// Write `body` to `<root>/<encode(project)>/<id>.jsonl`, creating the encoded directory.
fn write_at(root: &Path, project: &str, id: &str, body: &str) -> PathBuf {
    let dir = root.join(encode_path_str(project));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, body).unwrap();
    path
}

// ===== 1. cwd / gitBranch come out of the existing bounded prefix =====

#[test]
fn cwd_and_branch_survive_a_leading_summary_record() {
    let dir = temp_dir("cwd");
    let path = dir.join("s1.jsonl");
    std::fs::write(&path, transcript("/w/repo", "main", "hello")).unwrap();

    let s = read_session_file(&path).unwrap();
    assert_eq!(s.cwd.as_deref(), Some(Path::new("/w/repo")));
    assert_eq!(s.git_branch.as_deref(), Some("main"));
    // The summary record still wins the label — this changed nothing about that.
    assert_eq!(s.summary, "an earlier conversation");
    assert_eq!(s.first_user, "hello");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_transcript_with_no_cwd_reports_none_rather_than_guessing() {
    let dir = temp_dir("nocwd");
    let path = dir.join("s2.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"no cwd anywhere\"}}\n",
    )
    .unwrap();
    let s = read_session_file(&path).unwrap();
    assert!(s.cwd.is_none() && s.git_branch.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_branch_is_not_a_branch() {
    // A project that is not a git repo records `gitBranch: ""`. Empty is absence.
    let dir = temp_dir("nobranch");
    let path = dir.join("s3.jsonl");
    std::fs::write(&path, transcript("/w/plain", "", "hi")).unwrap();
    let s = read_session_file(&path).unwrap();
    assert_eq!(s.cwd.as_deref(), Some(Path::new("/w/plain")));
    assert!(s.git_branch.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_huge_opening_message_does_not_starve_the_cwd_scan() {
    // The prefix scan stops JSON-parsing once summary AND full-text are both done. A single
    // enormous first message fills the full-text budget on line 1, so `cwd` — which lands on
    // a later record — is only found if it has a bound of its own.
    let dir = temp_dir("huge");
    let path = dir.join("s4.jsonl");
    let huge = "word ".repeat(20_000);
    let body = format!(
        "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{huge}\"}}}}\n\
         {{\"type\":\"assistant\",\"cwd\":\"/w/late\",\"gitBranch\":\"main\",\
           \"message\":{{\"role\":\"assistant\",\"content\":\"ok\"}}}}\n"
    );
    std::fs::write(&path, body).unwrap();
    let s = read_session_file(&path).unwrap();
    assert_eq!(s.cwd.as_deref(), Some(Path::new("/w/late")));
    let _ = std::fs::remove_dir_all(&dir);
}

// ===== 2. global enumeration and the provenance of each project path =====

#[test]
fn enumerates_every_project_directory_under_a_root() {
    let root = temp_dir("enum");
    write_at(
        &root,
        "/w/alpha",
        "a1",
        &transcript("/w/alpha", "main", "one"),
    );
    write_at(
        &root,
        "/w/alpha",
        "a2",
        &transcript("/w/alpha", "main", "two"),
    );
    write_at(
        &root,
        "/w/beta",
        "b1",
        &transcript("/w/beta", "dev", "three"),
    );
    // A directory holding no transcripts at all (Claude leaves `memory/`-only dirs behind).
    std::fs::create_dir_all(root.join("-w-empty").join("memory")).unwrap();

    let projects = all_projects_in(&root);
    assert_eq!(
        projects.len(),
        2,
        "the transcript-less directory is skipped"
    );
    assert_eq!(projects[0].project, PathBuf::from("/w/alpha"));
    assert_eq!(projects[0].sessions.len(), 2);
    assert_eq!(projects[1].project, PathBuf::from("/w/beta"));
    assert!(projects
        .iter()
        .all(|p| p.origin == ProjectOrigin::TranscriptExact));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_transcript_cwd_that_does_not_match_its_directory_is_not_called_exact() {
    // The real shape this defends against: a session launched in the main repo that later
    // `cd`s into a worktree. Its opening record carries the *launch* directory, while the
    // transcript is filed under the worktree — and the worktree has since been deleted, so
    // the filesystem probe cannot rescue it either.
    let root = temp_dir("mismatch");
    let dir = root.join("-w-repo--claude-worktrees-gone");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("m1.jsonl"), transcript("/w/repo", "main", "hi")).unwrap();

    let projects = all_projects_in(&root);
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].origin, ProjectOrigin::TranscriptCwd);
    assert_eq!(projects[0].project, PathBuf::from("/w/repo"));
    assert!(!projects[0].origin.is_exact());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn with_no_cwd_at_all_the_decode_is_labelled_unverified() {
    let root = temp_dir("decoded");
    let dir = root.join("-nowhere-at-all-really");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("d1.jsonl"),
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"orphan\"}}\n",
    )
    .unwrap();

    let projects = all_projects_in(&root);
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].origin, ProjectOrigin::DecodedUnverified);
    assert!(!projects[0].origin.is_exact());
    // The lossy decode is exactly that: every `-` became a separator.
    assert_eq!(
        projects[0].project,
        decode_project_dir("-nowhere-at-all-really")
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_filesystem_probe_recovers_dots_and_underscores_no_rule_could() {
    // Both of these are real shapes from `~/.claude/projects` on the machine this was
    // written against: a dot-directory (`.pane`) and an underscore in a repo name
    // (`what_is_light`), each of which encodes to a `-` indistinguishable from a separator.
    let base = temp_dir("probe");
    let deep = base.join("code").join("what_is_light").join(".claude");
    std::fs::create_dir_all(&deep).unwrap();

    let encoded = encode_project_dir(&deep);
    let probed = decode_by_probing(&encoded).expect("the directories exist, so the walk lands");
    assert_eq!(probed, deep);
    // The naive decode gets it wrong, which is the whole reason the probe exists.
    assert_ne!(decode_project_dir(&encoded), deep);

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_probe_that_finds_nothing_returns_none_rather_than_a_guess() {
    let base = temp_dir("probe-miss");
    let missing = base.join("never_existed").join("nor_this");
    let encoded = encode_project_dir(&missing);
    assert!(decode_by_probing(&encoded).is_none());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_probe_beats_a_transcript_cwd_that_disagrees_with_its_directory() {
    // Same worktree shape as above, except the worktree still exists — so the probe finds
    // the directory the transcripts are actually filed under, and that beats the launch
    // directory the opening record happened to carry.
    let base = temp_dir("probe-wins");
    let worktree = base.join("repo").join(".worktrees").join("daily_games");
    std::fs::create_dir_all(&worktree).unwrap();
    let root = temp_dir("probe-wins-root");
    let dir = root.join(encode_project_dir(&worktree));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("w1.jsonl"),
        transcript(&base.join("repo").to_string_lossy(), "main", "hi"),
    )
    .unwrap();

    let projects = all_projects_in(&root);
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].origin, ProjectOrigin::ProbedExact);
    assert_eq!(projects[0].project, worktree);

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn the_global_scan_stays_o_of_new_files() {
    let root = temp_dir("cache");
    for i in 0..3 {
        write_at(
            &root,
            &format!("/w/p{i}"),
            &format!("s{i}"),
            &transcript(&format!("/w/p{i}"), "main", "hi"),
        );
    }
    let mut cache = SessionCache::new();
    let first = cache.scan_all_in(&root);
    assert_eq!(first.len(), 3);
    assert_eq!(cache.last_scan_parsed(), 3, "cold scan parses everything");

    let second = cache.scan_all_in(&root);
    assert_eq!(second, first);
    assert_eq!(cache.last_scan_parsed(), 0, "warm scan parses nothing");

    // One new project appears; only its transcript is parsed.
    write_at(&root, "/w/p9", "s9", &transcript("/w/p9", "main", "hi"));
    let third = cache.scan_all_in(&root);
    assert_eq!(third.len(), 4);
    assert_eq!(cache.last_scan_parsed(), 1);

    let _ = std::fs::remove_dir_all(&root);
}

// ===== 3. the provider: rows, order, and the plan back in =====

/// Assert the panel's grouping contract holds: rows are project-ascending and a project's
/// rows are contiguous (so a heading-on-change renderer emits each heading once), newest
/// first inside each project.
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

#[test]
fn the_provider_returns_every_project_in_panel_order() {
    let root = temp_dir("provider");
    // Deliberately written out of order, across three projects.
    let files = [
        ("/w/gamma", "1111-1111", 300u64),
        ("/w/alpha", "2222-2222", 100),
        ("/w/gamma", "3333-3333", 500),
        ("/w/beta", "4444-4444", 200),
        ("/w/alpha", "5555-5555", 400),
    ];
    for (project, id, mtime) in files {
        let path = write_at(&root, project, id, &transcript(project, "main", "hi"));
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime))
            .unwrap();
    }

    let mut p = ClaudeProvider::with_root(&root);
    let rows = p.scan();
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|r| r.source == HistorySource::Claude));
    assert_grouping_contract(&rows);

    let order: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| (r.project.to_str().unwrap(), r.id.as_str()))
        .collect();
    assert_eq!(
        order,
        vec![
            ("/w/alpha", "5555-5555"),
            ("/w/alpha", "2222-2222"),
            ("/w/beta", "4444-4444"),
            ("/w/gamma", "3333-3333"),
            ("/w/gamma", "1111-1111"),
        ]
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn resume_spawns_the_resolved_binary_in_the_sessions_own_project() {
    let project = temp_dir("resume-proj");
    let root = temp_dir("resume-root");
    write_at(
        &root,
        &project.to_string_lossy(),
        "aaaa-bbbb-cccc-dddd",
        &transcript(&project.to_string_lossy(), "feature/x", "hi"),
    );

    let mut overrides = BTreeMap::new();
    overrides.insert("claude".to_string(), "/opt/claude/bin/claude".to_string());
    let mut p = ClaudeProvider::with_root(&root);
    let rows = p.scan();
    assert_eq!(rows[0].branch.as_deref(), Some("feature/x"));

    // The override is what a plan spawns — resolution goes through `tools::detect`, so the
    // human's per-tool path wins over anything on PATH.
    let p = ClaudeProvider::with_overrides(overrides);
    let plan = p.resume(&rows[0]);
    let cmd = plan.command().expect("real project, resolvable binary");
    assert_eq!(cmd.program, PathBuf::from("/opt/claude/bin/claude"));
    assert_eq!(cmd.args, vec!["--resume", "aaaa-bbbb-cccc-dddd"]);
    assert_eq!(cmd.cwd, project);

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn an_unverified_project_is_blocked_not_spawned() {
    let root = temp_dir("unverified");
    let dir = root.join("-nowhere-at-all");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("aaaa-bbbb.jsonl"),
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"orphan\"}}\n",
    )
    .unwrap();

    let mut p = ClaudeProvider::with_root(&root);
    let rows = p.scan();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].project_origin, ProjectOrigin::DecodedUnverified);
    let plan = p.resume(&rows[0]);
    assert!(matches!(
        plan.blocked(),
        Some(ResumeBlocked::ProjectUnverified { .. })
    ));
    assert!(plan.command().is_none());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_uninstalled_tool_is_blocked_with_a_reason() {
    let project = temp_dir("noinstall-proj");
    let sess = ToolSession {
        id: "aaaa-bbbb".into(),
        source: HistorySource::Claude,
        project: project.clone(),
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
    // which we cannot guarantee on a dev box that has `claude` installed. Assert the shape
    // instead: either a ready plan, or the one blocked reason that can apply here.
    let p = ClaudeProvider::new();
    let plan = p.resume(&sess);
    match plan.blocked() {
        Some(ResumeBlocked::ToolNotInstalled { tool_id }) => assert_eq!(*tool_id, "claude"),
        Some(other) => panic!("unexpected block: {}", other.reason()),
        None => assert!(plan.is_ready()),
    }
    let _ = std::fs::remove_dir_all(&project);
}

// ===== 4. the real store, absence-tolerant =====

/// The machine's own `~/.claude/projects`, or `None` when Claude Code has never run here.
fn real_store() -> Option<PathBuf> {
    hyperpanes_core::claude_history::claude_projects_root().filter(|p| p.is_dir())
}

#[test]
fn the_real_store_enumerates_without_panicking() {
    let Some(root) = real_store() else {
        eprintln!("no ~/.claude/projects on this machine — skipped");
        return;
    };
    let mut p = ClaudeProvider::with_root(&root);
    let rows = p.scan();
    assert_grouping_contract(&rows);
    // Every project called exact must re-encode to a directory that really is in the store —
    // that is what "exact" means, and it is the only claim `resume` is allowed to act on.
    for pr in p.scan_projects() {
        assert!(!pr.sessions.is_empty());
        if pr.origin.is_exact() {
            assert_eq!(
                root.join(encode_project_dir(&pr.project)),
                pr.dir,
                "an exact project path must re-encode to the directory holding it"
            );
        }
    }
    assert!(rows.iter().all(|r| !r.id.is_empty()));
}

/// Not an assertion — a report. Run with:
/// `cargo test -p hyperpanes-core --test session_providers -- --ignored --nocapture`
#[test]
#[ignore = "reports numbers from the machine's real transcript store"]
fn report_real_store_numbers() {
    let Some(root) = real_store() else {
        eprintln!("no ~/.claude/projects on this machine — nothing to report");
        return;
    };
    let dirs = std::fs::read_dir(&root)
        .map(|e| e.flatten().filter(|e| e.path().is_dir()).count())
        .unwrap_or(0);

    let mut p = ClaudeProvider::with_root(&root);
    let started = std::time::Instant::now();
    let rows = p.scan();
    let cold = started.elapsed();
    let cold_parsed = p.last_scan_parsed();
    let started = std::time::Instant::now();
    let again = p.scan();
    let warm = started.elapsed();

    let projects = p.scan_projects();
    let mut by_origin: BTreeMap<String, usize> = BTreeMap::new();
    for pr in &projects {
        *by_origin.entry(format!("{:?}", pr.origin)).or_default() += 1;
    }
    let with_cwd = rows
        .iter()
        .filter(|r| r.project_origin == ProjectOrigin::TranscriptExact)
        .count();
    let ready = rows.iter().filter(|r| p.resume(r).is_ready()).count();

    println!("project directories on disk   : {dirs}");
    println!("projects with transcripts     : {}", projects.len());
    println!("sessions                      : {}", rows.len());
    println!("  from an exact transcript cwd: {with_cwd}");
    println!("  resumable right now         : {ready}");
    println!("project path provenance       : {by_origin:?}");
    println!(
        "cold scan {cold:?} ({cold_parsed} parsed) -> warm scan {warm:?} ({} parsed)",
        p.last_scan_parsed()
    );
    assert_eq!(again.len(), rows.len());
}
