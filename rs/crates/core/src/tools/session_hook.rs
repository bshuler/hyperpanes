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
//! * **Codex CLI** (0.151.0) — `$CODEX_HOME/hooks.json`, and it is Claude's shape rather
//!   than the other two's: PascalCase event names and a nested matcher group,
//!   `{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "…"}]}]}}`.
//!   The payload is Claude's too — `session_id`, `cwd`, `hook_event_name`, and a
//!   first-class `transcript_path` naming the rollout JSONL. A **project-level**
//!   `<cwd>/.codex/hooks.json` is not read, and `[hooks.*]` tables in `config.toml` are
//!   silently ignored (codex accepts unknown config keys without complaint, which is why
//!   the working shape had to be found by a matrix test rather than by reading one).
//!
//!   Codex additionally **trust-gates** hooks: a `hooks.json` it has not been told to
//!   trust does not fire, and the trust is persisted as `[hooks.state]` in `config.toml`
//!   over a normalized hook identity rather than a plain digest of the file. That is a
//!   security control, so the file is written and the human approves it inside codex once
//!   — nothing here forges a trust record.
//!
//! * **Gemini CLI** (0.58.0) — `~/.gemini/settings.json` under a top-level `"hooks"` key,
//!   in codex's (and therefore Claude's) nested PascalCase shape; gemini ships a
//!   `gemini hooks migrate` that imports Claude Code's config, which is the corroboration
//!   for that. The payload is Claude's as well. Two things differ from codex: gemini does
//!   **not** trust-gate hooks — writing the file is enough — and its config override
//!   `GEMINI_CLI_HOME` names the *home* directory rather than the config directory
//!   `CODEX_HOME` names, which is the distinction [`RootEnv`] carries.
//!
//! Cursor's and Copilot's shapes are not Claude's (Claude nests a matcher group:
//! `[{"hooks": [{"type": "command", "command": "…"}]}]`), so this is a sibling of
//! `claude_hook` rather than a generalisation of it — sharing the *policy* (additive,
//! idempotent, best-effort, never create a config the tool itself has not created) and not
//! the JSON. Codex, arriving later, happens to share Claude's JSON but none of its
//! multi-account fan-out, so it lives here with a shape flag rather than there.
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
//!
//! # Windows
//!
//! The five POSIX hooks are `/bin/sh` wrappers around `python3`, which is neither present
//! nor invocable that way on a default Windows install — and their state directory is
//! computed with `uname`, which under a Git-Bash-ish shell resolves to the XDG path rather
//! than `%APPDATA%\hyperpanes`, so shipping them there would write markers nowhere the
//! reader looks. Windows therefore gets one PowerShell script,
//! `resources/hooks/hp-session-hook.ps1`, told which tool it is running for on the command
//! line — see [`windows_hook_command`]. The five payload shapes are small enough to be a
//! switch, and the part that is easy to get wrong (BOM-less UTF-8, an atomic replace) is
//! identical for all of them.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::claude_panes::{valid_resume_cwd, valid_session_id};
use crate::persistence::paths::{state_dir, write_atomic};
use crate::tools::session_mark::ToolSessionMark;

/// How a tool spells one registered hook inside its `hooks.<event>` array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookShape {
    /// cursor-agent and Copilot CLI: `{"command": "…"}`.
    Flat,
    /// Codex CLI (and Claude Code): a matcher group wrapping the command,
    /// `{"hooks": [{"type": "command", "command": "…"}]}`.
    Nested,
}

