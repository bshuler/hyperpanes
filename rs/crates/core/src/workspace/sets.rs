//! Workspace **sets** — the library layer on top of [`WorkspaceFile`]
//! (`docs/mux-backend-plan.md` M6).
//!
//! A [`WorkspaceSet`] is *a name plus references to member workspaces*: it owns no panes
//! of its own, it points at workspace files. Sets live as `sets/*.json` under the canonical
//! data dir ([`paths::sets_dir`]); the members that `SaveSet` generates go under
//! [`paths::set_members_dir`], though a set may reference a workspace file anywhere.
//!
//! The serde idiom is [`crate::workspace::io`]'s, deliberately and exactly:
//!   * camelCase field names, `skip_serializing_if = "Option::is_none"` on every optional
//!     (an unset field is OMITTED, never `null`), declaration order = canonical file order,
//!     so a canonical file round-trips byte-identically through 2-space pretty printing;
//!   * a **versioned container** ([`SetEnvelope`]) `{ "format": "hyperpanes-set",
//!     "version": 1, "set": {…} }` — the reader also accepts a bare legacy [`WorkspaceSet`]
//!     object ("version 0"), a wrong `format` or a too-new `version` is a clear error;
//!   * member paths are resolved relative to the set file's own directory on read, exactly
//!     the way [`io::resolve_cwds`](crate::workspace::io::resolve_cwds) resolves pane cwds;
//!   * writes go through [`paths::write_atomic`] — the one write path this repo has. No
//!     second hand-rolled one.

use crate::persistence::paths;
use crate::workspace::io;
use crate::workspace::model::WorkspaceFile;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The magic `format` discriminator of a versioned workspace-set container.
pub const SET_FORMAT: &str = "hyperpanes-set";
/// The newest set-envelope `version` this build reads, and the version it writes.
pub const SET_VERSION: u32 = 1;

/// One member of a set: a reference to a workspace file, not an inline copy of it. A set
/// is a *library index*, so editing the workspace updates every set that names it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetMember {
    /// Path to the member workspace file (`.hyperpanes` or legacy `.json`). A relative path
    /// is resolved against the set file's own directory on read (see [`resolve_members`]),
    /// so a set + its workspaces stay movable as a folder.
    pub path: String,
    /// Display name for the member. Absent ⇒ fall back to the workspace's own `name`, then
    /// to the file stem.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A named collection of workspace references — the `sets/*.json` payload.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSet {
    /// Human name of the set (the library entry's label). The file *stem* is a slug of it
    /// ([`slug`]); this is the authoritative display form.
    pub name: String,
    /// The member workspaces, in the order they should be opened.
    #[serde(default)]
    pub members: Vec<SetMember>,
}

/// The versioned on-disk container: `{ "format": "hyperpanes-set", "version": 1,
/// "set": { … } }`. Field declaration order is the canonical file order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetEnvelope {
    pub format: String,
    pub version: u32,
    pub set: WorkspaceSet,
}

impl SetEnvelope {
    /// Wrap a set payload in the current-version envelope.
    pub fn wrap(set: WorkspaceSet) -> Self {
        Self {
            format: SET_FORMAT.to_string(),
            version: SET_VERSION,
            set,
        }
    }
}

/// Filesystem-safe stem for a set name: lowercase, non-alphanumerics collapsed to `-`,
/// trimmed. An empty/entirely-punctuation name yields `"set"` so a path is always valid.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        "set".to_string()
    } else {
        out
    }
}

/// Parse set-file text, accepting both shapes: the versioned envelope (a top-level object
/// with a `format` key) and a bare legacy [`WorkspaceSet`] ("version 0"). Mirrors
/// [`io::parse_workspace_str`] one-for-one.
pub fn parse_set_str(raw: &str) -> Result<WorkspaceSet, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid JSON: {e}"))?;

    let is_envelope = value
        .as_object()
        .is_some_and(|obj| obj.contains_key("format"));
    if !is_envelope {
        // Legacy bare WorkspaceSet (version 0).
        return serde_json::from_value(value).map_err(|e| format!("invalid workspace set: {e}"));
    }

    let obj = value.as_object().unwrap();
    match obj.get("format").and_then(|f| f.as_str()) {
        Some(SET_FORMAT) => {}
        other => {
            return Err(format!(
                "not a hyperpanes workspace set: \"format\" is {:?}, expected \"{SET_FORMAT}\"",
                other.unwrap_or("<non-string>")
            ));
        }
    }
    match obj.get("version").and_then(|v| v.as_u64()) {
        Some(v) if (1..=SET_VERSION as u64).contains(&v) => {}
        Some(v) => {
            return Err(format!(
                "workspace set version {v} is newer than this build understands \
                 (max {SET_VERSION}) — update hyperpanes to open it"
            ));
        }
        None => {
            return Err(
                "hyperpanes workspace set is missing a numeric \"version\" field".to_string(),
            );
        }
    }
    let set = obj
        .get("set")
        .cloned()
        .ok_or_else(|| "hyperpanes workspace set is missing the \"set\" payload".to_string())?;
    serde_json::from_value(set).map_err(|e| format!("invalid workspace set payload: {e}"))
}

