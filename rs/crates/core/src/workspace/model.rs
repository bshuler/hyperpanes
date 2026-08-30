//! Workspace-file serde model — a 1:1 port of the `workspace.json` shape declared
//! in `src/main/workspace.ts` (`WorkspaceFile` / `WindowSpec` / `GroupSpec` /
//! `PaneSpec`, plus the nested `WindowBounds`).
//!
//! Parity rules baked in here:
//!   * JSON field names match the TS interfaces exactly (camelCase) via
//!     `#[serde(rename_all = "camelCase")]` — so `font_size` ⇄ `fontSize`,
//!     `main_fraction` ⇄ `mainFraction`.
//!   * Every optional carries `skip_serializing_if = "Option::is_none"`, so an
//!     unset field is OMITTED rather than written as `null` (downstream is strict).
//!   * Field declaration order mirrors the TS interface order, so a canonically
//!     ordered file round-trips byte-identically through `serde_json`
//!     pretty-printing (2-space indent — the same as `JSON.stringify(x, null, 2)`).
//!
//! `meta` is a `BTreeMap` (sorted keys): `serde_json` has no insertion-order map
//! without the `preserve_order` feature, which the frozen `Cargo.toml` does not
//! enable. Sorted-key files round-trip exactly. The TS source declares the pane
//! fields as label/color/command/args/cwd/shell/fontSize/meta — there is no
//! `subtitle` (the stub header's mention was stale), so this port omits it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::tools::kind::{PaneKind, META_KIND_KEY};

/// One terminal pane: an optional shell command plus presentation/launch hints.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// literal argv for a direct (no-shell) spawn with `command` (P4a)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_size: Option<u32>,
    /// free-form per-pane metadata (agent-orchestration C)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<BTreeMap<String, String>>,
    /// The pane's live session uid at snapshot time, so a later load can `Attach{uid}` a
    /// surviving daemon session instead of re-spawning it (session-daemon plan, M2
    /// re-attach; the decision is `SessionManager::pane_load`).
    ///
    /// **Always optional, in both directions.** It is written by the relaunch ("last
    /// session") snapshot and — since M6 — by "Save workspace"/"Save workspace as…" and by
    /// the member workspaces a set writes, so a saved workspace can be re-opened while its
    /// panes are still running. A file that omits it (anything written before M6, or a
    /// hand-authored launch template) is fully valid: every pane simply spawns fresh, which
    /// is exactly the pre-M6 behaviour. Nothing here is `deny_unknown_fields`, so a file
    /// carrying a uid also loads on a build that predates the field — it is ignored.
    ///
    /// New here (no TS sibling), so it trails the ported fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    /// Per-pane "talk" (speak new Claude assistant replies aloud via local TTS), recorded only
    /// when on (a plain pane omits it). New here (no TS sibling), so it trails the ported fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub talk: Option<bool>,
}

impl PaneSpec {
    /// What kind of pane this is, read out of `meta["pane.kind"]`.
    ///
    /// A spec with no `meta`, or no such key, is a plain [`PaneKind::Terminal`] — which
    /// is every pane written before tool panes existed, and every pane that is still
    /// just a shell. See `tools::kind` for why the discriminator rides in `meta`
    /// instead of becoming a field of its own.
    pub fn pane_kind(&self) -> PaneKind {
        self.meta
            .as_ref()
            .and_then(|m| m.get(META_KIND_KEY))
            .map(|v| PaneKind::from_meta_value(v))
            .unwrap_or_default()
    }

    /// Record this pane's kind. `Terminal` *removes* the key rather than writing
    /// `"terminal"`, so an ordinary pane's file stays byte-identical to what a build
    /// without this feature would write, and an empty `meta` map is dropped entirely.
    pub fn set_pane_kind(&mut self, kind: &PaneKind) {
        match kind.as_meta_value() {
            Some(v) => {
                self.meta
                    .get_or_insert_with(BTreeMap::new)
                    .insert(META_KIND_KEY.to_string(), v);
            }
            None => {
                if let Some(m) = self.meta.as_mut() {
                    m.remove(META_KIND_KEY);
                    if m.is_empty() {
                        self.meta = None;
                    }
                }
            }
        }
    }
}

/// One tab (group): a layout plus its panes and per-slot split state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    #[serde(default)]
    pub panes: Vec<PaneSpec>,
    /// per-slot split fractions (sum→1); length must match panes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sizes: Option<Vec<f64>>,
    /// main-stack split fraction (0<f<1)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_fraction: Option<f64>,
    /// index of the focused pane (default 0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused: Option<u32>,
    /// index of the maximized pane (default: none)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zoomed: Option<u32>,
}

/// Saved OS-window geometry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowBounds {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximized: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fullscreen: Option<bool>,
}

/// One OS window: its tabs (groups), the active tab index, and optional bounds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<WindowBounds>,
    #[serde(default)]
    pub groups: Vec<GroupSpec>,
}

