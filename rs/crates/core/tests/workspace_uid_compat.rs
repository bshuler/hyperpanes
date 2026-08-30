//! **The M6 on-disk shape change, pinned from both sides.**
//!
//! M6 (`docs/mux-backend-plan.md`) made "Save workspace"/"Save workspace as…" and the member
//! workspaces a set writes record each pane's durable session `uid`, so loading can be
//! **reattach-or-spawn** per pane instead of always re-spawning. That changes what a saved
//! workspace file *contains*, which is the riskiest thing in the milestone. These tests lock
//! the compatibility contract in **both directions**:
//!
//!   * **Backward** — a workspace written by an OLD build (no `uid` anywhere, at any nesting
//!     level) still loads on this one, every pane reads back `uid: None`, and the
//!     reattach-or-spawn decision therefore degrades to *spawn everything* — byte-for-byte
//!     the pre-M6 behaviour.
//!   * **Forward** — a workspace written by THIS build (uids present) still deserializes on a
//!     build that predates the field. That is proved literally: the uid-bearing JSON is
//!     deserialized into a local struct that mirrors the **pre-M6 `PaneSpec`** (no `uid`
//!     field). It succeeds because nothing in the model is `deny_unknown_fields` — the test
//!     exists so adding that attribute breaks here rather than in a user's downgrade.
//!   * **Round-trip** — the new field survives write→read unchanged, is emitted in camelCase,
//!     and an unset uid is OMITTED rather than written as `null`.
//!
//! Sibling coverage: `workspace_format.rs` (the shape at large), `session_manager.rs` tests
//! (the in-process decision), `session::daemon_client` tests (the decision against a REAL
//! daemon — a live uid re-attaches, a dead one spawns).

use hyperpanes_core::session_manager::{PaneLoad, SessionManager};
use hyperpanes_core::workspace::io::{read_workspace, windows_of, write_workspace};
use hyperpanes_core::workspace::model::{GroupSpec, PaneSpec, WorkspaceFile};
use serde::Deserialize;

fn temp_file(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("hp-uid-compat-{}-{tag}.json", std::process::id()))
}

fn in_process_manager() -> SessionManager {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::mem::forget(rx); // keep the channel open for the manager's lifetime
    SessionManager::new(tx)
}

/// A workspace exactly as a PRE-M6 build wrote it: the full field set of the day
/// (label/color/command/args/cwd/shell/fontSize/meta) across all three nesting levels, and
/// **no `uid` key anywhere**. Kept as literal bytes, not as a serialized `WorkspaceFile`, so
/// it cannot silently drift when the model gains fields.
const LEGACY_WORKSPACE_JSON: &str = r##"{
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
    { "command": "tail -f log" }
  ],
  "groups": [
    {
      "title": "build",
      "layout": "columns",
      "panes": [
        { "label": "a", "command": "claude", "args": ["--model", "opus"] },
        { "label": "b", "meta": { "role": "worker" } }
      ],
      "sizes": [0.5, 0.5],
      "mainFraction": 0.6,
      "focused": 1
    }
  ],
  "active": 0,
  "windows": [
    {
      "title": "main",
      "active": 0,
      "bounds": { "x": 10, "y": 20, "width": 1200, "height": 800 },
      "groups": [ { "title": "w", "panes": [ { "command": "htop" } ] } ]
    }
  ]
}"##;

/// Every pane of every nesting level of the legacy fixture, flattened.
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

// ---------------------------------------------------------------- backward compatibility

