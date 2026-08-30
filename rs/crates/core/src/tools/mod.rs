//! CLI AI tools: the catalogue, binary detection, and (later) session-history providers.
//!
//! The shape here follows one rule — **a tool is data**. [`registry::TOOLS`] is a
//! static table; adding Claude's next competitor is a row, not a code path. Detection,
//! icons, brand accents, pane chrome, and the left panel's per-tool views all read
//! that table, so they cannot drift apart.
//!
//! See `docs/tool-panes-plan.md` for the decisions behind this module.

pub mod detect;
pub mod kind;
pub mod registry;

pub use kind::{PaneKind, META_KIND_KEY};
pub use registry::{by_id, by_title, HistoryKind, ToolDef, TOOLS, TOOL_ICON_BASE};
