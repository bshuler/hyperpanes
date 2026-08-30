//! Repo-local project files: `.hyperpanes/project.json`, the layout as a property of
//! the *checkout* instead of the machine.
//!
//! The persistence layer keys a saved layout by workspace uid under the user's
//! app-support directory, which means it travels with the laptop and dies with it. A
//! file inside the repo travels with the repo: it clones, it branches, it can be
//! committed or `.gitignore`d to the owner's taste, and it is still there after a reboot
//! or a six-month pause. Opening `~/code/tplx` can then answer "which windows, and what
//! was I doing in each" from the checkout alone.
//!
//! Three rules shape everything below.
//!
//! **One format.** The payload is the ordinary [`WorkspaceFile`] inside the ordinary
//! [`io::WorkspaceEnvelope`] — same `format`/`version` header, same `Option<T>` +
//! `skip_serializing_if` compat rule, same [`io::ENVELOPE_VERSION`]. A second
//! serialization of the same data is how two readers drift apart, so there isn't one:
//! [`io::parse_workspace_str`] reads this file too, and the one genuinely new field,
//! [`PaneSpec::note`], was added to the shared model additively rather than forked into
//! a parallel struct here.
//!
//! **A running layout is never overwritten.** [`resolve`] is the whole precedence rule
//! as one pure function: a live session wins, the repo file is the fallback when there
//! is no live session and the seed on first open.
//!
//! **Never a secret store.** This file lands in a git working tree, so a token written
//! here is a token pushed to a remote. The envelope gives it nowhere obvious to go —
//! there is no env block and no auth field, only commands, cwds and notes — and the one
//! free-form slot, `PaneSpec::meta`, is swept by [`scrub_secrets`] on every write. That
//! sweep is a guard against an accident, not a filter to rely on: `command` and `note`
//! are prose and cannot be checked. What belongs in this file is which windows to open,
//! what they run, and what the work is.

use crate::persistence::paths;
use crate::workspace::io;
use crate::workspace::model::{PaneSpec, WorkspaceFile};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The per-repo directory this module owns, at the root of a checkout.
pub const PROJECT_DIR: &str = ".hyperpanes";
/// The file inside it.
pub const PROJECT_FILE: &str = "project.json";

/// How many ancestors the discovery walk inspects before giving up.
///
/// `Path::parent` already terminates at the filesystem root, so this is not what makes
/// the loop finite — it is what keeps a pathological argument (a path assembled from
/// thousands of `..`-free components, a deep symlink farm) from turning "open a folder"
/// into thousands of `stat` calls. 64 is far past any real checkout depth.
pub const MAX_WALK_DEPTH: usize = 64;

/// Which marker ended the ancestor walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootMarker {
    /// A `.hyperpanes/` directory — this repo describes its own windows.
    Hyperpanes,
    /// A `.git` entry with no `.hyperpanes/` beside it. The walk stops here rather than
    /// continuing: a repo nested inside another checkout must not adopt the outer
    /// checkout's windows. It is also where a first "save to repo" belongs.
    Git,
}

/// Where the walk stopped, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRoot {
    pub dir: PathBuf,
    pub marker: RootMarker,
}

impl ProjectRoot {
    /// The project file this root would hold, whether or not it exists.
    pub fn file_path(&self) -> PathBuf {
        project_file_path(&self.dir)
    }
}

/// A repo-local project file that was found and parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectFile {
    /// The checkout root — the directory *containing* `.hyperpanes/`.
    pub root: PathBuf,
    /// `<root>/.hyperpanes/project.json`.
    pub path: PathBuf,
    pub workspace: WorkspaceFile,
}

/// `<dir>/.hyperpanes/project.json`.
pub fn project_file_path(dir: &Path) -> PathBuf {
    dir.join(PROJECT_DIR).join(PROJECT_FILE)
}