#[test]
fn a_legacy_workspace_file_without_pane_uids_still_loads() {
    let path = temp_file("legacy-loads");
    std::fs::write(&path, LEGACY_WORKSPACE_JSON.as_bytes()).unwrap();

    let ws = read_workspace(&path).expect("a pre-M6 workspace file must still load");
    assert_eq!(ws.name.as_deref(), Some("dev"));
    // The content survived intact — this is a real load, not an empty fallback.
    let panes = every_pane(&ws);
    assert_eq!(panes.len(), 5, "all three nesting levels parsed: {panes:?}");
    assert_eq!(panes[0].label.as_deref(), Some("server"));
    assert_eq!(panes[0].command.as_deref(), Some("npm run dev"));
    assert_eq!(panes[0].font_size, Some(14));
    assert_eq!(panes[2].args.as_deref().map(<[String]>::len), Some(2));
    assert_eq!(panes[4].command.as_deref(), Some("htop")); // the windows[] leg
                                                           // …and the NEW field is simply absent on every one of them.
    for (i, p) in panes.iter().enumerate() {
        assert_eq!(
            p.uid, None,
            "legacy pane {i} must read back uid-less: {p:?}"
        );
    }
    // The launcher's view of the file is unaffected by the new field's absence.
    assert_eq!(windows_of(Some(&ws)).len(), 1);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_legacy_workspace_degrades_to_spawn_everything() {
    // The whole point of the compatibility promise: with no recorded uids there is nothing
    // to re-attach to, so every pane of a pre-M6 file takes the SPAWN branch — under a
    // freshly minted uid, never an adopted one — which is exactly the pre-M6 behaviour.
    let path = temp_file("legacy-spawns");
    std::fs::write(&path, LEGACY_WORKSPACE_JSON.as_bytes()).unwrap();
    let ws = read_workspace(&path).expect("legacy file loads");
    let mgr = in_process_manager();

    let mut minted = Vec::new();
    for (i, spec) in every_pane(&ws).iter().enumerate() {
        let load = mgr.pane_load(spec.uid.as_deref());
        assert!(
            !load.is_reattach(),
            "legacy pane {i} must SPAWN, got {load:?}"
        );
        assert!(matches!(load, PaneLoad::Spawn(_)));
        assert!(
            load.uid().starts_with("pane-") || load.uid().starts_with('s'),
            "spawn mints a backend uid, got {}",
            load.uid()
        );
        minted.push(load.uid().to_string());
    }
    let unique: std::collections::BTreeSet<_> = minted.iter().collect();
    assert_eq!(
        unique.len(),
        minted.len(),
        "each spawned pane gets its own uid: {minted:?}"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_mixed_file_decides_per_pane_not_per_file() {
    // A file half-written by an old build and half by a new one (or one whose sessions have
    // partly died) must not decide globally: the uid-less pane spawns, and the uid-bearing
    // pane ALSO spawns here only because this backend is in-process. The invariant under
    // test is that the decision is taken per pane, from that pane's own uid.
    let mgr = in_process_manager();
    let specs = [
        PaneSpec {
            command: Some("bash".into()),
            ..Default::default()
        },
        PaneSpec {
            command: Some("claude".into()),
            uid: Some("pane-from-a-newer-save".into()),
            ..Default::default()
        },
    ];
    let loads: Vec<PaneLoad> = specs
        .iter()
        .map(|s| mgr.pane_load(s.uid.as_deref()))
        .collect();
    assert!(loads.iter().all(|l| !l.is_reattach()));
    assert_ne!(
        loads[1].uid(),
        "pane-from-a-newer-save",
        "a recorded uid is never adopted without a live session behind it"
    );
    assert_ne!(loads[0].uid(), loads[1].uid());
}

// ----------------------------------------------------------------- forward compatibility

/// The pane shape a build that PREDATES the `uid` field knows about. Deserializing a
/// uid-bearing file into this is what an older hyperpanes does when a user downgrades.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreM6PaneSpec {
    label: Option<String>,
    command: Option<String>,
    font_size: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreM6WorkspaceFile {
    name: Option<String>,
    panes: Option<Vec<PreM6PaneSpec>>,
}

/// The envelope as an older build sees it — only the payload shape it knows.
#[derive(Debug, Deserialize)]
struct PreM6Envelope {
    workspace: PreM6WorkspaceFile,
}

/// Everything an old build must still get out of a uid-bearing payload.
fn assert_old_build_reads_it(old: &PreM6WorkspaceFile) {
    let panes = old.panes.as_ref().expect("panes");
    assert_eq!(old.name.as_deref(), Some("new"));
    assert_eq!(panes[0].label.as_deref(), Some("server"));
    assert_eq!(panes[0].command.as_deref(), Some("npm run dev"));
    assert_eq!(panes[0].font_size, Some(16));
}

#[test]
fn a_uid_bearing_file_still_loads_on_a_build_that_predates_the_field() {
    // Written by THIS build, with uids.
    let path = temp_file("forward");
    let ws = WorkspaceFile {
        name: Some("new".into()),
        panes: Some(vec![PaneSpec {
            label: Some("server".into()),
            command: Some("npm run dev".into()),
            font_size: Some(16),
            uid: Some("pane-8f14e45f-ea8f-4b0d-9c1a-2f3c4d5e6f70".into()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    // (a) The BARE payload — what a pre-envelope build wrote and reads. The unknown `uid`
    //     key is ignored and everything it DOES know about still arrives. If the model ever
    //     gains `deny_unknown_fields`, this is where it fails — not in a user's downgrade.
    let bare = serde_json::to_string_pretty(&ws).unwrap();
    assert!(
        bare.contains("\"uid\""),
        "the new build records uid: {bare}"
    );
    let old: PreM6WorkspaceFile =
        serde_json::from_str(&bare).expect("a pre-M6 build must still parse a uid-bearing payload");
    assert_old_build_reads_it(&old);

    // (b) The same, through the versioned container the writer actually emits: the uid rides
    //     inside `workspace`, and an envelope-aware older build still reads its payload.
    assert!(write_workspace(&path, &ws));
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("\"uid\""),
        "the file on disk records uid: {raw}"
    );
    let enveloped: PreM6Envelope =
        serde_json::from_str(&raw).expect("a pre-M6 build must still parse the enveloped file");
    assert_old_build_reads_it(&enveloped.workspace);

    let _ = std::fs::remove_file(&path);
}

// ------------------------------------------------------------------------- new-format I/O

#[test]
fn pane_uids_survive_a_disk_round_trip_at_every_nesting_level() {
    let path = temp_file("roundtrip");
    let ws = WorkspaceFile {
        name: Some("library".into()),
        panes: Some(vec![PaneSpec {
            command: Some("claude".into()),
            uid: Some("pane-top".into()),
            ..Default::default()
        }]),
        groups: Some(vec![GroupSpec {
            panes: vec![
                PaneSpec {
                    command: Some("htop".into()),
                    uid: Some("pane-group".into()),
                    ..Default::default()
                },
                // A pane with no live session: uid stays unset through the round-trip.
                PaneSpec {
                    command: Some("bash".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }]),
        ..Default::default()
    };
    assert!(write_workspace(&path, &ws));
    let back = read_workspace(&path).expect("reads back");
    assert_eq!(back, ws, "the uid-bearing file round-trips losslessly");

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("\"uid\": \"pane-top\"") && raw.contains("\"uid\": \"pane-group\""),
        "uid is written in camelCase at every level: {raw}"
    );
    assert!(
        !raw.contains("null"),
        "an unset uid is OMITTED, never null: {raw}"
    );
    assert_eq!(
        raw.matches("\"uid\"").count(),
        2,
        "the uid-less pane writes no uid key: {raw}"
    );

    let _ = std::fs::remove_file(&path);
}
