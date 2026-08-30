//! **Pane kinds on disk, pinned from both sides.**
//!
//! Tool panes (`docs/tool-panes-plan.md`) give every pane a *kind* — plain terminal,
//! a named CLI AI tool, or one of the non-PTY views. That kind is persisted, and the
//! riskiest thing about persisting anything is what happens to files written by a
//! build that disagrees with yours. These tests lock the contract in every direction,
//! mirroring `workspace_uid_compat.rs`, which does the same job for `PaneSpec.uid`.
//!
//! The design choice that makes this cheap: the kind rides in the existing open
//! `meta` map under `pane.kind`, not in a new `PaneSpec` field. So:
//!
//!   * **Backward** — a workspace written by an OLD build (no `meta`, or a `meta`
//!     without the key, at any nesting level) loads here and reads back `Terminal`.
//!   * **Forward** — a kind-bearing workspace deserializes on a build that predates
//!     the key. Proved literally, against a local struct mirroring the pre-feature
//!     `PaneSpec`. It works because nothing in the model is `deny_unknown_fields`;
//!     the test exists so adding that attribute breaks here, not in a downgrade.
//!   * **Unknown kinds** — a tool this build has never heard of round-trips byte-for-
//!     byte instead of being silently rewritten to `terminal`. A future build's file
//!     must survive a trip through today's build.
//!   * **No default written** — a plain terminal writes no key at all, so ordinary
//!     workspaces are byte-identical to what a build without this feature produces.
//!   * **Round-trip** at all three nesting levels (top-level panes, group panes,
//!     window→group panes).

use hyperpanes_core::tools::kind::{PaneKind, META_KIND_KEY};
use hyperpanes_core::workspace::io::{read_workspace, write_workspace};
use hyperpanes_core::workspace::model::{GroupSpec, PaneSpec, WindowSpec, WorkspaceFile};
use serde::Deserialize;
use std::collections::BTreeMap;

fn temp_file(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("hp-kind-compat-{}-{tag}.json", std::process::id()))
}

fn every_pane(ws: &WorkspaceFile) -> Vec<PaneSpec> {
    let mut out: Vec<PaneSpec> = ws.panes.clone().unwrap_or_default();
    for g in ws.groups.iter().flatten() {
        out.extend(g.panes.iter().cloned());
    }
    for w in ws.windows.iter().flatten() {
        for g in &w.groups {
            out.extend(g.panes.iter().cloned());
        }
    }
    out
}

fn pane_with_kind(command: &str, kind: PaneKind) -> PaneSpec {
    let mut p = PaneSpec {
        command: Some(command.to_string()),
        ..Default::default()
    };
    p.set_pane_kind(&kind);
    p
}

/// A workspace exactly as a build predating tool panes wrote it: panes at all three
/// nesting levels, `meta` present on one of them carrying the keys that already
/// existed, and **no `pane.kind` anywhere**. Literal bytes, so it cannot drift when
/// the model gains fields.
const LEGACY_WORKSPACE_JSON: &str = r##"{
  "name": "dev",
  "layout": "main-stack",
  "panes": [
    { "label": "server", "command": "npm run dev", "cwd": "/work" },
    { "command": "tail -f log", "meta": { "role": "logs", "ai.subtitle": "watching" } }
  ],
  "groups": [
    { "title": "build", "panes": [ { "command": "cargo watch" } ] }
  ],
  "windows": [
    { "groups": [ { "title": "w2", "panes": [ { "command": "htop" } ] } ] }
  ]
}"##;

// ---------------------------------------------------------------- backward compatibility

