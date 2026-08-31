//! CLI AI tools: the catalogue, binary detection, and (later) session-history providers.
//!
//! The shape here follows one rule — **a tool is data**. [`registry::TOOLS`] is a
//! static table; adding Claude's next competitor is a row, not a code path. Detection,
//! icons, brand accents, pane chrome, and the left panel's per-tool views all read
//! that table, so they cannot drift apart.
//!
//! See `docs/tool-panes-plan.md` for the decisions behind this module.

pub mod detect;
/// The kernel's own answer to "what is running in this pane right now" — the pty's
/// foreground process group, resolved to an executable name and fed to the same
/// [`registry::by_bin`] the launch command uses.
pub mod foreground;
/// Session history: one [`history::SessionProvider`] per tool that keeps resumable
/// conversations on disk, all feeding the same row shape.
pub mod history;
pub mod kind;
pub mod registry;
/// The pane-meta pair that puts a restarted pane back into the same conversation, and the
/// one authority on each tool's resume argv.
pub mod session_mark;

pub use foreground::{
    foreground_cwd, foreground_name, foreground_tool, tool_for_foreground_name, PtyFd,
};
pub use kind::{PaneKind, META_KIND_KEY};
pub use registry::{by_bin, by_id, by_title, HistoryKind, ToolDef, TOOLS, TOOL_ICON_BASE};
pub use session_mark::{resume_args, ToolSessionMark, META_SESSION_CWD_KEY, META_SESSION_KEY};
