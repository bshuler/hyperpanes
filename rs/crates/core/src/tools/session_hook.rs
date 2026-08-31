//! Per-tool session hooks: the *other* tools' answer to "which conversation is this pane
//! in", and the reader that turns their markers into a [`ToolSessionMark`].
//!
//! # Why this exists next to `claude_hook`
//!
//! `claude_hook` + `claude_panes` were written when Claude Code was the only CLI agent
//! with a lifecycle hook, so the mechanism and the tool were fused: one hard-coded
//! settings shape, one hard-coded marker directory, one hard-coded payload. Two more
//! tools turned out to have the same mechanism, verified against the installs on this
//! machine rather than inferred from documentation:
//!
//! * **cursor-agent** (2026.08.25-3e8eec8) — `~/.cursor/hooks.json`, shape
//!   `{"version": 1, "hooks": {"sessionStart": [{"command": "…"}]}}`. The sessionStart
//!   payload carries `conversation_id` and `workspace_roots`; the sessionEnd payload adds
//!   a `transcript_path` under `agent-transcripts/<conversation_id>/`, which is what
//!   proves `conversation_id` is the id the on-disk history — and therefore
//!   `--resume` — is keyed by.
//! * **GitHub Copilot CLI** (1.0.80) — `~/.copilot/settings.json`, shape
//!   `{"hooks": {"sessionStart": [{"command": "…"}]}}`, no `version` wrapper. The payload
//!   carries `sessionId` and a first-class `cwd`, and `sessionId` is exactly what the CLI
//!   itself prints as `copilot --resume=<id>`. The same block written into `config.json`
//!   also fires, but the CLI *migrates* it into `settings.json`, so `settings.json` is the
//!   file to own.
//!
//! Neither shape is Claude's (Claude nests a matcher group: `[{"hooks": [{"type":
//! "command", "command": "…"}]}]`), so this is a sibling of `claude_hook` rather than a
//! generalisation of it — sharing the *policy* (additive, idempotent, best-effort, never
//! create a config the tool itself has not created) and not the JSON.
//!
//! # Why a hook beats every other signal
//!
//! It is the only signal that is not an inference. The tool names its own conversation id,
//! in the tool's own words, in a process whose environment carries `HYPERPANES_PANE_ID` —
//! so the pane→conversation binding is a fact rather than a correlation. That is why a
//! hook-written mark outranks the scan-and-diff inference in
//! [`crate::tools::session_infer`], and why that module exists only for tools with no hook.
//!
//! Everything here is best-effort: a missing bundled script, a tool that is not installed,
//! an unreadable settings file, a malformed marker — each is a silent no-op that leaves
//! the pane exactly as well off as it was before.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::claude_panes::{valid_resume_cwd, valid_session_id};
use crate::persistence::paths::{state_dir, write_atomic};
use crate::tools::session_mark::ToolSessionMark;

/// One tool whose lifecycle hook we have verified against a real install.
pub struct HookedTool {
    /// Registry id ([`crate::tools::registry`]) — also the marker sub-directory name.
    pub id: &'static str,
    /// `resources/<dir>/<script>` in the shipped tree.
    dir: &'static str,
    script: &'static str,
    /// The settings file to merge into, relative to the user's home.
    settings_rel: &'static [&'static str],
    /// Whether the file's top level wants `"version": 1` (cursor does, copilot does not).
    versioned: bool,
}

/// The tools with a verified hook. Claude is deliberately absent: it has one, but
/// `claude_hook` owns it — its settings shape, its marker directory, and its
/// multi-account `CLAUDE_CONFIG_DIR` fan-out are all different, and folding them in here
/// would mean rewriting a path that already works.
pub static HOOKED_TOOLS: &[HookedTool] = &[
    HookedTool {
        id: "cursor-agent",
        dir: "cursor",
        script: "hp-cursor-session-hook.sh",
        settings_rel: &[".cursor", "hooks.json"],
        versioned: true,
    },
    HookedTool {
        id: "copilot",
        dir: "copilot",
        script: "hp-copilot-session-hook.sh",
        settings_rel: &[".copilot", "settings.json"],
        versioned: false,
    },
];

