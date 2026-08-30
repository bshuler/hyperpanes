//! The catalogue of CLI AI tools Hyperpanes knows about — **data, not code**.
//!
//! Everything downstream keys off this table: which tools the settings page lists,
//! which binary name detection looks for, which OSC-title tokens upgrade a terminal
//! pane into a tool pane, which icon and accent that pane wears, and whether a
//! session-history provider exists for it. Adding a tool is a row here plus, if the
//! tool keeps resumable sessions on disk, a provider — never new UI code.
//!
//! The glow module's AI-name list is *derived* from [`detect_tokens`] rather than
//! kept in parallel, so a tool added here starts glowing without a second edit.

/// Where a tool keeps its locally resumable sessions, if anywhere.
///
/// `None` is not a placeholder for "we haven't got round to it" — it is the honest
/// answer for a tool whose on-disk layout we have not verified against a real
/// install. A tool with `None` is still detected, still branded, still favouritable;
/// it just has no left-panel session view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryKind {
    None,
    /// `~/.claude/projects/<slug>/<uuid>.jsonl`
    ClaudeJsonl,
    /// `~/.cursor/chats/<workspace-hash>/<uuid>/store.db`
    CursorSqlite,
    /// `~/.copilot/session-state/<uuid>/` + `~/.copilot/session-store.db`
    CopilotSqlite,
}

/// One tool. All fields are `'static` so the whole table is a compile-time constant.
#[derive(Debug, Clone, Copy)]
pub struct ToolDef {
    /// Stable identifier. Persisted in settings and in `PaneSpec.meta["pane.kind"]`,
    /// so it must never change once shipped.
    pub id: &'static str,
    /// Human-facing name for the settings list and pane header.
    pub name: &'static str,
    /// The binary detection looks for first.
    pub bin: &'static str,
    /// Other names the same tool ships under, tried in order after `bin`.
    pub alt_bins: &'static [&'static str],
    /// Icon kind int, matching the app-side `TOOL_ICON_BASE` allocation.
    pub icon: u32,
    /// Brand accent as sRGB, used for the pane halo and the left-panel mode strip.
    pub brand: (u8, u8, u8),
    /// Lowercase tokens that, seen in a pane's OSC title, suggest this tool is running.
    /// Inferred evidence only — see the detection precedence in `docs/tool-panes-plan.md`.
    pub detect_tokens: &'static [&'static str],
    /// Where its resumable sessions live, if we have verified the layout.
    pub history: HistoryKind,
}

impl ToolDef {
    /// Every binary name this tool may be installed under, `bin` first.
    pub fn candidate_bins(&self) -> impl Iterator<Item = &'static str> + '_ {
        std::iter::once(self.bin).chain(self.alt_bins.iter().copied())
    }

    /// Whether a session-history provider can exist for this tool.
    pub fn has_history(&self) -> bool {
        self.history != HistoryKind::None
    }
}

/// Icon kinds start here; the app's `theme.rs` allocates from the same base so the
/// two stay in lock-step the way `menu_icon`/`MenuIcon` already do.
pub const TOOL_ICON_BASE: u32 = 40;

/// The catalogue. Order is the order the settings page lists them in.
pub static TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "claude",
        name: "Claude Code",
        bin: "claude",
        alt_bins: &[],
        icon: TOOL_ICON_BASE,
        brand: (0xD9, 0x77, 0x57),
        detect_tokens: &["claude"],
        history: HistoryKind::ClaudeJsonl,
    },
    ToolDef {
        id: "cursor-agent",
        name: "Cursor Agent",
        bin: "cursor-agent",
        alt_bins: &["cursor"],
        icon: TOOL_ICON_BASE + 1,
        brand: (0x6E, 0x7B, 0x8B),
        detect_tokens: &["cursor-agent"],
        history: HistoryKind::CursorSqlite,
    },
    ToolDef {
        id: "codex",
        name: "Codex CLI",
        bin: "codex",
        alt_bins: &[],
        icon: TOOL_ICON_BASE + 2,
        brand: (0x10, 0xA3, 0x7F),
        detect_tokens: &["codex"],
        // Layout unverified against a real install — registry entry only, by design.
        history: HistoryKind::None,
    },
    ToolDef {
        id: "copilot",
        name: "GitHub Copilot CLI",
        bin: "copilot",
        alt_bins: &["github-copilot-cli"],
        icon: TOOL_ICON_BASE + 3,
        brand: (0x6E, 0x54, 0xD0),
        detect_tokens: &["copilot"],
        history: HistoryKind::CopilotSqlite,
    },
    ToolDef {
        id: "aider",
        name: "Aider",
        bin: "aider",
        alt_bins: &[],
        icon: TOOL_ICON_BASE + 4,
        brand: (0x3F, 0x9B, 0x6E),
        detect_tokens: &["aider"],
        history: HistoryKind::None,
    },
    ToolDef {
        id: "gemini",
        name: "Gemini CLI",
        bin: "gemini",
        alt_bins: &[],
        icon: TOOL_ICON_BASE + 5,
        brand: (0x42, 0x85, 0xF4),
        detect_tokens: &["gemini"],
        history: HistoryKind::None,
    },
    ToolDef {
        id: "goose",
        name: "Goose",
        bin: "goose",
        alt_bins: &[],
        icon: TOOL_ICON_BASE + 6,
        brand: (0xC8, 0x9B, 0x3C),
        detect_tokens: &["goose"],
        history: HistoryKind::None,
    },
    ToolDef {
        id: "ollama",
        name: "Ollama",
        bin: "ollama",
        alt_bins: &[],
        icon: TOOL_ICON_BASE + 7,
        brand: (0x8A, 0x8A, 0x8A),
        detect_tokens: &["ollama"],
        history: HistoryKind::None,
    },
    ToolDef {
        id: "cody",
        name: "Cody",
        bin: "cody",
        alt_bins: &[],
        icon: TOOL_ICON_BASE + 8,
        brand: (0xA3, 0x05, 0xC7),
        detect_tokens: &["cody"],
        history: HistoryKind::None,
    },
    ToolDef {
        id: "continue",
        name: "Continue",
        bin: "cn",
        alt_bins: &["continue"],
        icon: TOOL_ICON_BASE + 9,
        brand: (0x2E, 0x7D, 0xB8),
        detect_tokens: &["continue"],
        history: HistoryKind::None,
    },
    ToolDef {
        id: "opencode",
        name: "OpenCode",
        bin: "opencode",
        alt_bins: &[],
        icon: TOOL_ICON_BASE + 10,
        brand: (0xE0, 0x5D, 0x38),
        detect_tokens: &["opencode"],
        history: HistoryKind::None,
    },
    ToolDef {
        id: "amp",
        name: "Amp",
        bin: "amp",
        alt_bins: &[],
        icon: TOOL_ICON_BASE + 11,
        brand: (0xF2, 0x6D, 0x21),
        detect_tokens: &["amp"],
        history: HistoryKind::None,
    },
];