/// Walk up from `start` to the first ancestor holding a `.hyperpanes/` directory or a
/// `.git` entry, whichever comes first.
///
/// Same shape as the git-root walk the sidebar does before recording a project — one
/// `parent()` chain, one existence probe per level — because two different answers to
/// "which directory is this pane in" is a bug generator. `.git` is probed with `exists`
/// rather than `is_dir` so a linked worktree (whose `.git` is a file) still counts.
pub fn find_project_root<P: AsRef<Path>>(start: P) -> Option<ProjectRoot> {
    let mut dir: Option<&Path> = Some(start.as_ref());
    let mut depth = 0usize;
    while let Some(d) = dir {
        if depth >= MAX_WALK_DEPTH {
            return None;
        }
        depth += 1;
        if d.join(PROJECT_DIR).is_dir() {
            return Some(ProjectRoot {
                dir: d.to_path_buf(),
                marker: RootMarker::Hyperpanes,
            });
        }
        if d.join(".git").exists() {
            return Some(ProjectRoot {
                dir: d.to_path_buf(),
                marker: RootMarker::Git,
            });
        }
        dir = d.parent();
    }
    None
}

/// The project file governing `start`, if one exists. `None` when the walk found no
/// marker, or found a `.git` root that has no project file — both mean "this checkout
/// does not describe its own windows yet".
pub fn find_project_file<P: AsRef<Path>>(start: P) -> Option<PathBuf> {
    let path = find_project_root(start)?.file_path();
    path.is_file().then_some(path)
}

/// Read the project file at a known checkout root. `Ok(None)` when there is none;
/// `Err` when one exists but cannot be read or parsed — a corrupt file is worth a
/// message, not a silent empty workspace (this is where it differs from
/// [`io::read_workspace`], which answers `None` to both).
///
/// Relative pane cwds resolve against `root`, **not** against the file's own directory:
/// a cwd in a repo-local file is written by a human thinking in repo-relative terms, and
/// `"src"` meaning `<root>/.hyperpanes/src` would be nonsense.
pub fn read_project_at<P: AsRef<Path>>(root: P) -> Result<Option<ProjectFile>, String> {
    let root = root.as_ref();
    let path = project_file_path(root);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let parsed = io::parse_workspace_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    let base = root.to_string_lossy().into_owned();
    Ok(Some(ProjectFile {
        root: root.to_path_buf(),
        path,
        workspace: io::resolve_cwds(&parsed, &base),
    }))
}

/// Walk up from an opened directory and read whatever project file governs it.
pub fn discover_project<P: AsRef<Path>>(start: P) -> Result<Option<ProjectFile>, String> {
    match find_project_root(start) {
        Some(root) if root.marker == RootMarker::Hyperpanes => read_project_at(&root.dir),
        _ => Ok(None),
    }
}

/// Write `workspace` to `<root>/.hyperpanes/project.json`, creating the directory,
/// scrubbing credential-shaped `meta` keys, and carrying forward any key a newer build
/// left behind. Returns the path written.
///
/// The write always routes through a `serde_json::Value`, even when there is nothing to
/// carry. `serde_json`'s map is a `BTreeMap` in this build (the frozen `Cargo.toml` does
/// not enable `preserve_order`), so that route sorts keys — and a file under version
/// control must not reshuffle itself depending on whether the previous writer happened
/// to know every field. One ordering, every save, is worth more here than matching the
/// declaration order `io::write_workspace` produces for app-support files.
pub fn write_project<P: AsRef<Path>>(
    root: P,
    workspace: &WorkspaceFile,
) -> Result<PathBuf, String> {
    let path = project_file_path(root.as_ref());
    let mut safe = workspace.clone();
    scrub_secrets(&mut safe);
    strip_uids(&mut safe);

    let mut doc = serde_json::to_value(io::WorkspaceEnvelope::wrap(safe))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(prior) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    {
        carry_prior(&mut doc, &prior);
    }
    let json =
        serde_json::to_string_pretty(&doc).map_err(|e| format!("{}: {e}", path.display()))?;
    paths::write_atomic(&path, json.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

// ===== precedence =====

/// Which of the two candidate layouts an open should use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A session is already running for this workspace; its layout is the truth.
    Live,
    /// No live session — the repo file is the fallback, and the seed on first open.
    Repo,
    /// Neither describes any panes; the caller opens its default window.
    Neither,
}

/// The precedence rule, entire.
///
/// A live session always wins, so re-opening a folder that is already on screen can
/// never silently replace the running layout with a months-old file. The repo file is
/// consulted only when there is nothing live, which is both the "cold start" case and
/// the "first ever open" case — the same answer serves both.
///
/// "Live" means live *and describing panes*: a session record that lists no panes at any
/// nesting level ([`io::has_panes`]) is an empty shell, and letting it outrank a real
/// repo file would make the feature look broken exactly when it matters.
pub fn resolve(live: Option<&WorkspaceFile>, repo: Option<&WorkspaceFile>) -> Source {
    if live.is_some_and(io::has_panes) {
        Source::Live
    } else if repo.is_some_and(io::has_panes) {
        Source::Repo
    } else {
        Source::Neither
    }
}

/// [`resolve`], already dereferenced — the layout to open, or `None`.
pub fn resolve_workspace<'a>(
    live: Option<&'a WorkspaceFile>,
    repo: Option<&'a WorkspaceFile>,
) -> Option<&'a WorkspaceFile> {
    match resolve(live, repo) {
        Source::Live => live,
        Source::Repo => repo,
        Source::Neither => None,
    }
}

