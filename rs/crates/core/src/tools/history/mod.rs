//! Session history, one shape for every tool — the generalization of what
//! `claude_history.rs` already does for Claude Code.
//!
//! The left panel's per-tool session view (F6 in `docs/tool-panes-plan.md`) asks the same
//! three questions of every tool: *what conversations exist on this machine*, *which project
//! was each one in*, and *how do I get back into one*. [`SessionProvider`] is those three
//! questions and nothing else, so a new tool is a file next to `claude.rs` rather than a
//! branch in the panel.
//!
//! Two things are deliberately in the trait rather than left to callers:
//!
//! **`scan` returns every project.** Not "the sessions for this root" — the view is a global
//! index, and per-tool the global scan is the only place that knows how to enumerate its own
//! store. Providers are expected to be cache-backed so a refresh is O(new files).
//!
//! **`resume` returns a [`ResumePlan`], which may be [`ResumePlan::Blocked`].** A resume can
//! be impossible for perfectly ordinary reasons — the tool was uninstalled, the worktree was
//! deleted — and the honest place to say so is the return type, before a pane is spawned onto
//! a command that cannot work. The UI can grey the row and say why.
//!
//! Privacy: transcripts are the user's own local files. Providers read them, hand the text to
//! the in-memory index that backs search, and write nothing anywhere.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::claude_history::{HistorySource, ProjectOrigin};

pub mod claude;

pub use claude::ClaudeProvider;

/// One resumable conversation, from any tool: a `ClaudeSession` plus the provenance that a
/// cross-tool, cross-project list needs — which tool it came from, and which project it was
/// in (a per-project reader never had to say).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSession {
    /// The tool's own id for the conversation — what its `--resume` flag takes.
    pub id: String,
    /// Which harness wrote it.
    pub source: HistorySource,
    /// The project directory the conversation happened in.
    pub project: PathBuf,
    /// How `project` was recovered. Anything other than
    /// [`ProjectOrigin::is_exact`] is a *label*, not a path to spawn in — [`resume`] refuses
    /// to build a plan on an unverified one.
    ///
    /// [`resume`]: SessionProvider::resume
    pub project_origin: ProjectOrigin,
    /// The git branch recorded with the session, when it recorded one.
    pub branch: Option<String>,
    /// Epoch milliseconds, newest-first ordering key.
    pub started_at: Option<u64>,
    /// One-line label for the row.
    pub summary: String,
    /// The opening prompt, kept separately so search can match it.
    pub first_user: String,
    /// Record count — a cheap proxy for length.
    pub message_count: usize,
    /// Bounded, lowercased conversation text; the slow path of search.
    pub full_text: String,
}

/// Everything needed to spawn a pane that lands back inside a conversation.
///
/// A program *path*, not a name: resolution goes through [`crate::tools::detect`], which
/// honours the human's per-tool override and finds binaries a GUI app's `PATH` misses
/// (`~/.local/bin` and Homebrew are routinely absent on macOS). Spawning `"claude"` and
/// hoping would reintroduce exactly that bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The directory to spawn in. The conversation's own project — a tool that keys resume
    /// off the working directory (most do) resumes the wrong thing anywhere else.
    pub cwd: PathBuf,
}

impl ResumeCommand {
    /// The command as a shell line, for the paths that type into a live pane instead of
    /// spawning (`state.rs` has both shapes). Quoting is minimal on purpose: only the
    /// program path can contain a space, and session ids are validated before they get here.
    pub fn shell_line(&self) -> String {
        let mut out = quote_if_needed(&self.program.to_string_lossy());
        for a in &self.args {
            out.push(' ');
            out.push_str(&quote_if_needed(a));
        }
        out
    }
}