/// Tokens that mark a pane as agent-ish without naming a specific tool. Kept apart
/// from [`TOOLS`] because they must never resolve to a branded pane — "agent" in a
/// title is a hint that *something* is running, not evidence of *what*.
pub static GENERIC_AI_TOKENS: &[&str] = &["llm", "chatgpt", "agent"];

/// Look a tool up by its stable id.
pub fn by_id(id: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|t| t.id == id)
}

/// The tool an OSC title names, if exactly one is named.
///
/// Case-insensitive **token** match, so `user@host: claude` hits and `ssh-agent`
/// does not accidentally resolve to an agent tool. Ambiguity returns `None`: a title
/// mentioning two tools is not evidence for either.
pub fn by_title(title: &str) -> Option<&'static ToolDef> {
    let lower = title.to_ascii_lowercase();
    let mut hit: Option<&'static ToolDef> = None;
    for tok in lower.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
        if tok.is_empty() {
            continue;
        }
        if let Some(t) = TOOLS.iter().find(|t| t.detect_tokens.contains(&tok)) {
            match hit {
                Some(prev) if prev.id != t.id => return None,
                _ => hit = Some(t),
            }
        }
    }
    hit
}

/// Every token that marks a pane as worth watching for agent idle — the union of the
/// registry's detect tokens and the generic ones. `glow::is_ai_pane` reads this so the
/// two lists cannot drift.
pub fn ai_tokens() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = TOOLS
        .iter()
        .flat_map(|t| t.detect_tokens.iter().copied())
        .chain(GENERIC_AI_TOKENS.iter().copied())
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_stable_shaped() {
        let mut ids: Vec<&str> = TOOLS.iter().map(|t| t.id).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate tool id in TOOLS");
        for t in TOOLS {
            assert!(!t.id.is_empty() && !t.bin.is_empty(), "{} has an empty field", t.name);
            assert!(
                t.id.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{} id must be lowercase-kebab (it is persisted)",
                t.id
            );
            assert!(!t.detect_tokens.is_empty(), "{} has no detect tokens", t.id);
        }
    }

    #[test]
    fn icon_kinds_are_unique_and_above_the_base() {
        let mut icons: Vec<u32> = TOOLS.iter().map(|t| t.icon).collect();
        let n = icons.len();
        icons.sort_unstable();
        icons.dedup();
        assert_eq!(icons.len(), n, "two tools share an icon kind");
        assert!(TOOLS.iter().all(|t| t.icon >= TOOL_ICON_BASE));
    }

    #[test]
    fn title_match_is_token_wise_and_refuses_ambiguity() {
        assert_eq!(by_title("user@host: claude").map(|t| t.id), Some("claude"));
        assert_eq!(by_title("cursor-agent — repo").map(|t| t.id), Some("cursor-agent"));
        // A word merely containing a tool name is not a match.
        assert!(by_title("ssh-agent").is_none());
        assert!(by_title("claudette").is_none());
        // Two tools named at once is evidence for neither.
        assert!(by_title("claude vs codex").is_none());
    }

    #[test]
    fn ai_tokens_cover_the_generic_ones() {
        let toks = ai_tokens();
        for g in GENERIC_AI_TOKENS {
            assert!(toks.contains(g), "generic token {g} missing");
        }
        assert!(toks.contains(&"claude"));
    }
}