// ===== secrets =====

/// Key tails that mark a `meta` entry as credential-shaped.
const SECRET_KEY_TAILS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "apikey",
    "key",
    "keys",
    "credential",
    "credentials",
    "auth",
];

/// Whether a `meta` key's *last* segment names a credential.
///
/// Only the last segment, deliberately. Matching anywhere in the key would eat
/// `ai.token_count`, and a guard that deletes ordinary data is a guard people work
/// around. `claude.api_token`, `api-key` and `session.secret` still go, while
/// `keychain` and `ai.token_count` stay; the rule is narrow enough to be silent.
fn is_secret_meta_key(key: &str) -> bool {
    key.to_ascii_lowercase()
        .rsplit(['.', '_', '-', ':', '/'])
        .find(|s| !s.is_empty())
        .is_some_and(|tail| SECRET_KEY_TAILS.contains(&tail))
}

/// Drop credential-shaped `meta` entries from every pane, returning how many went.
///
/// Called on every [`write_project`]. It is the enforceable half of "never a secret
/// store" — the unenforceable half being that nobody can tell whether a `command` or a
/// `note` embeds a key.
pub fn scrub_secrets(file: &mut WorkspaceFile) -> usize {
    let mut removed = 0usize;
    for_each_pane_mut(file, |pane| {
        if let Some(meta) = pane.meta.as_mut() {
            let before = meta.len();
            meta.retain(|k, _| !is_secret_meta_key(k));
            removed += before - meta.len();
            if meta.is_empty() {
                pane.meta = None;
            }
        }
    });
    removed
}

/// Drop every pane's live session uid.
///
/// A uid identifies a session on *this* machine in *this* run: it is meaningless in a
/// clone, it churns the diff of a version-controlled file on every save, and on the
/// in-process backend the ids are positional (`pane-3`), so a months-old file could
/// otherwise ask to re-attach to whatever `pane-3` happens to be running today. What
/// belongs here is which windows to open and what they run — a reader with no uid
/// spawns from the recorded command, which is the right answer in a checkout.
fn strip_uids(file: &mut WorkspaceFile) {
    for_each_pane_mut(file, |pane| pane.uid = None);
}

// ===== portability =====

/// Rewrite absolute pane cwds that live under `root` as paths relative to it, so the
/// file survives being cloned to a machine where the checkout sits elsewhere. Paths
/// outside `root` are left verbatim — they are deliberate references to somewhere else.
/// Separators are normalised to `/`, which every platform's reader accepts and which
/// keeps a Windows-authored file from churning the diff on macOS.
pub fn relativize_cwds(file: &WorkspaceFile, root: &Path) -> WorkspaceFile {
    let mut out = file.clone();
    for_each_pane_mut(&mut out, |pane| {
        if let Some(cwd) = pane.cwd.as_deref() {
            if let Ok(rel) = Path::new(cwd).strip_prefix(root) {
                let s = rel.to_string_lossy().replace('\\', "/");
                pane.cwd = Some(if s.is_empty() { ".".to_string() } else { s });
            }
        }
    });
    out
}