/// Single-quote `s` for a POSIX shell when it holds anything that would word-split.
fn quote_if_needed(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:\\".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Why a conversation cannot be resumed right now. Each variant is a state the machine can
/// genuinely be in, and each carries enough to explain itself in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeBlocked {
    /// The tool's binary was not found — uninstalled, or a version manager mid-switch.
    ToolNotInstalled { tool_id: &'static str },
    /// The project directory is gone. Deleted worktrees are the common case, and the
    /// transcripts outlive them.
    ProjectMissing { project: PathBuf },
    /// The project path was never verified (see [`ProjectOrigin`]), so we have no directory
    /// we would stand behind spawning in. Better a greyed row than a pane that resumes a
    /// conversation against the wrong tree.
    ProjectUnverified { project: PathBuf },
    /// The session id would not survive landing on a command line.
    BadSessionId { id: String },
    /// No reader knows how this tool resumes. The honest answer for a registry entry whose
    /// layout has not been verified against a real install.
    Unsupported { tool_id: &'static str },
}

impl ResumeBlocked {
    /// One sentence, for the row's tooltip.
    pub fn reason(&self) -> String {
        match self {
            ResumeBlocked::ToolNotInstalled { tool_id } => {
                format!("{tool_id} was not found on this machine")
            }
            ResumeBlocked::ProjectMissing { project } => {
                format!("{} no longer exists", project.display())
            }
            ResumeBlocked::ProjectUnverified { project } => {
                format!("the project path is a guess ({})", project.display())
            }
            ResumeBlocked::BadSessionId { id } => format!("malformed session id ({id})"),
            ResumeBlocked::Unsupported { tool_id } => {
                format!("resuming {tool_id} sessions is not supported yet")
            }
        }
    }
}

/// A resume that can be spawned, or the reason it cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumePlan {
    Ready(ResumeCommand),
    Blocked(ResumeBlocked),
}

impl ResumePlan {
    /// The command, when there is one.
    pub fn command(&self) -> Option<&ResumeCommand> {
        match self {
            ResumePlan::Ready(c) => Some(c),
            ResumePlan::Blocked(_) => None,
        }
    }

    /// Why not, when there isn't.
    pub fn blocked(&self) -> Option<&ResumeBlocked> {
        match self {
            ResumePlan::Blocked(b) => Some(b),
            ResumePlan::Ready(_) => None,
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, ResumePlan::Ready(_))
    }
}

/// One tool's session history.
pub trait SessionProvider {
    /// The [`crate::tools::registry::TOOLS`] id this provider serves.
    fn id(&self) -> &'static str;

    /// Every locally resumable conversation this tool has, across every project, sorted by
    /// [`sort_for_panel`]. `&mut self` because a provider is expected to hold a cache.
    fn scan(&mut self) -> Vec<ToolSession>;

    /// How to get back into `session` — or why we cannot.
    fn resume(&self, session: &ToolSession) -> ResumePlan;
}

/// The order the left panel renders: **project ascending, then newest-first inside it.**
///
/// This is a contract, not a preference. The panel draws a project heading wherever the
/// project changes between adjacent rows and does no grouping of its own, so a provider that
/// returned rows out of this order would draw the same project heading several times. Ties
/// break on id so two sessions written in the same millisecond do not swap places between
/// scans.
pub fn sort_for_panel(sessions: &mut [ToolSession]) {
    sessions.sort_by(|a, b| {
        a.project
            .cmp(&b.project)
            .then_with(|| b.started_at.cmp(&a.started_at))
            .then_with(|| b.id.cmp(&a.id))
    });
}

/// Resolve a tool's binary for a resume, mapping "not installed" onto the blocked variant.
/// The shared half of every provider's [`SessionProvider::resume`].
pub fn resolve_program(
    tool_id: &'static str,
    overrides: &BTreeMap<String, String>,
) -> Result<PathBuf, ResumeBlocked> {
    let Some(tool) = crate::tools::registry::by_id(tool_id) else {
        return Err(ResumeBlocked::Unsupported { tool_id });
    };
    crate::tools::detect::resolve(tool, overrides)
        .map(|r| r.path)
        .ok_or(ResumeBlocked::ToolNotInstalled { tool_id })
}

/// The project checks every provider owes: a path we verified, that still exists.
pub fn check_project(session: &ToolSession) -> Result<PathBuf, ResumeBlocked> {
    if !session.project_origin.is_exact() {
        return Err(ResumeBlocked::ProjectUnverified {
            project: session.project.clone(),
        });
    }
    if !session.project.is_dir() {
        return Err(ResumeBlocked::ProjectMissing {
            project: session.project.clone(),
        });
    }
    Ok(session.project.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(project: &str, id: &str, at: Option<u64>) -> ToolSession {
        ToolSession {
            id: id.into(),
            source: HistorySource::Claude,
            project: PathBuf::from(project),
            project_origin: ProjectOrigin::TranscriptExact,
            branch: None,
            started_at: at,
            summary: String::new(),
            first_user: String::new(),
            message_count: 0,
            full_text: String::new(),
        }
    }

    #[test]
    fn sort_groups_by_project_then_newest_first() {
        let mut v = vec![
            s("/b", "old", Some(100)),
            s("/a", "mid", Some(200)),
            s("/b", "new", Some(300)),
            s("/a", "newest", Some(400)),
        ];
        sort_for_panel(&mut v);
        let seen: Vec<(&str, &str)> = v
            .iter()
            .map(|x| (x.project.to_str().unwrap(), x.id.as_str()))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("/a", "newest"),
                ("/a", "mid"),
                ("/b", "new"),
                ("/b", "old"),
            ]
        );
    }

    #[test]
    fn a_project_never_appears_twice_after_sorting() {
        // The panel's grouping IS this order: it emits a heading on every project change.
        let mut v = vec![
            s("/a", "1", Some(1)),
            s("/b", "2", Some(9)),
            s("/a", "3", Some(5)),
            s("/c", "4", Some(2)),
            s("/b", "5", Some(3)),
        ];
        sort_for_panel(&mut v);
        let mut headings: Vec<PathBuf> = Vec::new();
        for row in &v {
            if headings.last() != Some(&row.project) {
                headings.push(row.project.clone());
            }
        }
        let unique: std::collections::HashSet<&PathBuf> = headings.iter().collect();
        assert_eq!(headings.len(), unique.len(), "a project heading repeated");
        assert_eq!(headings.len(), 3);
    }

    #[test]
    fn sort_is_stable_across_equal_timestamps() {
        let mut a = vec![s("/p", "aaa", Some(5)), s("/p", "bbb", Some(5))];
        let mut b = vec![s("/p", "bbb", Some(5)), s("/p", "aaa", Some(5))];
        sort_for_panel(&mut a);
        sort_for_panel(&mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn unverified_project_blocks_the_plan() {
        let mut sess = s("/definitely/not/here", "x", None);
        sess.project_origin = ProjectOrigin::DecodedUnverified;
        assert!(matches!(
            check_project(&sess),
            Err(ResumeBlocked::ProjectUnverified { .. })
        ));
    }

    #[test]
    fn a_missing_project_blocks_the_plan() {
        let sess = s("/definitely/not/here", "x", None);
        assert!(matches!(
            check_project(&sess),
            Err(ResumeBlocked::ProjectMissing { .. })
        ));
    }

    #[test]
    fn an_existing_verified_project_passes() {
        let tmp = std::env::temp_dir();
        let sess = s(&tmp.to_string_lossy(), "x", None);
        assert_eq!(check_project(&sess).unwrap(), tmp);
    }

    #[test]
    fn an_unknown_tool_is_unsupported_not_missing() {
        let err = resolve_program("no-such-tool", &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, ResumeBlocked::Unsupported { .. }));
    }

    #[test]
    fn a_user_override_is_the_program_a_plan_spawns() {
        let mut ov = BTreeMap::new();
        ov.insert("claude".to_string(), "/opt/custom/claude".to_string());
        let p = resolve_program("claude", &ov).unwrap();
        assert_eq!(p, PathBuf::from("/opt/custom/claude"));
    }

    #[test]
    fn shell_line_quotes_only_what_needs_it() {
        let c = ResumeCommand {
            program: PathBuf::from("/usr/local/bin/claude"),
            args: vec!["--resume".into(), "abc-123".into()],
            cwd: PathBuf::from("/tmp"),
        };
        assert_eq!(c.shell_line(), "/usr/local/bin/claude --resume abc-123");

        let spaced = ResumeCommand {
            program: PathBuf::from("/Applications/My Tools/claude"),
            args: vec!["--resume".into(), "abc".into()],
            cwd: PathBuf::from("/tmp"),
        };
        assert_eq!(
            spaced.shell_line(),
            "'/Applications/My Tools/claude' --resume abc"
        );
    }

    #[test]
    fn blocked_reasons_name_the_thing_that_is_wrong() {
        let b = ResumeBlocked::ProjectMissing {
            project: PathBuf::from("/gone"),
        };
        assert!(b.reason().contains("/gone"));
        let b = ResumeBlocked::ToolNotInstalled { tool_id: "claude" };
        assert!(b.reason().contains("claude"));
    }

    #[test]
    fn plan_accessors_are_exclusive() {
        let ready = ResumePlan::Ready(ResumeCommand {
            program: PathBuf::from("/bin/claude"),
            args: vec![],
            cwd: PathBuf::from("/tmp"),
        });
        assert!(ready.is_ready() && ready.command().is_some() && ready.blocked().is_none());
        let no = ResumePlan::Blocked(ResumeBlocked::BadSessionId { id: "!".into() });
        assert!(!no.is_ready() && no.command().is_none() && no.blocked().is_some());
    }
}
