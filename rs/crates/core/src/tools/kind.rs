//! What a pane *is* — the discriminator that turns a terminal into a Claude pane.
//!
//! # Why this lives in core and not in the app
//!
//! A pane's kind is persisted, so the encode/decode pair has to sit next to the
//! workspace model that writes it. It rides in `PaneSpec.meta["pane.kind"]` rather
//! than a new `PaneSpec` field, which means **no format change at all**: `meta` is
//! already an open `BTreeMap<String, String>` carrying `claude.session`,
//! `claude.cwd`, `ai.subtitle` and `role`, so an older build loads a file with this
//! key and preserves it, and a newer build loads an older file and gets
//! [`PaneKind::Terminal`]. `ENVELOPE_VERSION` does not move.
//!
//! # Unknown kinds round-trip
//!
//! [`PaneKind::Tool`] holds the registry id as an owned `String`, not a resolved
//! `&'static ToolDef`. A workspace written by a future build that knows a tool we do
//! not still saves back byte-identically — it just renders as a plain terminal in the
//! meantime. Losing the value on save would be a silent downgrade, which the compat
//! suite exists to prevent.

use super::registry::{self, ToolDef};

/// The `PaneSpec.meta` key this is stored under.
pub const META_KIND_KEY: &str = "pane.kind";

/// Views are namespaced so a tool id can never collide with one. Registry ids are
/// lowercase-kebab (enforced by a test in `registry`), so they cannot contain `:`.
const VIEW_PREFIX: &str = "view:";

/// What a pane is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PaneKind {
    /// A plain shell. The default, and what every pane written before this existed is.
    #[default]
    Terminal,
    /// PTY-backed, but running a known CLI AI tool — the string is a registry id.
    /// An id no build in this binary knows is still valid and still round-trips.
    Tool(String),
    /// Non-PTY views.
    FileBrowser,
    FileViewer,
    Markdown,
    /// Web content. Gated behind the internal-browser decision; see the plan's Q2.
    Browser,
}

impl PaneKind {
    /// Whether this pane is backed by a pseudo-terminal.
    ///
    /// Load-bearing: `SessionManager` keys sessions by `PaneState.uid`, so minting a
    /// session uid for a non-PTY pane would put a phantom entry in front of
    /// `pane_load`, `has()`, and the multi-window `claim_session` arbitration. Every
    /// `mgr.*` call is gated on this.
    #[tracing::instrument(level = "debug", ret)]
    pub fn is_pty(&self) -> bool {
        matches!(self, PaneKind::Terminal | PaneKind::Tool(_))
    }

