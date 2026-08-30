//! The pane-meta pair that puts a restarted pane back into the SAME conversation.
//!
//! # Why this exists when `claude_panes` already did it
//!
//! Claude Code has a `SessionStart` hook, so Hyperpanes learns a Claude pane's live
//! conversation id from the outside and records it under `claude.session`. No other tool
//! offers a hook, and the ids still have to survive a relaunch — so the mark here is
//! written from what Hyperpanes itself *did*: a pane spawned out of the left panel's
//! session list was handed one exact conversation, and that is the one to come back to.
//!
//! The tool is deliberately **not** stored a second time. A pane's kind already rides in
//! `meta["pane.kind"]` as [`crate::tools::PaneKind::Tool`], so the mark answers only
//! "which conversation, in which directory" and the kind answers "in which tool". Two
//! copies of the same fact would eventually disagree.
//!
//! # Why the directory is part of the mark
//!
//! Every provider-backed tool keys resume off the working directory — Claude by project
//! encoding, Cursor by a hash of the path, Copilot by a recorded cwd column — so the same
//! id resumes nothing anywhere else. The pane's *live* cwd is not a substitute: a shell
//! parked inside a TUI stops emitting OSC 7, so the tracked value can be stale by the time
//! a snapshot is taken. The mark carries the directory the conversation was actually
//! resumed in.

use std::collections::BTreeMap;

use crate::claude_panes::{valid_resume_cwd, valid_session_id};

/// Pane-meta key for the conversation id the pane is in.
pub const META_SESSION_KEY: &str = "tool.session";

/// Pane-meta key for the directory that conversation belongs to.
pub const META_SESSION_CWD_KEY: &str = "tool.cwd";

/// One pane's remembered conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSessionMark {
    /// The tool's own resume key.
    pub id: String,
    /// The directory the conversation belongs to.
    pub cwd: String,
}

impl ToolSessionMark {
    /// Build a mark, or `None` when either half would not survive landing on a command
    /// line. Both gates are the ones `claude_panes` already applies to hook-written
    /// markers: this value comes back out of `workspace.json`, which a human edits.
    pub fn new(id: &str, cwd: &str) -> Option<Self> {
        (valid_session_id(id) && valid_resume_cwd(cwd)).then(|| ToolSessionMark {
            id: id.to_string(),
            cwd: cwd.to_string(),
        })
    }

    /// Read a pane's mark back out of its `meta`. Re-validated rather than trusted: the
    /// file it came from is user-editable and the id lands on a command line.
    pub fn read(meta: Option<&BTreeMap<String, String>>) -> Option<Self> {
        let m = meta?;
        Self::new(m.get(META_SESSION_KEY)?, m.get(META_SESSION_CWD_KEY)?)
    }

    /// Record the mark in a pane's `meta`.
    pub fn write_into(&self, meta: &mut BTreeMap<String, String>) {
        meta.insert(META_SESSION_KEY.to_string(), self.id.clone());
        meta.insert(META_SESSION_CWD_KEY.to_string(), self.cwd.clone());
    }
}

/// The argv that puts `tool_id` back into conversation `id`, or `None` for a tool whose
/// resume shape has not been checked against a real install.
///
/// One authority rather than a copy per provider: the three history providers all build
/// their `ResumeCommand` from this, and so does the relaunch path in the app, which has no
/// provider to ask (a cold start has scanned nothing yet). A tool missing here degrades to
/// "starts fresh", which is what it does today — never to a guessed flag.
pub fn resume_args(tool_id: &str, id: &str) -> Option<Vec<String>> {
    match tool_id {
        // `claude --resume <id>`.
        "claude"
        // `copilot -r, --resume[=value]` — an *optional*-value option, so whether a
        // space-separated id binds to it was worth checking rather than assuming. It does:
        // `copilot --resume <uuid> mcp --help` runs the `mcp` subcommand, where the same
        // line without `--resume` treats the uuid as a command.
        | "copilot"
        // `cursor-agent --resume [chatId]`, verified against `--help` on 2026.08.25-3e8eec8.
        | "cursor-agent" => Some(vec!["--resume".to_string(), id.to_string()]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mark_round_trips_through_meta() {
        let m = ToolSessionMark::new("aaaa-bbbb-cccc", "/tmp/proj").unwrap();
        let mut meta = BTreeMap::new();
        m.write_into(&mut meta);
        assert_eq!(ToolSessionMark::read(Some(&meta)), Some(m));
    }

    #[test]
    fn half_a_mark_is_no_mark() {
        // The id alone cannot be resumed: every provider-backed tool keys resume off the
        // directory, so a mark without one would resume the wrong conversation or none.
        let mut meta = BTreeMap::new();
        meta.insert(META_SESSION_KEY.to_string(), "aaaa-bbbb".to_string());
        assert_eq!(ToolSessionMark::read(Some(&meta)), None);
        assert_eq!(ToolSessionMark::read(None), None);
    }

    #[test]
    fn a_hostile_mark_is_refused_on_the_way_back_in() {
        // `workspace.json` is a file the human edits, so the gate that stopped a bad value
        // being written has to run again on the way out.
        let mut meta = BTreeMap::new();
        meta.insert(META_SESSION_KEY.to_string(), "x; rm -rf /".to_string());
        meta.insert(META_SESSION_CWD_KEY.to_string(), "/tmp".to_string());
        assert_eq!(ToolSessionMark::read(Some(&meta)), None);

        let mut meta = BTreeMap::new();
        meta.insert(META_SESSION_KEY.to_string(), "aaaa-bbbb".to_string());
        meta.insert(META_SESSION_CWD_KEY.to_string(), "/has'quote".to_string());
        assert_eq!(ToolSessionMark::read(Some(&meta)), None);
    }

    #[test]
    fn only_tools_with_a_checked_resume_shape_get_argv() {
        assert_eq!(
            resume_args("claude", "abcd-1234"),
            Some(vec!["--resume".to_string(), "abcd-1234".to_string()])
        );
        assert_eq!(resume_args("copilot", "x").unwrap()[0], "--resume");
        assert_eq!(resume_args("cursor-agent", "x").unwrap()[0], "--resume");
        // A registry entry with no verified resume flag starts fresh rather than guessing.
        assert_eq!(resume_args("aider", "abcd-1234"), None);
        assert_eq!(resume_args("vim", "abcd-1234"), None);
    }
}