/// What a tool's config-location environment variable actually names.
///
/// The distinction is not pedantry: `CODEX_HOME` and `GEMINI_CLI_HOME` read alike and mean
/// different things, and treating either as the other writes the hook into a file the tool
/// never opens — which fails silently, because a hook that is not registered simply never
/// fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootEnv {
    /// Names the config **directory** itself; the settings file's own name is joined onto
    /// it. Codex's `CODEX_HOME` — `$CODEX_HOME/hooks.json`.
    ConfigDir(&'static str),
    /// Names the **home** directory; the whole home-relative path is joined onto it.
    /// Gemini's `GEMINI_CLI_HOME` — `$GEMINI_CLI_HOME/.gemini/settings.json`. (Gemini's
    /// bundle also exports a `GEMINI_DIR`, which is a compile-time constant holding the
    /// string `.gemini` and not an environment variable at all.)
    Home(&'static str),
}

/// One tool whose lifecycle hook we have verified against a real install.
pub struct HookedTool {
    /// Registry id ([`crate::tools::registry`]) — also the marker sub-directory name.
    pub id: &'static str,
    /// `resources/<dir>/<script>` in the shipped tree.
    dir: &'static str,
    script: &'static str,
    /// The settings file to merge into, relative to the user's home. Rebased by
    /// `root_env` when that tool's own override is set.
    settings_rel: &'static [&'static str],
    /// The tool's OWN environment override for where its config lives, if it has one.
    root_env: Option<RootEnv>,
    /// The two lifecycle events, start first. Spelled differently per tool — cursor and
    /// copilot use `sessionStart`/`sessionEnd`, codex uses `SessionStart`/`SessionEnd` —
    /// and an event under the wrong casing is silently never called.
    events: [&'static str; 2],
    /// How one entry in an event array is spelled.
    shape: HookShape,
    /// Whether the file's top level wants `"version": 1` (cursor does; copilot and codex
    /// do not).
    versioned: bool,
}

impl HookedTool {
    /// Where this tool's hook settings live under `home`.
    ///
    /// `root_env` wins when it is set and non-empty: it is the tool's OWN override, so a
    /// human who moved `$CODEX_HOME` has moved the file codex reads — writing the
    /// home-relative path instead would register a hook nothing ever loads.
    fn settings_file(&self, home: &Path) -> PathBuf {
        let rel = |base: PathBuf| self.settings_rel.iter().fold(base, |p, s| p.join(s));
        match self.root_env {
            Some(RootEnv::ConfigDir(var)) => {
                if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
                    let file = self.settings_rel.last().copied().unwrap_or("hooks.json");
                    return PathBuf::from(v).join(file);
                }
            }
            Some(RootEnv::Home(var)) => {
                if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
                    return rel(PathBuf::from(v));
                }
            }
            None => {}
        }
        rel(home.to_path_buf())
    }
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
        root_env: None,
        events: ["sessionStart", "sessionEnd"],
        shape: HookShape::Flat,
        versioned: true,
    },
    HookedTool {
        id: "copilot",
        dir: "copilot",
        script: "hp-copilot-session-hook.sh",
        settings_rel: &[".copilot", "settings.json"],
        root_env: None,
        events: ["sessionStart", "sessionEnd"],
        shape: HookShape::Flat,
        versioned: false,
    },
    HookedTool {
        id: "codex",
        dir: "codex",
        script: "hp-codex-session-hook.sh",
        settings_rel: &[".codex", "hooks.json"],
        root_env: Some(RootEnv::ConfigDir("CODEX_HOME")),
        events: ["SessionStart", "SessionEnd"],
        shape: HookShape::Nested,
        versioned: false,
    },
    // Gemini takes the same nested, PascalCase shape as codex — it ships a
    // `gemini hooks migrate` that imports Claude Code's config, which is why. Unlike codex
    // it does NOT trust-gate hooks: writing the file is enough, nothing has to be approved.
    HookedTool {
        id: "gemini",
        dir: "gemini",
        script: "hp-gemini-session-hook.sh",
        settings_rel: &[".gemini", "settings.json"],
        root_env: Some(RootEnv::Home("GEMINI_CLI_HOME")),
        events: ["SessionStart", "SessionEnd"],
        shape: HookShape::Nested,
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
/// The shipped hook scripts recompute this path in `sh`; the two must move together.
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

/// The Windows hook script, which stands in for all five `.sh` ones. See the module's
/// **Windows** section for why it is one file rather than five.
const WINDOWS_HOOK: (&str, &str) = ("hooks", "hp-session-hook.ps1");

/// The command string to register for `tool`, or `None` when this build's layout ships no
/// script for it (a dev build run straight out of `target/`, say).
fn hook_command(tool: &HookedTool) -> Option<String> {
    // `cfg!` rather than `#[cfg]` so both arms compile everywhere: the Windows arm is then
    // type-checked on the machines this is actually developed on, and `HookedTool::script`
    // does not read as dead on Windows.
    if cfg!(windows) {
        let script = bundled_script(WINDOWS_HOOK.0, WINDOWS_HOOK.1)?;
        Some(windows_hook_command(&script, tool.id))
    } else {
        Some(
            bundled_script(tool.dir, tool.script)?
                .to_string_lossy()
                .to_string(),
        )
    }
}

/// How Windows spells "run this hook for this tool".
///
/// `-ExecutionPolicy Bypass` is not optional: the default policy on a fresh Windows install
/// is `Restricted`, under which the script is refused before its first line — and a hook
/// that fails to start fails *silently*, since no tool surfaces a hook's exit status. The
/// path is quoted because an install under `C:\Program Files\…` contains a space, and
/// `-NoProfile` so that a user profile cannot slow down or break something that runs at
/// every session start.
fn windows_hook_command(script: &Path, tool_id: &str) -> String {
    format!(
        "powershell -NoProfile -ExecutionPolicy Bypass -File \"{}\" -Tool {tool_id}",
        script.display()
    )
}

/// The user's home, for locating `~/.cursor`, `~/.copilot`, `~/.codex` and `~/.gemini`.
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
        let Some(cmd) = hook_command(tool) else {
            continue; // not shipped in this build's layout — nothing to register
        };
        let file = tool.settings_file(&home);
        match ensure_in_file(&file, &cmd, tool) {
            Ok(true) => {
                eprintln!(
                    "[{}-hook] registered {}/{} in {}",
                    tool.id,
                    tool.events[0],
                    tool.events[1],
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

/// Merge the hook command into one settings file's two lifecycle events, under the spelling
/// and shape that tool takes. Returns whether the file was written.
///
/// The parent-directory gate is the same one `claude_hook` applies, and for the same
/// reason: a `~/.cursor` that does not exist means cursor-agent is not installed (or has
/// never run), and conjuring a config for a tool the human does not use is not this
/// program's business.
fn ensure_in_file(file: &Path, cmd: &str, tool: &HookedTool) -> Result<bool, String> {
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
    let modified = tool.events.iter().fold(false, |acc, ev| {
        ensure_event(&mut root, ev, cmd, tool.shape) | acc
    });
    // The version stamp is added only alongside a hook we are adding, and only when the
    // file does not already declare one: a file we are not otherwise changing is left
    // exactly as found, version included — that field is the tool's business, not ours.
    if modified && tool.versioned && root.get("version").is_none() {
        root["version"] = json!(1);
    }
    if modified {
        let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
        write_atomic(file, pretty.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(modified)
}

/// Ensure `hooks.<event>` contains an entry running `cmd`, spelled `shape`'s way. Returns
/// whether it added one (idempotent: a no-op if already present; leaves a malformed shape
/// untouched, returning false rather than repairing a file the human may have meant).
fn ensure_event(root: &mut Value, event: &str, cmd: &str, shape: HookShape) -> bool {
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
    if entries.iter().any(|e| entry_runs(e, cmd)) {
        return false;
    }
    entries.push(match shape {
        HookShape::Flat => json!({ "command": cmd }),
        HookShape::Nested => json!({ "hooks": [ { "type": "command", "command": cmd } ] }),
    });
    true
}

/// Whether one already-present entry runs `cmd`, in either spelling.
///
/// Both are checked regardless of the tool's own shape so that a file already carrying the
/// hook in the *other* form is left alone rather than gaining a duplicate — the cost of
/// being wrong here is a tool that calls the same script twice per session, and the check
/// is free.
fn entry_runs(entry: &Value, cmd: &str) -> bool {
    if entry.get("command").and_then(Value::as_str) == Some(cmd) {
        return true;
    }
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|inner| {
            inner
                .iter()
                .any(|h| h.get("command").and_then(Value::as_str) == Some(cmd))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tool row for `id`, for a test that needs a real shape rather than a made-up one.
    fn tool(id: &str) -> &'static HookedTool {
        HOOKED_TOOLS.iter().find(|t| t.id == id).unwrap()
    }

    #[test]
    fn adds_a_flat_command_entry_for_the_tools_that_take_one() {
        // The original reason this is not a call into `claude_hook`: cursor and copilot
        // take `[{"command": …}]` where Claude takes a matcher group.
        let mut root = json!({});
        assert!(ensure_event(
            &mut root,
            "sessionStart",
            "/hook.sh",
            HookShape::Flat
        ));
        assert_eq!(root["hooks"]["sessionStart"][0]["command"], "/hook.sh");
        assert!(root["hooks"]["sessionStart"][0].get("hooks").is_none());
    }

    #[test]
    fn adds_codexs_nested_matcher_group_under_its_pascal_case_event() {
        // Codex takes Claude's JSON, and the event name is `SessionStart`, not
        // `sessionStart` — an event under the wrong casing is silently never called, which
        // is exactly the failure this shape flag exists to prevent.
        let mut root = json!({});
        assert!(ensure_event(
            &mut root,
            "SessionStart",
            "/hook.sh",
            HookShape::Nested
        ));
        let entry = &root["hooks"]["SessionStart"][0];
        assert_eq!(entry["hooks"][0]["type"], "command");
        assert_eq!(entry["hooks"][0]["command"], "/hook.sh");
        assert!(entry.get("command").is_none());
        assert!(root["hooks"].get("sessionStart").is_none());
    }

    #[test]
    fn is_idempotent_and_preserves_other_settings() {
        let mut root = json!({
            "model": "auto",
            "hooks": { "sessionStart": [ { "command": "/hook.sh" } ] }
        });
        assert!(!ensure_event(
            &mut root,
            "sessionStart",
            "/hook.sh",
            HookShape::Flat
        ));
        assert_eq!(root["model"], "auto");
        // Somebody else's hook is appended beside, never replaced.
        assert!(ensure_event(
            &mut root,
            "sessionStart",
            "/theirs.sh",
            HookShape::Flat
        ));
        assert_eq!(root["hooks"]["sessionStart"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_hook_already_present_in_the_other_spelling_is_not_duplicated() {
        // A human (or an older build) may have registered the same script the other way
        // round. Adding a second entry would make the tool run it twice per session.
        let mut root = json!({
            "hooks": { "SessionStart": [ { "hooks": [ { "type": "command", "command": "/hook.sh" } ] } ] }
        });
        assert!(!ensure_event(
            &mut root,
            "SessionStart",
            "/hook.sh",
            HookShape::Flat
        ));
        let mut root = json!({ "hooks": { "SessionStart": [ { "command": "/hook.sh" } ] } });
        assert!(!ensure_event(
            &mut root,
            "SessionStart",
            "/hook.sh",
            HookShape::Nested
        ));
    }

    #[test]
    fn leaves_a_malformed_hooks_block_untouched() {
        let mut root = json!({ "hooks": { "sessionStart": "not-an-array" } });
        assert!(!ensure_event(
            &mut root,
            "sessionStart",
            "/hook.sh",
            HookShape::Flat
        ));
        assert_eq!(root["hooks"]["sessionStart"], "not-an-array");
    }

    #[test]
    fn a_versioned_file_gets_cursors_version_stamp_and_an_unversioned_one_does_not() {
        let dir = std::env::temp_dir().join(format!("hp-hooked-ver-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let cursor = dir.join("hooks.json");
        assert_eq!(
            ensure_in_file(&cursor, "/hook.sh", tool("cursor-agent")),
            Ok(true)
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cursor).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["hooks"]["sessionStart"][0]["command"], "/hook.sh");
        assert_eq!(v["hooks"]["sessionEnd"][0]["command"], "/hook.sh");

        let copilot = dir.join("settings.json");
        assert_eq!(
            ensure_in_file(&copilot, "/hook.sh", tool("copilot")),
            Ok(true)
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&copilot).unwrap()).unwrap();
        assert!(
            v.get("version").is_none(),
            "copilot's settings.json has no version wrapper at this level"
        );
        assert_eq!(v["hooks"]["sessionEnd"][0]["command"], "/hook.sh");

        let codex = dir.join("codex-hooks.json");
        assert_eq!(ensure_in_file(&codex, "/hook.sh", tool("codex")), Ok(true));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&codex).unwrap()).unwrap();
        assert!(v.get("version").is_none());
        assert_eq!(
            v["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
            "/hook.sh"
        );

        // Second pass changes nothing — this runs on every app start.
        assert_eq!(
            ensure_in_file(&cursor, "/hook.sh", tool("cursor-agent")),
            Ok(false)
        );
        assert_eq!(
            ensure_in_file(&copilot, "/hook.sh", tool("copilot")),
            Ok(false)
        );
        assert_eq!(ensure_in_file(&codex, "/hook.sh", tool("codex")), Ok(false));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tool_that_was_never_installed_gets_no_config_conjured_for_it() {
        let file = std::env::temp_dir()
            .join("hp-hooked-nope-xyz")
            .join("hooks.json");
        assert_eq!(
            ensure_in_file(&file, "/hook.sh", tool("cursor-agent")),
            Ok(false)
        );
        assert!(!file.exists());
    }

    #[test]
    fn a_config_dir_override_and_a_home_override_land_in_different_places() {
        // `CODEX_HOME` and `GEMINI_CLI_HOME` read alike and are not alike: codex's names the
        // config directory (`$CODEX_HOME/hooks.json`) and gemini's names the home
        // (`$GEMINI_CLI_HOME/.gemini/settings.json`). Confusing them writes a valid hook
        // into a file the tool never opens, and an unregistered hook does not fail — it
        // just never fires, which is the hardest kind of wrong to notice.
        let home = Path::new("/home/someone");
        let over = std::env::temp_dir().join("hp-root-env-probe");

        let prev_c = std::env::var_os("CODEX_HOME");
        let prev_g = std::env::var_os("GEMINI_CLI_HOME");
        std::env::set_var("CODEX_HOME", &over);
        std::env::set_var("GEMINI_CLI_HOME", &over);
        let codex = tool("codex").settings_file(home);
        let gemini = tool("gemini").settings_file(home);
        // A tool with no override is home-relative no matter what those variables say.
        let cursor = tool("cursor-agent").settings_file(home);
        std::env::remove_var("CODEX_HOME");
        std::env::remove_var("GEMINI_CLI_HOME");
        let codex_unset = tool("codex").settings_file(home);
        let gemini_unset = tool("gemini").settings_file(home);
        match prev_c {
            Some(v) => std::env::set_var("CODEX_HOME", v),
            None => std::env::remove_var("CODEX_HOME"),
        }
        match prev_g {
            Some(v) => std::env::set_var("GEMINI_CLI_HOME", v),
            None => std::env::remove_var("GEMINI_CLI_HOME"),
        }

        assert_eq!(codex, over.join("hooks.json"));
        assert_eq!(gemini, over.join(".gemini").join("settings.json"));
        assert_eq!(cursor, home.join(".cursor").join("hooks.json"));
        assert_eq!(codex_unset, home.join(".codex").join("hooks.json"));
        assert_eq!(gemini_unset, home.join(".gemini").join("settings.json"));
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
        std::fs::write(
            &path,
            r#"{"sessionId":"e91284c5-9826-4ae9-839c-f96b1ac7fbe7"}"#,
        )
        .unwrap();
        assert_eq!(read_pane_mark("cursor-agent", &pane), None);

        // And a hostile id never reaches the command line it would land on.
        std::fs::write(&path, r#"{"sessionId":"x; rm -rf /","cwd":"/tmp/proj"}"#).unwrap();
        assert_eq!(read_pane_mark("cursor-agent", &pane), None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_windows_command_survives_a_path_with_spaces_and_a_restricted_policy() {
        // Both flags are load-bearing and neither failure is visible: `Restricted` is the
        // default execution policy on a fresh Windows install and refuses the script before
        // its first line, and an unquoted path breaks on the `C:\Program Files\` install
        // that most people get. Nothing surfaces a hook's exit status, so either mistake
        // reads as "the tool just doesn't resume".
        let cmd = windows_hook_command(
            Path::new(r"C:\Program Files\Hyperpanes\resources\hooks\hp-session-hook.ps1"),
            "codex",
        );
        assert_eq!(
            cmd,
            "powershell -NoProfile -ExecutionPolicy Bypass -File \
             \"C:\\Program Files\\Hyperpanes\\resources\\hooks\\hp-session-hook.ps1\" -Tool codex"
        );

        // One script serves every tool, so the `-Tool` argument is the only thing telling
        // it which payload shape and which marker directory it is running for.
        for t in HOOKED_TOOLS {
            let cmd = windows_hook_command(Path::new("C:\\hp\\hook.ps1"), t.id);
            assert!(cmd.ends_with(&format!(" -Tool {}", t.id)), "{cmd}");
        }
    }

    #[test]
    fn every_hook_ships_in_every_packaging_manifest() {
        // The set of shipped hooks is spelled out in six places — four packaging manifests,
        // the deb and rpm asset lists inside one Cargo.toml, and build.rs's dev deploy —
        // and each of the last two tools added reached some of them and not the others.
        // A missed one is silent by construction: `bundled_script` returning None makes
        // registration a no-op, so the only symptom is that panes of that tool stop
        // resuming, on that platform, months later.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let read = |rel: &str| {
            std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| {
                panic!("{rel}: {e} — packaging manifests must stay in the repo")
            })
        };
        let cargo = read("rs/crates/app/Cargo.toml");
        // deb and rpm are two independent asset lists in that one file, and checking the
        // file as a whole would let an entry present in only one of them pass.
        let (deb, rpm) = cargo
            .split_once("[package.metadata.generate-rpm]")
            .expect("rs/crates/app/Cargo.toml still has an rpm asset list");
        let build_rs = read("rs/crates/app/build.rs");
        let appimage = read("rs/packaging/appimage.sh");
        let macos = read("rs/packaging/macos/bundle.sh");
        let nsis = read("rs/packaging/installer.nsi");

        // Claude's hook is not in HOOKED_TOOLS — `claude_hook` owns it — but it is shipped
        // by the same manifests and has been dropped from one before.
        let posix: Vec<&str> = std::iter::once("hp-claude-session-hook.sh")
            .chain(HOOKED_TOOLS.iter().map(|t| t.script))
            .collect();

        for script in &posix {
            for (name, text) in [
                ("the [package.metadata.deb] asset list", deb),
                ("the [package.metadata.generate-rpm] asset list", rpm),
                ("rs/crates/app/build.rs", build_rs.as_str()),
                ("rs/packaging/appimage.sh", appimage.as_str()),
                ("rs/packaging/macos/bundle.sh", macos.as_str()),
            ] {
                assert!(text.contains(script), "{script} is not shipped by {name}");
            }
            // Deliberately NOT installer.nsi: Windows ships one .ps1 instead of the five
            // `sh` scripts (see the module's `# Windows` section).
            assert!(
                !nsis.contains(script),
                "{script} is a POSIX hook; Windows ships {} instead",
                WINDOWS_HOOK.1
            );
        }

        for (name, text) in [
            ("rs/packaging/installer.nsi", &nsis),
            ("rs/crates/app/build.rs", &build_rs),
        ] {
            assert!(
                text.contains(WINDOWS_HOOK.1),
                "{} is not shipped by {name}",
                WINDOWS_HOOK.1
            );
        }

        // And every script a manifest names has to actually exist to be copied.
        for script in &posix {
            let dir = HOOKED_TOOLS
                .iter()
                .find(|t| t.script == *script)
                .map(|t| t.dir)
                .unwrap_or("claude");
            assert!(
                root.join("resources").join(dir).join(script).is_file(),
                "resources/{dir}/{script} is missing"
            );
        }
        assert!(
            root.join("resources")
                .join(WINDOWS_HOOK.0)
                .join(WINDOWS_HOOK.1)
                .is_file(),
            "resources/{}/{} is missing",
            WINDOWS_HOOK.0,
            WINDOWS_HOOK.1
        );
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