/// Where one tool's per-pane markers live: `<state>/tool-sessions/<tool-id>/<pane-id>.json`.
///
/// Runtime state rather than data, for the same reason as
/// [`crate::persistence::paths::claude_sessions_dir`]: a marker describes a pane that is
/// alive right now, so it must not survive into backups or dotfile syncs as if it were
/// durable. Namespaced per tool because a pane can run one agent, exit it, and run another
/// — un-namespaced, the second tool's marker would silently answer for the first.
///
/// The two shipped hook scripts recompute this path in `sh`; the two must move together.
pub fn marker_dir(tool_id: &str) -> PathBuf {
    state_dir().join("tool-sessions").join(tool_id)
}

/// One pane's live conversation, as reported by a tool's session hook. Camel-cased on
/// disk because every one of these payloads is written by a script reading the tool's own
/// camel-cased JSON, and matching it keeps the scripts free of a rename step.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookMarker {
    session_id: String,
    #[serde(default)]
    cwd: String,
}

/// The mark `tool_id`'s hook has recorded for `pane_id`, if any.
///
/// The mark is stamped with the tool because the pane this answers for is, by definition,
/// one nobody spawned as a tool: a pane Hyperpanes launched with `cursor-agent` already
/// carries `PaneKind::Tool`, whereas the pane a hook rescues is the one where the human
/// typed the tool's name into a shell and the persisted kind stayed `Terminal`.
///
/// Never trusted blindly — the same gates `claude_panes` applies. The file is written by
/// an external script, and the id it carries ends up on a command line.
pub fn read_pane_mark(tool_id: &str, pane_id: &str) -> Option<ToolSessionMark> {
    // A pane id is a path component here; a hostile one must not be able to climb out of
    // the marker directory. Real ids are uid/alias strings, so refusing is free.
    if pane_id.is_empty() || pane_id.contains(['/', '\\']) || pane_id.contains("..") {
        return None;
    }
    let path = marker_dir(tool_id).join(format!("{pane_id}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    let parsed: HookMarker = serde_json::from_str(&text).ok()?;
    if !valid_session_id(&parsed.session_id) || !valid_resume_cwd(&parsed.cwd) {
        return None;
    }
    Some(ToolSessionMark::new(&parsed.session_id, &parsed.cwd)?.with_tool(tool_id))
}

/// The mark ANY hooked tool has recorded for `pane_id`.
///
/// A pane runs one agent at a time, so at most one marker is normally live. When more than
/// one is — the human ran two agents in the same pane in turn and one hook's `sessionEnd`
/// never fired — the first hit in [`HOOKED_TOOLS`] order wins. That is a stable, boring
/// answer to a situation with no right one, and it costs nothing: adoption only ever fills
/// an EMPTY mark, so whichever tool got there first has already been recorded anyway.
pub fn read_any_pane_mark(pane_id: &str) -> Option<ToolSessionMark> {
    HOOKED_TOOLS
        .iter()
        .find_map(|t| read_pane_mark(t.id, pane_id))
}

/// Resolve a bundled hook script, mirroring the packaged layouts
/// [`crate::claude_hook::bundled_hook_path`] handles: next to the exe, the macOS `.app`
/// `Contents/Resources`, and the FHS `share`/`lib` install prefixes.
fn bundled_script(dir: &str, script: &str) -> Option<PathBuf> {
    let rel = Path::new("resources").join(dir).join(script);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))?;
    let mut candidates = vec![exe_dir.join(&rel)];
    if let Some(prefix) = exe_dir.parent() {
        candidates.push(prefix.join("Resources").join(dir).join(script));
        candidates.push(prefix.join("share").join("hyperpanes").join(&rel));
        candidates.push(prefix.join("lib").join("hyperpanes").join(&rel));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// The user's home, for locating `~/.cursor` and `~/.copilot`.
fn home_dir() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(v));
    }
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