#[test]
fn a_legacy_workspace_without_pane_kinds_still_loads_as_terminals() {
    let path = temp_file("legacy-loads");
    std::fs::write(&path, LEGACY_WORKSPACE_JSON).unwrap();
    let ws = read_workspace(&path).expect("a pre-feature workspace must still load");
    let panes = every_pane(&ws);
    assert_eq!(panes.len(), 4, "all three nesting levels came through");
    for p in &panes {
        assert_eq!(
            p.pane_kind(),
            PaneKind::Terminal,
            "a pane with no kind key is a plain shell, not a broken pane"
        );
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_legacy_pane_keeps_the_meta_keys_it_already_had() {
    let path = temp_file("legacy-meta");
    std::fs::write(&path, LEGACY_WORKSPACE_JSON).unwrap();
    let ws = read_workspace(&path).unwrap();
    let meta = ws.panes.as_ref().unwrap()[1].meta.clone().expect("meta survived");
    assert_eq!(meta.get("role").map(String::as_str), Some("logs"));
    assert_eq!(meta.get("ai.subtitle").map(String::as_str), Some("watching"));
    assert!(!meta.contains_key(META_KIND_KEY));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_mixed_file_resolves_per_pane_not_per_file() {
    let path = temp_file("mixed");
    let ws = WorkspaceFile {
        name: Some("mixed".into()),
        panes: Some(vec![
            pane_with_kind("claude", PaneKind::Tool("claude".into())),
            PaneSpec {
                command: Some("zsh".into()),
                ..Default::default()
            },
            pane_with_kind("", PaneKind::Markdown),
        ]),
        ..Default::default()
    };
    assert!(write_workspace(&path, &ws), "write must succeed");
    let back = read_workspace(&path).unwrap();
    let panes = back.panes.unwrap();
    assert_eq!(panes[0].pane_kind(), PaneKind::Tool("claude".into()));
    assert_eq!(panes[1].pane_kind(), PaneKind::Terminal);
    assert_eq!(panes[2].pane_kind(), PaneKind::Markdown);
    let _ = std::fs::remove_file(&path);
}

// ------------------------------------------------------------------ forward compatibility

/// `PaneSpec` as a build predating tool panes sees it — the field set of the day.
/// `meta` is an open map there too, so the extra key is simply carried.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreKindPaneSpec {
    label: Option<String>,
    command: Option<String>,
    meta: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreKindWorkspaceFile {
    name: Option<String>,
    panes: Option<Vec<PreKindPaneSpec>>,
}

#[derive(Debug, Deserialize)]
struct PreKindEnvelope {
    workspace: PreKindWorkspaceFile,
}

#[test]
fn a_kind_bearing_file_still_loads_on_a_build_that_predates_the_key() {
    let path = temp_file("forward");
    let ws = WorkspaceFile {
        name: Some("new".into()),
        panes: Some(vec![{
            let mut p = pane_with_kind("claude", PaneKind::Tool("claude".into()));
            p.label = Some("agent".into());
            p
        }]),
        ..Default::default()
    };
    assert!(write_workspace(&path, &ws), "write must succeed");
    let raw = std::fs::read_to_string(&path).unwrap();

    // This deserialize failing is the alarm: something grew `deny_unknown_fields`,
    // and a user downgrading a build would lose their whole workspace file.
    let old: PreKindWorkspaceFile = serde_json::from_str::<PreKindEnvelope>(&raw)
        .map(|e| e.workspace)
        .or_else(|_| serde_json::from_str(&raw))
        .expect("an old build must still parse a kind-bearing workspace");

    let panes = old.panes.expect("panes");
    assert_eq!(old.name.as_deref(), Some("new"));
    assert_eq!(panes[0].label.as_deref(), Some("agent"));
    assert_eq!(panes[0].command.as_deref(), Some("claude"));
    // The old build sees the key as just another meta entry and preserves it.
    assert_eq!(
        panes[0].meta.as_ref().and_then(|m| m.get(META_KIND_KEY)).map(String::as_str),
        Some("claude")
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_kind_this_build_does_not_know_survives_a_round_trip_unchanged() {
    // A workspace from a future build that supports a tool we have never heard of.
    let path = temp_file("unknown-kind");
    let json = r##"{"name":"future","panes":[{"command":"x","meta":{"pane.kind":"some-future-tool"}}]}"##;
    std::fs::write(&path, json).unwrap();

    let ws = read_workspace(&path).unwrap();
    let kind = ws.panes.as_ref().unwrap()[0].pane_kind();
    assert_eq!(kind, PaneKind::Tool("some-future-tool".into()));

    // Save it back out on this build; the value must be byte-identical, not "terminal".
    let out = temp_file("unknown-kind-out");
    assert!(write_workspace(&out, &ws), "write must succeed");
    let back = read_workspace(&out).unwrap();
    assert_eq!(
        back.panes.as_ref().unwrap()[0]
            .meta
            .as_ref()
            .and_then(|m| m.get(META_KIND_KEY))
            .map(String::as_str),
        Some("some-future-tool"),
        "an unknown tool id must not be downgraded on save"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&out);
}

// ------------------------------------------------------------------------- round-tripping

#[test]
fn a_plain_terminal_writes_no_kind_key_at_all() {
    let path = temp_file("no-key");
    let mut p = PaneSpec {
        command: Some("zsh".into()),
        ..Default::default()
    };
    p.set_pane_kind(&PaneKind::Terminal);
    assert!(p.meta.is_none(), "setting the default kind must not create a meta map");

    let ws = WorkspaceFile {
        panes: Some(vec![p]),
        ..Default::default()
    };
    assert!(write_workspace(&path, &ws), "write must succeed");
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        !raw.contains(META_KIND_KEY),
        "an ordinary workspace must stay byte-identical to a pre-feature build's output"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn clearing_a_kind_removes_the_key_but_keeps_other_meta() {
    let mut p = pane_with_kind("claude", PaneKind::Tool("claude".into()));
    p.meta
        .get_or_insert_with(BTreeMap::new)
        .insert("role".into(), "agent".into());
    p.set_pane_kind(&PaneKind::Terminal);
    let meta = p.meta.expect("other meta keys must survive");
    assert!(!meta.contains_key(META_KIND_KEY));
    assert_eq!(meta.get("role").map(String::as_str), Some("agent"));
}

#[test]
fn pane_kinds_survive_a_disk_round_trip_at_every_nesting_level() {
    let path = temp_file("nesting");
    let ws = WorkspaceFile {
        name: Some("nested".into()),
        panes: Some(vec![pane_with_kind("claude", PaneKind::Tool("claude".into()))]),
        groups: Some(vec![GroupSpec {
            title: Some("g".into()),
            panes: vec![pane_with_kind("", PaneKind::FileBrowser)],
            ..Default::default()
        }]),
        windows: Some(vec![WindowSpec {
            groups: vec![GroupSpec {
                title: Some("w".into()),
                panes: vec![pane_with_kind("", PaneKind::Browser)],
                ..Default::default()
            }],
            ..Default::default()
        }]),
        ..Default::default()
    };
    assert!(write_workspace(&path, &ws), "write must succeed");
    let back = read_workspace(&path).unwrap();

    assert_eq!(
        back.panes.as_ref().unwrap()[0].pane_kind(),
        PaneKind::Tool("claude".into())
    );
    assert_eq!(
        back.groups.as_ref().unwrap()[0].panes[0].pane_kind(),
        PaneKind::FileBrowser
    );
    assert_eq!(
        back.windows.as_ref().unwrap()[0].groups[0].panes[0].pane_kind(),
        PaneKind::Browser
    );
    let _ = std::fs::remove_file(&path);
}