/// Visit every pane at all three nesting levels (top-level, per group, per window).
fn for_each_pane_mut(file: &mut WorkspaceFile, mut f: impl FnMut(&mut PaneSpec)) {
    if let Some(panes) = file.panes.as_mut() {
        panes.iter_mut().for_each(&mut f);
    }
    if let Some(groups) = file.groups.as_mut() {
        for g in groups.iter_mut() {
            g.panes.iter_mut().for_each(&mut f);
        }
    }
    if let Some(windows) = file.windows.as_mut() {
        for w in windows.iter_mut() {
            for g in w.groups.iter_mut() {
                g.panes.iter_mut().for_each(&mut f);
            }
        }
    }
}

// ===== forward compatibility =====
//
// The compat rule is "additive optional fields, no version bump", which means a build
// six months newer writes a project.json this build parses fine — minus the fields it
// has never heard of. Dropping them on the next save would make a shared repo a ratchet:
// whoever runs the older build wins, permanently. So the writer diffs against what is
// actually on disk and carries the strangers across.
//
// Only *unknown* keys are carried. A known key the new payload omits was unset by the
// user, not misunderstood by the parser, and resurrecting it would make a cleared label
// impossible to clear. That distinction is the whole reason these lists are spelled out
// by hand rather than inferred; `the_known_key_lists_cover_every_field_of_the_shared_model`
// fails the build if a field is added to the model without being added here.

const ENVELOPE_KEYS: &[&str] = &["format", "version", "workspace"];
const WORKSPACE_KEYS: &[&str] = &["name", "layout", "panes", "groups", "active", "windows"];
const WINDOW_KEYS: &[&str] = &["title", "active", "bounds", "groups"];
const GROUP_KEYS: &[&str] = &[
    "title",
    "layout",
    "panes",
    "sizes",
    "mainFraction",
    "focused",
    "zoomed",
];
const PANE_KEYS: &[&str] = &[
    "label", "color", "command", "args", "cwd", "shell", "fontSize", "meta", "uid", "talk", "note",
];
const BOUNDS_KEYS: &[&str] = &["x", "y", "width", "height", "maximized", "fullscreen"];