/// The top-level workspace file. Panes may be described at any nesting level
/// (`panes` / `groups` / `windows`); all three slots are optional.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panes: Option<Vec<PaneSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<GroupSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows: Option<Vec<WindowSpec>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deserialize → re-serialize (pretty, 2-space) must reproduce the input
    /// bytes exactly — the byte-identical round-trip contract.
    fn assert_round_trips(json: &str) {
        let parsed: WorkspaceFile = serde_json::from_str(json).expect("parse");
        let out = serde_json::to_string_pretty(&parsed).expect("serialize");
        assert_eq!(out, json, "round-trip mismatch");
    }

    #[test]
    fn round_trips_top_level_panes_shape() {
        let json = r##"{
  "name": "dev",
  "layout": "main-stack",
  "panes": [
    {
      "label": "server",
      "color": "#e5484d",
      "command": "npm run dev",
      "cwd": "/work",
      "shell": "pwsh",
      "fontSize": 14
    },
    {
      "command": "tail -f log"
    }
  ]
}"##;
        assert_round_trips(json);
    }

    #[test]
    fn round_trips_groups_shape_with_split_state() {
        let json = r#"{
  "name": "x",
  "groups": [
    {
      "title": "t1",
      "layout": "columns",
      "panes": [
        {
          "command": "a"
        },
        {
          "command": "b"
        }
      ],
      "sizes": [
        0.5,
        0.5
      ],
      "mainFraction": 0.6,
      "focused": 1,
      "zoomed": 0
    }
  ],
  "active": 0
}"#;
        assert_round_trips(json);
    }

    #[test]
    fn round_trips_windows_shape_with_bounds() {
        let json = r#"{
  "windows": [
    {
      "title": "w",
      "active": 1,
      "bounds": {
        "x": -10,
        "y": 0,
        "width": 1280,
        "height": 720,
        "maximized": false,
        "fullscreen": true
      },
      "groups": [
        {
          "panes": [
            {
              "label": "a"
            }
          ]
        }
      ]
    }
  ]
}"#;
        assert_round_trips(json);
    }

    /// Session-daemon prep: a pane's live `uid` (camelCase) plus its spawn `command`/`shell`
    /// survive the round-trip, so the relaunch snapshot carries what M2 re-attach needs (match
    /// a surviving session by uid; re-run the original program if it's gone). A pane with no
    /// uid omits the field entirely (`skip_serializing_if`).
    #[test]
    fn round_trips_pane_with_uid_and_command() {
        let json = r#"{
  "panes": [
    {
      "command": "claude",
      "shell": "pwsh",
      "uid": "pane-7"
    }
  ]
}"#;
        assert_round_trips(json);
        let parsed: WorkspaceFile = serde_json::from_str(json).unwrap();
        let panes = parsed.panes.unwrap();
        assert_eq!(panes[0].uid.as_deref(), Some("pane-7"));
        assert_eq!(panes[0].command.as_deref(), Some("claude"));
        // A pane left without a uid serializes nothing for it (not `null`).
        let bare = serde_json::to_string(&PaneSpec {
            command: Some("bash".into()),
            ..Default::default()
        })
        .unwrap();
        assert!(!bare.contains("uid"), "unset uid omitted: {bare}");
    }

    #[test]
    fn round_trips_pane_with_args_and_meta() {
        let json = r#"{
  "panes": [
    {
      "command": "claude",
      "args": [
        "--model",
        "opus"
      ],
      "meta": {
        "ai.subtitle": "working",
        "role": "worker"
      }
    }
  ]
}"#;
        assert_round_trips(json);
    }

    #[test]
    fn omits_unset_optionals_rather_than_writing_null() {
        let ws = WorkspaceFile {
            name: Some("only-name".into()),
            panes: Some(vec![PaneSpec {
                command: Some("bash".into()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let out = serde_json::to_string_pretty(&ws).unwrap();
        assert!(
            !out.contains("null"),
            "no field should serialize as null: {out}"
        );
        assert!(!out.contains("layout"), "unset layout omitted");
        assert!(!out.contains("label"), "unset pane label omitted");
        assert_eq!(
            out,
            r#"{
  "name": "only-name",
  "panes": [
    {
      "command": "bash"
    }
  ]
}"#
        );
    }

    #[test]
    fn empty_object_deserializes_to_all_none() {
        let parsed: WorkspaceFile = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, WorkspaceFile::default());
    }

    /// Task 14: a zoomed pane's per-pane terminal font size survives the workspace
    /// round-trip (`font_size` ⇄ `fontSize`), while a pane left at the base size omits it
    /// (`skip_serializing_if`). This is the persistence contract the native app relies on to
    /// restore per-pane zoom across a relaunch.
    #[test]
    fn pane_zoom_font_size_round_trips() {
        let ws = WorkspaceFile {
            name: Some("z".into()),
            panes: Some(vec![
                PaneSpec {
                    label: Some("zoomed".into()),
                    font_size: Some(20),
                    ..Default::default()
                },
                PaneSpec {
                    label: Some("base".into()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&ws).unwrap();
        // The zoomed pane records fontSize; the base pane omits it entirely.
        assert!(
            json.contains("\"fontSize\": 20"),
            "zoomed pane keeps its size: {json}"
        );
        let parsed: WorkspaceFile = serde_json::from_str(&json).unwrap();
        let panes = parsed.panes.clone().unwrap();
        assert_eq!(panes[0].font_size, Some(20));
        assert_eq!(panes[1].font_size, None);
        // Whole value round-trips identically.
        assert_eq!(parsed, ws);
    }
}