    /// The registry entry, when this is a tool we know about.
    #[tracing::instrument(level = "debug", ret)]
    pub fn tool(&self) -> Option<&'static ToolDef> {
        match self {
            PaneKind::Tool(id) => registry::by_id(id),
            _ => None,
        }
    }

    /// The tool id, whether or not this build knows the tool.
    #[tracing::instrument(level = "debug", ret)]
    pub fn tool_id(&self) -> Option<&str> {
        match self {
            PaneKind::Tool(id) => Some(id.as_str()),
            _ => None,
        }
    }

    /// The value written to `meta["pane.kind"]`. `Terminal` returns `None` so the
    /// default is never written — a file from a build with this feature and one from
    /// a build without it stay byte-identical for ordinary panes.
    #[tracing::instrument(level = "debug", ret)]
    pub fn as_meta_value(&self) -> Option<String> {
        match self {
            PaneKind::Terminal => None,
            PaneKind::Tool(id) => Some(id.clone()),
            PaneKind::FileBrowser => Some(format!("{VIEW_PREFIX}files")),
            PaneKind::FileViewer => Some(format!("{VIEW_PREFIX}file")),
            PaneKind::Markdown => Some(format!("{VIEW_PREFIX}markdown")),
            PaneKind::Browser => Some(format!("{VIEW_PREFIX}browser")),
        }
    }

    /// Decode a `meta["pane.kind"]` value. Anything unrecognised in the `view:`
    /// namespace falls back to `Terminal` — a view we cannot render is better shown
    /// as a shell than as a broken pane.
    #[tracing::instrument(level = "debug", ret)]
    pub fn from_meta_value(v: &str) -> PaneKind {
        let v = v.trim();
        if v.is_empty() || v == "terminal" {
            return PaneKind::Terminal;
        }
        if let Some(view) = v.strip_prefix(VIEW_PREFIX) {
            return match view {
                "files" => PaneKind::FileBrowser,
                "file" => PaneKind::FileViewer,
                "markdown" => PaneKind::Markdown,
                "browser" => PaneKind::Browser,
                _ => PaneKind::Terminal,
            };
        }
        PaneKind::Tool(v.to_string())
    }

    /// The int the Slint side switches on. Tool panes are `1`; *which* tool comes
    /// through the separate `tool-icon` property, so the UI never needs a per-tool
    /// enum arm.
    #[tracing::instrument(level = "debug", ret)]
    pub fn ui_kind(&self) -> i32 {
        match self {
            PaneKind::Terminal => 0,
            PaneKind::Tool(_) => 1,
            PaneKind::FileBrowser => 2,
            PaneKind::FileViewer => 3,
            PaneKind::Markdown => 4,
            PaneKind::Browser => 5,
        }
    }

    /// Icon kind for the pane header, or `0` when there is nothing tool-specific to
    /// show. An unknown tool id has no icon, which is the honest rendering.
    #[tracing::instrument(level = "debug", ret)]
    pub fn ui_icon(&self) -> i32 {
        self.tool().map(|t| t.icon as i32).unwrap_or(0)
    }

    /// Display name for the pane header badge; empty when the pane is a plain shell.
    #[tracing::instrument(level = "debug", ret)]
    pub fn ui_name(&self) -> String {
        match self {
            PaneKind::Terminal => String::new(),
            PaneKind::Tool(id) => registry::by_id(id)
                .map(|t| t.name.to_string())
                .unwrap_or_else(|| id.clone()),
            PaneKind::FileBrowser => "Files".to_string(),
            PaneKind::FileViewer => "Viewer".to_string(),
            PaneKind::Markdown => "Markdown".to_string(),
            PaneKind::Browser => "Browser".to_string(),
        }
    }

    /// The kind a pane spawned with `command` starts life as.
    ///
    /// Only the *program*'s file stem is matched — not its arguments, and not its
    /// directories. `claude`, `/opt/homebrew/bin/claude`, and `claude.cmd` are all a Claude
    /// pane; `bash --init-file ~/dev/gemini/rc` and `~/src/gemini-app/run.sh` are not
    /// Gemini panes, which they would be under a whole-string token match.
    ///
    /// This is the *deterministic* half of tool detection: what the user explicitly asked
    /// to run. The runtime half — noticing that a plain shell pane has started `claude` —
    /// reads the OSC title instead, and only ever upgrades a [`PaneKind::Terminal`].
    #[tracing::instrument(level = "debug", ret)]
    pub fn for_command(command: &str) -> PaneKind {
        let Some(program) = command.split_whitespace().next() else {
            return PaneKind::Terminal;
        };
        let stem = std::path::Path::new(program)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        // The executable name is checked first and on its own terms: `agent` is the
        // Cursor CLI even though "agent" is a title token that names no tool. Only if
        // no binary matches do we fall back to reading the stem as a title token, which
        // is what catches wrappers like `claude-code`.
        match registry::by_bin(&stem).or_else(|| registry::by_title(&stem)) {
            Some(t) => PaneKind::Tool(t.id.to_string()),
            None => PaneKind::Terminal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_writes_nothing() {
        assert_eq!(PaneKind::Terminal.as_meta_value(), None);
        assert_eq!(PaneKind::from_meta_value(""), PaneKind::Terminal);
        assert_eq!(PaneKind::from_meta_value("terminal"), PaneKind::Terminal);
    }

    #[test]
    fn every_kind_round_trips() {
        let kinds = [
            PaneKind::Tool("claude".into()),
            PaneKind::FileBrowser,
            PaneKind::FileViewer,
            PaneKind::Markdown,
            PaneKind::Browser,
        ];
        for k in kinds {
            let v = k.as_meta_value().expect("non-default kinds are written");
            assert_eq!(
                PaneKind::from_meta_value(&v),
                k,
                "round-trip failed for {k:?}"
            );
        }
    }

    #[test]
    fn an_unknown_tool_id_survives_a_round_trip() {
        // The whole point: a workspace from a newer build must save back unchanged.
        let k = PaneKind::from_meta_value("some-future-tool");
        assert_eq!(k, PaneKind::Tool("some-future-tool".into()));
        assert_eq!(k.as_meta_value().as_deref(), Some("some-future-tool"));
        assert!(
            k.tool().is_none(),
            "unknown id must not resolve in the registry"
        );
        assert_eq!(
            k.ui_icon(),
            0,
            "unknown tool shows no icon rather than a wrong one"
        );
        assert_eq!(k.ui_name(), "some-future-tool");
    }

    #[test]
    fn an_unknown_view_degrades_to_a_terminal_not_a_tool() {
        assert_eq!(
            PaneKind::from_meta_value("view:hologram"),
            PaneKind::Terminal
        );
    }

    #[test]
    fn only_terminals_and_tools_are_pty_backed() {
        assert!(PaneKind::Terminal.is_pty());
        assert!(PaneKind::Tool("claude".into()).is_pty());
        for k in [
            PaneKind::FileBrowser,
            PaneKind::FileViewer,
            PaneKind::Markdown,
            PaneKind::Browser,
        ] {
            assert!(!k.is_pty(), "{k:?} must not mint a session uid");
        }
    }

    #[test]
    fn ui_kinds_are_distinct() {
        let all = [
            PaneKind::Terminal,
            PaneKind::Tool("claude".into()),
            PaneKind::FileBrowser,
            PaneKind::FileViewer,
            PaneKind::Markdown,
            PaneKind::Browser,
        ];
        let mut seen: Vec<i32> = all.iter().map(|k| k.ui_kind()).collect();
        let n = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), n);
    }

    #[test]
    fn a_known_tool_resolves_its_icon_and_name() {
        let k = PaneKind::Tool("claude".into());
        assert_eq!(k.ui_name(), "Claude Code");
        assert_eq!(k.ui_icon(), registry::by_id("claude").unwrap().icon as i32);
    }

    #[test]
    fn a_spawn_command_names_the_kind_from_its_program_alone() {
        assert_eq!(
            PaneKind::for_command("claude"),
            PaneKind::Tool("claude".into())
        );
        assert_eq!(
            PaneKind::for_command("/opt/homebrew/bin/claude --resume abc"),
            PaneKind::Tool("claude".into())
        );
        assert_eq!(
            PaneKind::for_command("claude.cmd"),
            PaneKind::Tool("claude".into())
        );
    }

    #[test]
    fn a_tool_name_in_an_argument_or_a_directory_is_not_a_tool_pane() {
        // These are the false positives a whole-string token match would produce.
        assert_eq!(
            PaneKind::for_command("bash --init-file /home/me/dev/gemini/rc"),
            PaneKind::Terminal
        );
        assert_eq!(
            PaneKind::for_command("/home/me/src/gemini-app/run.sh"),
            PaneKind::Terminal
        );
        assert_eq!(PaneKind::for_command(""), PaneKind::Terminal);
        assert_eq!(PaneKind::for_command("   "), PaneKind::Terminal);
    }
}
