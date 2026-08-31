//! The pane-meta pair that puts a restarted pane back into the SAME conversation.
//!
//! # Why this exists when `claude_panes` already did it
//!
//! Claude Code has a `SessionStart` hook, so Hyperpanes learns a Claude pane's live
//! conversation id from the outside and records it under `claude.session`. The ids still
//! have to survive a relaunch for every *other* tool — so the mark here is the general
//! form, written from any of three sources: a pane spawned out of the left panel's session
//! list (Hyperpanes handed it one exact conversation), a per-tool session hook
//! ([`crate::tools::session_hook`] — cursor-agent and Copilot CLI have one too), or the
//! scan-and-diff inference of last resort ([`crate::tools::session_infer`]).
//!
//! # Why the tool id is optional rather than absent
//!
//! For a pane Hyperpanes *spawned* as a tool the kind already answers "which tool": it
//! rides in `meta["pane.kind"]` as [`crate::tools::PaneKind::Tool`], and storing the same
//! fact twice invites the two copies to disagree. That reasoning holds exactly as long as
//! the pane has a tool kind.
//!
//! A **hand-started** tool pane does not. The human opened a plain shell and typed
//! `cursor-agent`; the runtime sniff notices and re-brands the pane, but it deliberately
//! never writes the persisted kind (what a pane *relaunches* is not inferred), so the
//! snapshot records `Terminal`. For that pane the kind answers nothing, and a mark with no
//! tool is a conversation id nobody can spend. So the tool is recorded only when it was
//! learned from the outside — hook or inference — and read only where the kind is silent.
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

/// Pane-meta key for the tool the conversation belongs to — written only when the pane's
/// own [`crate::tools::PaneKind`] cannot answer that (see the module doc). Absent on every
/// mark written before this existed and on every spawned-as-a-tool pane, which is why it
/// reads back as `Option`.
pub const META_SESSION_TOOL_KEY: &str = "tool.id";

/// One pane's remembered conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSessionMark {
    /// The tool's own resume key.
    pub id: String,
    /// The directory the conversation belongs to.
    pub cwd: String,
    /// The registry id of the tool holding the conversation, when the pane's kind does
    /// not already say. `None` is the ordinary case: ask the kind.
    pub tool: Option<String>,
}

impl ToolSessionMark {
    /// Build a mark, or `None` when either half would not survive landing on a command
    /// line. Both gates are the ones `claude_panes` already applies to hook-written
    /// markers: this value comes back out of `workspace.json`, which a human edits.
    pub fn new(id: &str, cwd: &str) -> Option<Self> {
        (valid_session_id(id) && valid_resume_cwd(cwd)).then(|| ToolSessionMark {
            id: id.to_string(),
            cwd: cwd.to_string(),
            tool: None,
        })
    }

    /// Name the tool this conversation belongs to. Only for a mark learned from OUTSIDE a
    /// spawn — a hook marker or the scan-and-diff inference — where the pane is a plain
    /// terminal as far as its persisted kind is concerned.
    ///
    /// An id no build in this binary knows is dropped rather than kept: the only consumer
    /// resolves it in [`crate::tools::registry`] to get a program name, so an unresolvable
    /// id is a value that can never be spent, and keeping it would only invite a later
    /// reader to spend it unchecked.
    pub fn with_tool(mut self, tool_id: &str) -> Self {
        self.tool = crate::tools::registry::by_id(tool_id).map(|t| t.id.to_string());
        self
    }

    /// Read a pane's mark back out of its `meta`. Re-validated rather than trusted: the
    /// file it came from is user-editable and the id lands on a command line.
    pub fn read(meta: Option<&BTreeMap<String, String>>) -> Option<Self> {
        let m = meta?;
        let mark = Self::new(m.get(META_SESSION_KEY)?, m.get(META_SESSION_CWD_KEY)?)?;
        Some(match m.get(META_SESSION_TOOL_KEY) {
            Some(t) => mark.with_tool(t),
            None => mark,
        })
    }

    /// Record the mark in a pane's `meta`.
    pub fn write_into(&self, meta: &mut BTreeMap<String, String>) {
        meta.insert(META_SESSION_KEY.to_string(), self.id.clone());
        meta.insert(META_SESSION_CWD_KEY.to_string(), self.cwd.clone());
        if let Some(t) = &self.tool {
            meta.insert(META_SESSION_TOOL_KEY.to_string(), t.clone());
        }
    }

    /// The program to run to re-enter this conversation when the pane's kind is silent:
    /// the registry's binary name for the recorded tool. `'static`, so nothing a human
    /// typed into `workspace.json` reaches a command line through here.
    pub fn tool_bin(&self) -> Option<&'static str> {
        crate::tools::registry::by_id(self.tool.as_deref()?).map(|t| t.bin)
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
    fn a_hand_started_pane_s_mark_carries_the_tool_its_kind_cannot_name() {
        // The hand-started case: the pane persists as `Terminal`, so without this the id
        // is a conversation nobody can spend.
        let m = ToolSessionMark::new("aaaa-bbbb-cccc", "/tmp/proj")
            .unwrap()
            .with_tool("cursor-agent");
        let mut meta = BTreeMap::new();
        m.write_into(&mut meta);
        assert_eq!(meta.get(META_SESSION_TOOL_KEY).map(String::as_str), Some("cursor-agent"));
        let back = ToolSessionMark::read(Some(&meta)).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.tool_bin(), Some("cursor-agent"));
    }

    #[test]
    fn a_spawned_pane_s_mark_writes_no_tool_key_at_all() {
        // A pane spawned AS a tool already records which one in `pane.kind`; a second copy
        // of that fact is one that can drift. It must also stay byte-identical to what
        // builds before the key existed wrote.
        let m = ToolSessionMark::new("aaaa-bbbb-cccc", "/tmp/proj").unwrap();
        let mut meta = BTreeMap::new();
        m.write_into(&mut meta);
        assert!(!meta.contains_key(META_SESSION_TOOL_KEY));
        assert_eq!(ToolSessionMark::read(Some(&meta)).unwrap().tool, None);
    }

    #[test]
    fn an_unknown_or_hostile_tool_id_is_dropped_rather_than_carried() {
        // Unlike `pane.kind`, this key is not a round-trip contract — it exists only to be
        // resolved to a program name. An id that resolves to nothing is dead weight, and a
        // hostile one must never reach the command line the mark is spent on.
        let mut meta = BTreeMap::new();
        meta.insert(META_SESSION_KEY.to_string(), "aaaa-bbbb".to_string());
        meta.insert(META_SESSION_CWD_KEY.to_string(), "/tmp/proj".to_string());
        meta.insert(
            META_SESSION_TOOL_KEY.to_string(),
            "claude; rm -rf /".to_string(),
        );
        let back = ToolSessionMark::read(Some(&meta)).unwrap();
        assert_eq!(back.tool, None);
        assert_eq!(back.tool_bin(), None);
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