/// Resolve every relative member `path` against `base_dir` (absolute paths kept verbatim),
/// so a set file and the workspaces beside it move together.
pub fn resolve_members(set: &WorkspaceSet, base_dir: &Path) -> WorkspaceSet {
    let mut out = set.clone();
    for m in out.members.iter_mut() {
        let p = Path::new(&m.path);
        if p.is_relative() {
            m.path = base_dir.join(p).to_string_lossy().into_owned();
        }
    }
    out
}

/// Read + validate a set file, resolving relative member paths against its own directory.
/// `None` on read/parse error (parse failures are reported on stderr, exactly like
/// [`io::read_workspace`]).
pub fn read_set<P: AsRef<Path>>(path: P) -> Option<WorkspaceSet> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path).ok()?;
    let set = match parse_set_str(&raw) {
        Ok(set) => set,
        Err(e) => {
            tracing::warn!("{}: {e}", path.display());
            return None;
        }
    };
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    Some(resolve_members(&set, base))
}

/// Write a set file (pretty, 2-space) in the versioned container form, atomically through
/// [`paths::write_atomic`] (creating `sets/` if needed). Returns `false` on error, matching
/// [`io::write_workspace`]'s boolean.
pub fn write_set<P: AsRef<Path>>(path: P, set: &WorkspaceSet) -> bool {
    let Ok(json) = serde_json::to_string_pretty(&SetEnvelope::wrap(set.clone())) else {
        return false;
    };
    paths::write_atomic(path.as_ref(), json.as_bytes()).is_ok()
}

/// The canonical path a set with this name is saved to: `sets/<slug>.json`.
pub fn path_for(name: &str) -> PathBuf {
    paths::sets_dir().join(format!("{}.json", slug(name)))
}

/// Every readable set in `dir`, sorted by file name. Unreadable/invalid files are skipped
/// (a corrupt file must not hide the rest of the library).
pub fn list_sets_in(dir: &Path) -> Vec<(PathBuf, WorkspaceSet)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        })
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| read_set(&p).map(|s| (p, s)))
        .collect()
}

/// Every readable set in the canonical [`paths::sets_dir`].
pub fn list_sets() -> Vec<(PathBuf, WorkspaceSet)> {
    list_sets_in(&paths::sets_dir())
}