/// Register every bundled hook script in its tool's settings file. Returns the number of
/// files newly modified (0 = already registered / no such tool installed / nothing
/// bundled). Best-effort: per-file errors are logged and never fatal.
pub fn ensure_registered() -> usize {
    let Some(home) = home_dir() else {
        return 0;
    };
    let mut changed = 0;
    for tool in HOOKED_TOOLS {
        let Some(script) = bundled_script(tool.dir, tool.script) else {
            continue; // not shipped in this build's layout — nothing to register
        };
        let file = tool.settings_rel.iter().fold(home.clone(), |p, s| p.join(s));
        let cmd = script.to_string_lossy().to_string();
        match ensure_in_file(&file, &cmd, tool.versioned) {
            Ok(true) => {
                eprintln!(
                    "[{}-hook] registered sessionStart/End in {}",
                    tool.id,
                    file.display()
                );
                changed += 1;
            }
            Ok(false) => {}
            Err(e) => eprintln!("[{}-hook] {}: {e}", tool.id, file.display()),
        }
    }
    changed
}

/// Merge the hook command into one settings file's `sessionStart` + `sessionEnd`. Returns
/// whether the file was written.
///
/// The parent-directory gate is the same one `claude_hook` applies, and for the same
/// reason: a `~/.cursor` that does not exist means cursor-agent is not installed (or has
/// never run), and conjuring a config for a tool the human does not use is not this
/// program's business.
fn ensure_in_file(file: &Path, cmd: &str, versioned: bool) -> Result<bool, String> {
    match file.parent() {
        Some(p) if p.is_dir() => {}
        _ => return Ok(false), // no such config dir — skip
    }
    let mut root: Value = match std::fs::read_to_string(file) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).map_err(|e| e.to_string())?,
        _ => json!({}),
    };
    if !root.is_object() {
        return Err("settings file is not a JSON object".into());
    }
    // `|` not `||`: both events must be attempted, and short-circuiting on the first
    // would leave a file with a start hook and no end hook — markers that never clear.
    let modified = ["sessionStart", "sessionEnd"]
        .iter()
        .fold(false, |acc, ev| ensure_event(&mut root, ev, cmd) | acc);
    // The version stamp is added only alongside a hook we are adding, and only when the
    // file does not already declare one: a file we are not otherwise changing is left
    // exactly as found, version included — that field is the tool's business, not ours.
    if modified && versioned && root.get("version").is_none() {
        root["version"] = json!(1);
    }
    if modified {
        let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
        write_atomic(file, pretty.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(modified)
}

/// Ensure `hooks.<event>` contains an entry whose `command == cmd`. Returns whether it
/// added one (idempotent: a no-op if already present; leaves a malformed shape untouched,
/// returning false rather than repairing a file the human may have meant).
fn ensure_event(root: &mut Value, event: &str, cmd: &str) -> bool {
    let Some(obj) = root.as_object_mut() else {
        return false;
    };
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks_obj) = hooks.as_object_mut() else {
        return false;
    };
    let arr = hooks_obj.entry(event).or_insert_with(|| json!([]));
    let Some(entries) = arr.as_array_mut() else {
        return false;
    };
    if entries
        .iter()
        .any(|e| e.get("command").and_then(Value::as_str) == Some(cmd))
    {
        return false;
    }
    entries.push(json!({ "command": cmd }));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_a_flat_command_entry_not_claudes_matcher_group() {
        // The whole reason this is not a call into `claude_hook`: cursor and copilot take
        // `[{"command": …}]`, Claude takes `[{"hooks": [{"type": "command", …}]}]`.
        let mut root = json!({});
        assert!(ensure_event(&mut root, "sessionStart", "/hook.sh"));
        assert_eq!(root["hooks"]["sessionStart"][0]["command"], "/hook.sh");
        assert!(root["hooks"]["sessionStart"][0].get("hooks").is_none());
    }

    #[test]
    fn is_idempotent_and_preserves_other_settings() {
        let mut root = json!({
            "model": "auto",
            "hooks": { "sessionStart": [ { "command": "/hook.sh" } ] }
        });
        assert!(!ensure_event(&mut root, "sessionStart", "/hook.sh"));
        assert_eq!(root["model"], "auto");
        // Somebody else's hook is appended beside, never replaced.
        assert!(ensure_event(&mut root, "sessionStart", "/theirs.sh"));
        assert_eq!(root["hooks"]["sessionStart"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn leaves_a_malformed_hooks_block_untouched() {
        let mut root = json!({ "hooks": { "sessionStart": "not-an-array" } });
        assert!(!ensure_event(&mut root, "sessionStart", "/hook.sh"));
        assert_eq!(root["hooks"]["sessionStart"], "not-an-array");
    }

    #[test]
    fn a_versioned_file_gets_cursors_version_stamp_and_an_unversioned_one_does_not() {
        let dir = std::env::temp_dir().join(format!("hp-hooked-ver-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let cursor = dir.join("hooks.json");
        assert_eq!(ensure_in_file(&cursor, "/hook.sh", true), Ok(true));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cursor).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["hooks"]["sessionStart"][0]["command"], "/hook.sh");
        assert_eq!(v["hooks"]["sessionEnd"][0]["command"], "/hook.sh");

        let copilot = dir.join("settings.json");
        assert_eq!(ensure_in_file(&copilot, "/hook.sh", false), Ok(true));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&copilot).unwrap()).unwrap();
        assert!(
            v.get("version").is_none(),
            "copilot's settings.json has no version wrapper at this level"
        );
        assert_eq!(v["hooks"]["sessionEnd"][0]["command"], "/hook.sh");

        // Second pass changes nothing — this runs on every app start.
        assert_eq!(ensure_in_file(&cursor, "/hook.sh", true), Ok(false));
        assert_eq!(ensure_in_file(&copilot, "/hook.sh", false), Ok(false));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tool_that_was_never_installed_gets_no_config_conjured_for_it() {
        let file = std::env::temp_dir()
            .join("hp-hooked-nope-xyz")
            .join("hooks.json");
        assert_eq!(ensure_in_file(&file, "/hook.sh", true), Ok(false));
        assert!(!file.exists());
    }

    #[test]
    fn every_hooked_tool_names_a_real_registry_entry() {
        // The id is the marker directory name AND what gets stamped into the mark, where
        // it is resolved back through the registry — an id that resolves to nothing would
        // make every marker this module writes unusable.
        for t in HOOKED_TOOLS {
            assert!(
                crate::tools::registry::by_id(t.id).is_some(),
                "{} is not a registry id",
                t.id
            );
        }
    }

    #[test]
    fn a_marker_becomes_a_mark_stamped_with_its_tool() {
        // The pane this rescues is a plain terminal as far as its persisted kind knows, so
        // the tool has to travel with the id or the conversation cannot be re-entered.
        let pane = format!("hp-test-pane-{}", std::process::id());
        let dir = marker_dir("cursor-agent");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{pane}.json"));
        std::fs::write(
            &path,
            r#"{"sessionId":"e91284c5-9826-4ae9-839c-f96b1ac7fbe7","cwd":"/tmp/proj"}"#,
        )
        .unwrap();

        let mark = read_pane_mark("cursor-agent", &pane).expect("a well-formed marker reads back");
        assert_eq!(mark.id, "e91284c5-9826-4ae9-839c-f96b1ac7fbe7");
        assert_eq!(mark.cwd, "/tmp/proj");
        assert_eq!(mark.tool.as_deref(), Some("cursor-agent"));
        assert_eq!(read_any_pane_mark(&pane), Some(mark));

        // Half a marker is no marker: resume is directory-scoped for all of them, so an id
        // without a directory resumes nothing.
        std::fs::write(&path, r#"{"sessionId":"e91284c5-9826-4ae9-839c-f96b1ac7fbe7"}"#).unwrap();
        assert_eq!(read_pane_mark("cursor-agent", &pane), None);

        // And a hostile id never reaches the command line it would land on.
        std::fs::write(&path, r#"{"sessionId":"x; rm -rf /","cwd":"/tmp/proj"}"#).unwrap();
        assert_eq!(read_pane_mark("cursor-agent", &pane), None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_pane_id_cannot_climb_out_of_the_marker_directory() {
        // Pane ids come from the control host, which is a network-facing surface; the id
        // is used as a path component, so a traversal attempt is refused before any I/O.
        assert_eq!(read_pane_mark("cursor-agent", "../../etc/passwd"), None);
        assert_eq!(read_pane_mark("cursor-agent", ".."), None);
        assert_eq!(read_pane_mark("cursor-agent", ""), None);
    }
}
