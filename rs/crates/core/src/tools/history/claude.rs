//! The Claude Code [`SessionProvider`] — a thin adapter over `claude_history.rs`.
//!
//! Thin on purpose. Everything expensive already exists there: the bounded-prefix parse, the
//! `(path, mtime, size)` [`SessionCache`] that makes a re-scan O(new files), the project-path
//! recovery that reads `cwd` out of the transcript because the directory encoding cannot be
//! inverted. This file's whole job is to widen those rows to [`ToolSession`], put them in the
//! order the panel's grouping depends on, and turn one into a spawnable command.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{
    check_project, resolve_program, sort_for_panel, ResumeBlocked, ResumeCommand, ResumePlan,
    SessionProvider, ToolSession,
};
use crate::claude_history::{HistorySource, ProjectSessions, SessionCache};

/// The registry id this provider serves. A constant so the trait's `&'static str` and the
/// `detect` lookup cannot drift apart.
pub const TOOL_ID: &str = "claude";

/// Claude Code's locally resumable conversations.
pub struct ClaudeProvider {
    cache: SessionCache,
    overrides: BTreeMap<String, String>,
    /// A single `projects/` store to scan instead of the machine's real ones — the test seam,
    /// and the only reason this is not a unit struct.
    root: Option<PathBuf>,
}

impl Default for ClaudeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeProvider {
    /// A provider with no per-tool path override — detection falls to `PATH` and the
    /// well-known install locations.
    pub fn new() -> Self {
        Self {
            cache: SessionCache::new(),
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

    /// Scan `root` (one `projects/` directory) rather than every account's store.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
            ..Self::new()
        }
    }

    /// Transcripts re-parsed by the last [`scan`](SessionProvider::scan) — the cache
    /// effectiveness the global scan lives or dies by, surfaced so it can be asserted on.
    pub fn last_scan_parsed(&self) -> usize {
        self.cache.last_scan_parsed()
    }

    /// The per-project results behind the flat row list, for callers that want the project
    /// structure (and its [`crate::claude_history::ProjectOrigin`]) rather than rows.
    pub fn scan_projects(&mut self) -> Vec<ProjectSessions> {
        match &self.root {
            Some(root) => {
                let root = root.clone();
                self.cache.scan_all_in(&root)
            }
            None => self.cache.scan_all(),
        }
    }
}

/// Flatten one project's sessions into panel rows, stamping each with the project path and
/// the provenance of that path.
fn rows_for(project: &ProjectSessions) -> impl Iterator<Item = ToolSession> + '_ {
    project.sessions.iter().map(move |s| ToolSession {
        id: s.id.clone(),
        source: s.source,
        project: project.project.clone(),
        project_origin: project.origin,
        // The transcript's own `gitBranch`, not the branch the project is on *now* — a row
        // labels the conversation, and the tree has moved on since.
        branch: s.git_branch.clone(),
        started_at: s.started_at,
        summary: s.summary.clone(),
        first_user: s.first_user.clone(),
        message_count: s.message_count,
        full_text: s.full_text.clone(),
    })
}

impl SessionProvider for ClaudeProvider {
    fn id(&self) -> &'static str {
        // `TOOL_ID` is `&'static str` already; the constant exists so `detect` and this agree.
        TOOL_ID
    }

    fn scan(&mut self) -> Vec<ToolSession> {
        let projects = self.scan_projects();
        let mut rows: Vec<ToolSession> = projects.iter().flat_map(rows_for).collect();
        sort_for_panel(&mut rows);
        rows
    }

    fn resume(&self, session: &ToolSession) -> ResumePlan {
        if session.source != HistorySource::Claude {
            return ResumePlan::Blocked(ResumeBlocked::Unsupported {
                tool_id: session.source.tool_id(),
            });
        }
        // The id lands on a command line, and transcripts are files on a disk the user (or
        // anything running as them) can write — the same re-validation `state.rs` does before
        // appending `--resume` to a restored pane's argv.
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
        // One authority for every tool's resume shape, so a provider and the relaunch path
        // (which has no provider to ask on a cold start) can never disagree.
        let Some(args) = crate::tools::resume_args(TOOL_ID, &session.id) else {
            return ResumePlan::Blocked(ResumeBlocked::Unsupported { tool_id: TOOL_ID });
        };
        ResumePlan::Ready(ResumeCommand { program, args, cwd })
    }
}