/// Read each member workspace of `set`, in order. Members that don't resolve to a valid
/// workspace are skipped (with a stderr note) rather than failing the whole open — a set is
/// a loose index and one stale reference must not cost the user the other tabs.
pub fn load_members(set: &WorkspaceSet) -> Vec<WorkspaceFile> {
    set.members
        .iter()
        .filter_map(|m| match io::read_workspace(&m.path) {
            Some(f) => Some(f),
            None => {
                tracing::warn!(
                    "set {:?}: member {:?} is not a valid workspace — skipped",
                    set.name, m.path
                );
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::model::{GroupSpec, PaneSpec};

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hp-sets-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// `set_members_dir` nests inside `sets_dir`, so the set scan runs straight over it.
    /// It must survive that: a subdirectory is not a set, not even one named `*.json`.
    #[test]
    fn the_scan_ignores_subdirectories_including_the_members_dir() {
        let dir = temp_dir("subdirs");
        assert!(write_set(dir.join("real.json"), &sample()));
        std::fs::create_dir_all(dir.join("members")).unwrap();
        // A directory whose own name would pass the extension filter, to pin that the
        // is_file() half of the filter is what's doing the work.
        std::fs::create_dir_all(dir.join("decoy.json")).unwrap();

        let found = list_sets_in(&dir);
        assert_eq!(found.len(), 1, "only the real set: {found:?}");
        assert_eq!(found[0].1.name, "Morning");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample() -> WorkspaceSet {
        WorkspaceSet {
            name: "Morning".into(),
            members: vec![
                SetMember {
                    path: "/ws/dev.hyperpanes".into(),
                    name: Some("dev".into()),
                },
                SetMember {
                    path: "/ws/ops.hyperpanes".into(),
                    name: None,
                },
            ],
        }
    }

    #[test]
    fn round_trips_through_the_versioned_envelope() {
        let set = sample();
        let json = serde_json::to_string_pretty(&SetEnvelope::wrap(set.clone())).unwrap();
        assert_eq!(parse_set_str(&json).unwrap(), set);
    }

    /// The parity contract: camelCase, unset optionals OMITTED (never `null`), declaration
    /// order canonical — so the file is byte-identical to `JSON.stringify(x, null, 2)`.
    #[test]
    fn canonical_file_shape_is_byte_exact() {
        let json = serde_json::to_string_pretty(&SetEnvelope::wrap(sample())).unwrap();
        assert_eq!(
            json,
            "{\n  \"format\": \"hyperpanes-set\",\n  \"version\": 1,\n  \"set\": {\n    \
             \"name\": \"Morning\",\n    \"members\": [\n      {\n        \
             \"path\": \"/ws/dev.hyperpanes\",\n        \"name\": \"dev\"\n      },\n      {\n        \
             \"path\": \"/ws/ops.hyperpanes\"\n      }\n    ]\n  }\n}"
        );
    }

    #[test]
    fn accepts_a_bare_legacy_set_object() {
        let bare = r#"{ "name": "legacy", "members": [ { "path": "a.json" } ] }"#;
        let set = parse_set_str(bare).unwrap();
        assert_eq!(set.name, "legacy");
        assert_eq!(set.members[0].path, "a.json");
        assert_eq!(set.members[0].name, None);
    }

    #[test]
    fn rejects_a_wrong_format_or_future_version() {
        let wrong = r#"{ "format": "hyperpanes", "version": 1, "set": { "name": "x" } }"#;
        assert!(parse_set_str(wrong)
            .unwrap_err()
            .contains("not a hyperpanes workspace set"));
        let future = r#"{ "format": "hyperpanes-set", "version": 99, "set": { "name": "x" } }"#;
        assert!(parse_set_str(future)
            .unwrap_err()
            .contains("newer than this build"));
        let versionless = r#"{ "format": "hyperpanes-set", "set": { "name": "x" } }"#;
        assert!(parse_set_str(versionless).unwrap_err().contains("version"));
        let payloadless = r#"{ "format": "hyperpanes-set", "version": 1 }"#;
        assert!(parse_set_str(payloadless)
            .unwrap_err()
            .contains("\"set\" payload"));
    }

    #[test]
    fn write_then_read_round_trips_on_disk_creating_the_dir() {
        let dir = temp_dir("write-read").join("sets"); // deliberately absent
        let p = dir.join("morning.json");
        assert!(write_set(&p, &sample()), "atomic write creates sets/");
        assert_eq!(read_set(&p).unwrap(), sample()); // absolute members kept verbatim
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn relative_member_paths_resolve_against_the_set_file_dir() {
        let dir = temp_dir("relative");
        let p = dir.join("s.json");
        let set = WorkspaceSet {
            name: "rel".into(),
            members: vec![SetMember {
                path: "ws/a.hyperpanes".into(),
                name: None,
            }],
        };
        assert!(write_set(&p, &set));
        let back = read_set(&p).unwrap();
        assert_eq!(
            back.members[0].path,
            dir.join("ws/a.hyperpanes").to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slugs_are_filesystem_safe() {
        assert_eq!(slug("Morning Routine"), "morning-routine");
        assert_eq!(slug("  ../../etc/passwd "), "etc-passwd");
        assert_eq!(slug("A/B:C"), "a-b-c");
        assert_eq!(slug("!!!"), "set");
        assert_eq!(slug(""), "set");
    }

    #[test]
    fn listing_skips_corrupt_files_and_sorts() {
        let dir = temp_dir("listing");
        assert!(write_set(
            dir.join("b.json"),
            &WorkspaceSet {
                name: "B".into(),
                ..Default::default()
            }
        ));
        assert!(write_set(
            dir.join("a.json"),
            &WorkspaceSet {
                name: "A".into(),
                ..Default::default()
            }
        ));
        std::fs::write(dir.join("bad.json"), b"not json {").unwrap();
        std::fs::write(dir.join("ignored.txt"), b"{}").unwrap();
        let found = list_sets_in(&dir);
        assert_eq!(
            found
                .iter()
                .map(|(_, s)| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A set indexes workspace FILES: `load_members` reads them back through the normal
    /// workspace reader, and a stale reference is skipped instead of failing the open.
    #[test]
    fn load_members_reads_workspaces_and_skips_stale_references() {
        let dir = temp_dir("members");
        let ws = dir.join("dev.hyperpanes");
        let file = WorkspaceFile {
            name: Some("dev".into()),
            groups: Some(vec![GroupSpec {
                panes: vec![PaneSpec {
                    command: Some("claude".into()),
                    uid: Some("pane-abc".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert!(io::write_workspace(&ws, &file));
        let set = WorkspaceSet {
            name: "s".into(),
            members: vec![
                SetMember {
                    path: ws.to_string_lossy().into_owned(),
                    name: None,
                },
                SetMember {
                    path: dir.join("gone.hyperpanes").to_string_lossy().into_owned(),
                    name: None,
                },
            ],
        };
        let loaded = load_members(&set);
        assert_eq!(loaded.len(), 1, "the missing member is skipped, not fatal");
        let pane = &loaded[0].groups.as_ref().unwrap()[0].panes[0];
        assert_eq!(pane.command.as_deref(), Some("claude"));
        // The durable pane id survives the set → workspace → load round-trip; it is what
        // the reattach-or-spawn decision keys on.
        assert_eq!(pane.uid.as_deref(), Some("pane-abc"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