/// Copy `old`'s unrecognised keys into `new`, leaving anything `new` already says alone.
fn carry_unknown(new: &mut Value, old: &Value, known: &[&str]) {
    let (Some(old_obj), Some(new_obj)) = (old.as_object(), new.as_object_mut()) else {
        return;
    };
    for (k, v) in old_obj {
        if !known.contains(&k.as_str()) {
            new_obj.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
}

/// Carry unknowns across a child array, pairing elements by index.
///
/// Only when the lengths match. Index is the only correspondence available — panes carry
/// no identity that survives being closed and reopened — and pairing across a
/// pane that was added or removed would attach a stranger's data to the wrong pane, which
/// is worse than losing it. A layout edit therefore drops unknown keys; an ordinary save
/// keeps them.
fn carry_seq(new: &mut Value, old: &Value, key: &str, each: fn(&mut Value, &Value)) {
    let Some(old_items) = old.get(key).and_then(Value::as_array) else {
        return;
    };
    let Some(new_items) = new.get_mut(key).and_then(Value::as_array_mut) else {
        return;
    };
    if new_items.len() != old_items.len() {
        return;
    }
    for (n, o) in new_items.iter_mut().zip(old_items) {
        each(n, o);
    }
}

fn carry_pane(new: &mut Value, old: &Value) {
    carry_unknown(new, old, PANE_KEYS);
}

fn carry_group(new: &mut Value, old: &Value) {
    carry_unknown(new, old, GROUP_KEYS);
    carry_seq(new, old, "panes", carry_pane);
}

fn carry_window(new: &mut Value, old: &Value) {
    carry_unknown(new, old, WINDOW_KEYS);
    if let (Some(o), Some(n)) = (old.get("bounds"), new.get_mut("bounds")) {
        carry_unknown(n, o, BOUNDS_KEYS);
    }
    carry_seq(new, old, "groups", carry_group);
}

fn carry_workspace(new: &mut Value, old: &Value) {
    carry_unknown(new, old, WORKSPACE_KEYS);
    carry_seq(new, old, "panes", carry_pane);
    carry_seq(new, old, "groups", carry_group);
    carry_seq(new, old, "windows", carry_window);
}

/// Merge the strangers from the document currently on disk into the one about to
/// replace it. A bare legacy file (no `format` key) is the workspace payload itself, not
/// an envelope — treating it as one would hoist `name`/`panes` up beside `version`.
fn carry_prior(new: &mut Value, old: &Value) {
    if old.get("format").is_some() {
        carry_unknown(new, old, ENVELOPE_KEYS);
        if let (Some(o), Some(n)) = (old.get("workspace"), new.get_mut("workspace")) {
            carry_workspace(n, o);
        }
    } else if let Some(n) = new.get_mut("workspace") {
        carry_workspace(n, old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::model::{GroupSpec, WindowBounds, WindowSpec};
    use std::collections::BTreeMap;

    /// A scratch checkout root. `tempfile` is not a dependency of this crate (the frozen
    /// `Cargo.toml`), so this is the same pid-tagged temp dir `sets.rs` uses.
    fn temp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hp-project-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn with_panes() -> WorkspaceFile {
        WorkspaceFile {
            name: Some("tplx".into()),
            panes: Some(vec![PaneSpec {
                command: Some("claude".into()),
                note: Some("chasing the pty resize race".into()),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    fn read_raw(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    // --- discovery ---

    #[test]
    fn discovery_finds_the_hyperpanes_dir_from_a_nested_subdirectory() {
        let root = temp_root("nested");
        std::fs::create_dir_all(root.join(PROJECT_DIR)).unwrap();
        let deep = root.join("crates/core/src/workspace");
        std::fs::create_dir_all(&deep).unwrap();

        let found = find_project_root(&deep).expect("walk finds the root");
        assert_eq!(found.marker, RootMarker::Hyperpanes);
        assert_eq!(found.dir, root);
        assert_eq!(found.file_path(), project_file_path(&root));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A repo checked out inside another repo must not adopt the outer repo's windows:
    /// the walk stops at the inner `.git`, sees no `.hyperpanes/` there, and answers None.
    #[test]
    fn discovery_stops_at_a_git_root_with_no_hyperpanes_dir() {
        let outer = temp_root("stop-at-git");
        std::fs::create_dir_all(outer.join(PROJECT_DIR)).unwrap();
        write_project(&outer, &with_panes()).unwrap();

        let inner = outer.join("vendor/other-repo");
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        let deep = inner.join("src/deep");
        std::fs::create_dir_all(&deep).unwrap();

        let found = find_project_root(&deep).expect("walk stops somewhere");
        assert_eq!(found.marker, RootMarker::Git);
        assert_eq!(found.dir, inner);
        assert_eq!(find_project_file(&deep), None);
        assert_eq!(discover_project(&deep).unwrap(), None);

        // The outer repo's own file is still found from inside the outer repo.
        assert!(find_project_file(outer.join("src")).is_some());

        let _ = std::fs::remove_dir_all(&outer);
    }

    #[test]
    fn discovery_answers_none_when_nothing_describes_the_directory() {
        let root = temp_root("bare");
        let deep = root.join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(find_project_file(&deep), None);
        assert_eq!(discover_project(&deep).unwrap(), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The depth cap, not `parent()`, is what stops a pathological path. No filesystem
    /// needed: nothing along this chain exists, so the walk only ever runs out of budget.
    #[test]
    fn the_walk_is_bounded_rather_than_climbing_forever() {
        let mut p = PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" });
        for i in 0..(MAX_WALK_DEPTH * 4) {
            p.push(format!("seg{i}"));
        }
        assert_eq!(find_project_root(&p), None);
        assert_eq!(find_project_file(&p), None);
    }

    // --- round trip ---

    #[test]
    fn write_then_read_round_trips_the_note_creating_the_dir() {
        let root = temp_root("round-trip"); // no .hyperpanes/ yet
        let path = write_project(&root, &with_panes()).unwrap();
        assert_eq!(path, project_file_path(&root));

        let read = discover_project(root.join("src")).unwrap().expect("found");
        assert_eq!(read.root, root);
        assert_eq!(read.path, path);
        assert_eq!(read.workspace, with_panes());
        assert_eq!(
            read.workspace.panes.as_ref().unwrap()[0].note.as_deref(),
            Some("chasing the pty resize race")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The file on disk is the shared envelope — same magic, same version — so
    /// `io::parse_workspace_str` reads it with no project-specific knowledge at all.
    #[test]
    fn the_file_is_the_shared_workspace_envelope() {
        let root = temp_root("envelope");
        let path = write_project(&root, &with_panes()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();

        let doc = read_raw(&path);
        assert_eq!(doc["format"], Value::from(io::ENVELOPE_FORMAT));
        assert_eq!(doc["version"], Value::from(io::ENVELOPE_VERSION));
        assert_eq!(io::parse_workspace_str(&raw).unwrap(), with_panes());
        // A pane that set nothing else writes nothing else — no `null`s in a tracked file.
        assert!(!raw.contains("null"), "{raw}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A newer build adds optional fields without bumping the envelope version (that is
    /// the compat rule), so this build parses its file and must hand the strangers back
    /// untouched on the next save — otherwise a shared repo ratchets down to whoever runs
    /// the oldest binary.
    #[test]
    fn a_newer_builds_unknown_keys_survive_a_write_by_this_one() {
        let root = temp_root("forward-compat");
        let path = project_file_path(&root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{
  "format": "hyperpanes",
  "version": 1,
  "provenance": { "writtenBy": "9.9" },
  "workspace": {
    "name": "tplx",
    "mood": "focused",
    "panes": [
      { "command": "claude", "note": "old note", "vibe": "deep" }
    ]
  }
}"#,
        )
        .unwrap();

        let mut read = read_project_at(&root).unwrap().expect("found").workspace;
        assert_eq!(
            read.panes.as_ref().unwrap()[0].note.as_deref(),
            Some("old note")
        );

        read.panes.as_mut().unwrap()[0].note = Some("new note".into());
        read.name = None; // a known field the user cleared
        write_project(&root, &read).unwrap();

        let doc = read_raw(&path);
        assert_eq!(doc["provenance"]["writtenBy"], Value::from("9.9"));
        assert_eq!(doc["workspace"]["mood"], Value::from("focused"));
        assert_eq!(doc["workspace"]["panes"][0]["vibe"], Value::from("deep"));
        assert_eq!(
            doc["workspace"]["panes"][0]["note"],
            Value::from("new note")
        );
        // Cleared means cleared: a known key is never resurrected from the old file.
        assert!(doc["workspace"].get("name").is_none(), "{doc}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic_and_not_an_empty_workspace() {
        let root = temp_root("malformed");
        let path = project_file_path(&root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ \"format\": \"hyperpanes\", oops").unwrap();

        let err = discover_project(root.join("src")).unwrap_err();
        assert!(err.contains("invalid JSON"), "{err}");
        assert!(err.contains("project.json"), "{err}");

        // A well-formed file that isn't ours is an error too, not a blank layout.
        std::fs::write(&path, r#"{ "format": "vscode", "version": 1 }"#).unwrap();
        assert!(discover_project(&root)
            .unwrap_err()
            .contains("not a hyperpanes workspace"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Writing over a file this build understands completely must not disturb it.
    #[test]
    fn a_second_write_of_the_same_workspace_is_byte_identical() {
        let root = temp_root("stable");
        let path = write_project(&root, &with_panes()).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        write_project(&root, &with_panes()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);

        let _ = std::fs::remove_dir_all(&root);
    }

    // --- precedence ---

    #[test]
    fn a_live_session_always_outranks_the_repo_file() {
        let live = with_panes();
        let repo = WorkspaceFile {
            name: Some("from-repo".into()),
            ..with_panes()
        };
        assert_eq!(resolve(Some(&live), Some(&repo)), Source::Live);
        assert_eq!(resolve(Some(&live), None), Source::Live);
        assert_eq!(
            resolve_workspace(Some(&live), Some(&repo)).unwrap().name,
            live.name
        );
    }

    #[test]
    fn the_repo_file_is_the_fallback_and_the_first_open_seed() {
        let repo = with_panes();
        assert_eq!(resolve(None, Some(&repo)), Source::Repo);
        assert_eq!(resolve_workspace(None, Some(&repo)), Some(&repo));
    }

    #[test]
    fn a_contentless_candidate_is_not_a_layout_on_either_side() {
        let empty = WorkspaceFile::default();
        let full = with_panes();
        // An empty live record must not shadow a real repo file...
        assert_eq!(resolve(Some(&empty), Some(&full)), Source::Repo);
        // ...and an empty repo file must not shadow a real live session.
        assert_eq!(resolve(Some(&full), Some(&empty)), Source::Live);
        assert_eq!(resolve(Some(&empty), None), Source::Neither);
        assert_eq!(resolve(None, Some(&empty)), Source::Neither);
    }

    #[test]
    fn nothing_on_either_side_is_neither() {
        assert_eq!(resolve(None, None), Source::Neither);
        assert_eq!(resolve_workspace(None, None), None);
    }

    // --- secrets ---

    #[test]
    fn credential_shaped_meta_keys_never_reach_the_working_tree() {
        let root = temp_root("secrets");
        let mut meta = BTreeMap::new();
        meta.insert("pane.kind".to_string(), "tool:claude".to_string());
        meta.insert("ai.token_count".to_string(), "1200".to_string());
        meta.insert(
            "claude.api_token".to_string(),
            "sk-not-a-real-one".to_string(),
        );
        meta.insert("session.secret".to_string(), "hunter2".to_string());
        let ws = WorkspaceFile {
            panes: Some(vec![PaneSpec {
                command: Some("claude".into()),
                meta: Some(meta),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let path = write_project(&root, &ws).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("api_token"), "{raw}");
        assert!(!raw.contains("hunter2"), "{raw}");
        // The narrow rule keeps ordinary data: only the *last* segment is matched.
        assert!(raw.contains("ai.token_count"), "{raw}");
        assert!(raw.contains("pane.kind"), "{raw}");
        // The caller's own value is untouched — scrubbing is the writer's job.
        assert_eq!(
            ws.panes.as_ref().unwrap()[0].meta.as_ref().unwrap().len(),
            4
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A live session uid is a fact about this machine and this run. It must not travel
    /// in a version-controlled file: it means nothing in a clone, and on the in-process
    /// backend the ids are positional, so a months-old `pane-0` could ask to adopt an
    /// unrelated session that happens to hold that id today.
    #[test]
    fn a_live_session_uid_never_reaches_the_working_tree() {
        let root = temp_root("uid");
        let ws = WorkspaceFile {
            panes: Some(vec![PaneSpec {
                command: Some("claude".into()),
                uid: Some("pane-0".into()),
                ..Default::default()
            }]),
            groups: Some(vec![GroupSpec {
                panes: vec![PaneSpec {
                    command: Some("agent".into()),
                    uid: Some("sess-91f3".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            ..Default::default()
        };

        let path = write_project(&root, &ws).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("pane-0"), "{raw}");
        assert!(!raw.contains("sess-91f3"), "{raw}");
        // What the file is *for* survives.
        assert!(raw.contains("claude"), "{raw}");
        assert!(raw.contains("agent"), "{raw}");
        // The caller's own value is untouched — stripping is the writer's job.
        assert_eq!(ws.panes.as_ref().unwrap()[0].uid.as_deref(), Some("pane-0"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scrub_drops_an_emptied_meta_map_rather_than_writing_an_empty_object() {
        let mut meta = BTreeMap::new();
        meta.insert("gh.token".to_string(), "x".to_string());
        let mut ws = WorkspaceFile {
            panes: Some(vec![PaneSpec {
                meta: Some(meta),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(scrub_secrets(&mut ws), 1);
        assert_eq!(ws.panes.as_ref().unwrap()[0].meta, None);
    }

    #[test]
    fn the_secret_rule_matches_the_key_tail_and_only_the_tail() {
        for key in [
            "token",
            "gh.token",
            "api-key",
            "x_password",
            "a.b.credentials",
        ] {
            assert!(is_secret_meta_key(key), "{key} should be scrubbed");
        }
        for key in [
            "ai.token_count",
            "pane.kind",
            "authority",
            "role",
            "keychain",
        ] {
            assert!(!is_secret_meta_key(key), "{key} should be kept");
        }
    }

    // --- portability ---

    #[test]
    fn cwds_relativize_on_write_and_resolve_against_the_root_on_read() {
        let root = temp_root("portable");
        let ws = WorkspaceFile {
            panes: Some(vec![
                PaneSpec {
                    cwd: Some(root.join("crates/core").to_string_lossy().into_owned()),
                    ..Default::default()
                },
                PaneSpec {
                    cwd: Some(if cfg!(windows) {
                        r"C:\elsewhere".to_string()
                    } else {
                        "/elsewhere".to_string()
                    }),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        let portable = relativize_cwds(&ws, &root);
        let panes = portable.panes.as_ref().unwrap();
        assert_eq!(panes[0].cwd.as_deref(), Some("crates/core"));
        assert_eq!(panes[1].cwd, ws.panes.as_ref().unwrap()[1].cwd);

        write_project(&root, &portable).unwrap();
        let back = read_project_at(&root).unwrap().unwrap().workspace;
        // Read back, the relative cwd is absolute again — against the ROOT, not
        // against `.hyperpanes/`.
        assert_eq!(
            back.panes.as_ref().unwrap()[0].cwd.as_deref(),
            Some(root.join("crates/core").to_string_lossy().as_ref())
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // --- the guard on the hand-written key lists ---

    /// The forward-compat merge distinguishes "unknown" from "unset" using the lists
    /// above, so a field added to the shared model without being listed would start
    /// being resurrected after the user cleared it. This test is what catches that.
    #[test]
    fn the_known_key_lists_cover_every_field_of_the_shared_model() {
        let bounds = WindowBounds {
            x: Some(0),
            y: Some(0),
            width: Some(1),
            height: Some(1),
            maximized: Some(false),
            fullscreen: Some(true),
        };
        let pane = PaneSpec {
            label: Some("l".into()),
            color: Some("#fff".into()),
            command: Some("c".into()),
            args: Some(vec!["a".into()]),
            cwd: Some("/w".into()),
            shell: Some("sh".into()),
            font_size: Some(14),
            meta: Some(BTreeMap::new()),
            uid: Some("u".into()),
            talk: Some(true),
            note: Some("n".into()),
        };
        let group = GroupSpec {
            title: Some("t".into()),
            layout: Some("columns".into()),
            panes: vec![pane.clone()],
            sizes: Some(vec![1.0]),
            main_fraction: Some(0.5),
            focused: Some(0),
            zoomed: Some(0),
        };
        let window = WindowSpec {
            title: Some("w".into()),
            active: Some(0),
            bounds: Some(bounds.clone()),
            groups: vec![group.clone()],
        };
        let workspace = WorkspaceFile {
            name: Some("n".into()),
            layout: Some("columns".into()),
            panes: Some(vec![pane.clone()]),
            groups: Some(vec![group.clone()]),
            active: Some(0),
            windows: Some(vec![window.clone()]),
        };

        let cases: [(&str, Value, &[&str]); 6] = [
            (
                "envelope",
                serde_json::to_value(io::WorkspaceEnvelope::wrap(workspace.clone())).unwrap(),
                ENVELOPE_KEYS,
            ),
            (
                "workspace",
                serde_json::to_value(&workspace).unwrap(),
                WORKSPACE_KEYS,
            ),
            (
                "window",
                serde_json::to_value(&window).unwrap(),
                WINDOW_KEYS,
            ),
            ("group", serde_json::to_value(&group).unwrap(), GROUP_KEYS),
            ("pane", serde_json::to_value(&pane).unwrap(), PANE_KEYS),
            (
                "bounds",
                serde_json::to_value(&bounds).unwrap(),
                BOUNDS_KEYS,
            ),
        ];
        for (label, value, known) in cases {
            let actual: Vec<&str> = value
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            for k in &actual {
                assert!(known.contains(k), "{label}: `{k}` is missing from the list");
            }
            for k in known {
                assert!(actual.contains(k), "{label}: `{k}` is no longer a field");
            }
        }
    }
}