/// Whether `path` looks like a Claude `projects/` store worth scanning. Used by callers that
/// want to skip the whole provider when the tool has never run on this machine.
pub fn store_exists(path: &Path) -> bool {
    path.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_history::ProjectOrigin;

    fn temp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hp-hist-{}-{tag}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Write a transcript under `root/<encoded>/<id>.jsonl` whose opening records carry
    /// `cwd`/`gitBranch`, exactly as Claude Code writes them.
    fn write_session(root: &Path, project: &str, id: &str, branch: &str, prompt: &str) -> PathBuf {
        let dir = root.join(crate::claude_history::encode_path_str(project));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        let body = format!(
            "{{\"type\":\"summary\",\"summary\":\"a summary record with no cwd\",\"leafUuid\":\"x\"}}\n\
             {{\"type\":\"user\",\"cwd\":\"{project}\",\"gitBranch\":\"{branch}\",\
               \"message\":{{\"role\":\"user\",\"content\":\"{prompt}\"}}}}\n"
        );
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn scans_every_project_and_sorts_for_the_panel() {
        let root = temp_root("scan");
        write_session(&root, "/w/beta", "1111-2222-3333", "main", "beta one");
        write_session(&root, "/w/alpha", "4444-5555-6666", "dev", "alpha one");
        write_session(&root, "/w/alpha", "7777-8888-9999", "dev", "alpha two");

        let mut p = ClaudeProvider::with_root(&root);
        let rows = p.scan();
        assert_eq!(rows.len(), 3);
        assert_eq!(p.id(), "claude");

        // Projects ascending; both /w/alpha rows adjacent, so the panel draws one heading.
        let projects: Vec<&str> = rows.iter().map(|r| r.project.to_str().unwrap()).collect();
        assert_eq!(projects, vec!["/w/alpha", "/w/alpha", "/w/beta"]);
        // The project path came from inside the transcript, and proved itself.
        assert!(rows
            .iter()
            .all(|r| r.project_origin == ProjectOrigin::TranscriptExact));
        assert_eq!(rows[0].branch.as_deref(), Some("dev"));
        assert_eq!(rows[2].branch.as_deref(), Some("main"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rescan_reuses_the_cache() {
        let root = temp_root("cache");
        write_session(&root, "/w/one", "1111-2222", "main", "hello");
        write_session(&root, "/w/two", "3333-4444", "main", "hello");

        let mut p = ClaudeProvider::with_root(&root);
        let first = p.scan();
        assert_eq!(p.last_scan_parsed(), 2, "cold scan parses every transcript");
        let second = p.scan();
        assert_eq!(second, first);
        assert_eq!(
            p.last_scan_parsed(),
            0,
            "the global scan must stay O(new files)"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resume_is_the_resume_flag_in_the_sessions_own_cwd() {
        let root = temp_root("resume");
        // The project has to exist for a plan to be built, so use a real directory.
        let project = temp_root("proj");
        let project_s = project.to_string_lossy().into_owned();
        write_session(&root, &project_s, "aaaa-bbbb-cccc", "main", "hi");

        let mut ov = BTreeMap::new();
        ov.insert(TOOL_ID.to_string(), "/opt/tools/claude".to_string());
        let mut p = ClaudeProvider {
            root: Some(root.clone()),
            ..ClaudeProvider::with_overrides(ov)
        };
        let rows = p.scan();
        assert_eq!(rows.len(), 1);

        let plan = p.resume(&rows[0]);
        let cmd = plan
            .command()
            .expect("a real project + a resolvable binary");
        assert_eq!(cmd.program, PathBuf::from("/opt/tools/claude"));
        assert_eq!(cmd.args, vec!["--resume", "aaaa-bbbb-cccc"]);
        assert_eq!(cmd.cwd, project);
        assert!(cmd.shell_line().ends_with("--resume aaaa-bbbb-cccc"));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn a_deleted_project_blocks_instead_of_spawning() {
        let root = temp_root("gone");
        let project = temp_root("gone-proj");
        let project_s = project.to_string_lossy().into_owned();
        write_session(&root, &project_s, "aaaa-bbbb", "main", "hi");
        let mut p = ClaudeProvider::with_root(&root);
        let rows = p.scan();
        // The worktree goes away; its transcripts do not.
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
    fn a_malformed_session_id_never_reaches_a_command_line() {
        let mut sess = ToolSession {
            id: "; rm -rf /".into(),
            source: HistorySource::Claude,
            project: std::env::temp_dir(),
            project_origin: ProjectOrigin::TranscriptExact,
            branch: None,
            started_at: None,
            summary: String::new(),
            first_user: String::new(),
            message_count: 0,
            full_text: String::new(),
        };
        let p = ClaudeProvider::new();
        assert!(matches!(
            p.resume(&sess).blocked(),
            Some(ResumeBlocked::BadSessionId { .. })
        ));

        // …and a session from another harness is not this provider's to resume.
        sess.id = "aaaa-bbbb".into();
        sess.source = HistorySource::Codex;
        assert!(matches!(
            p.resume(&sess).blocked(),
            Some(ResumeBlocked::Unsupported { tool_id: "codex" })
        ));
    }

    #[test]
    fn a_missing_store_scans_to_nothing() {
        let missing = std::env::temp_dir().join(format!("hp-hist-none-{}", uuid::Uuid::new_v4()));
        assert!(!store_exists(&missing));
        let mut p = ClaudeProvider::with_root(&missing);
        assert!(p.scan().is_empty());
        assert_eq!(p.last_scan_parsed(), 0);
    }
}
