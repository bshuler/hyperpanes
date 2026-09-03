//! Per-pane dictation: hold the mic, talk, and the transcript is typed into the pane.
//!
//! The mirror image of [`crate::speech`], and deliberately built the same way — two
//! external commands, auto-detected, each overridable from a settings file:
//!
//! ```text
//! click mic  → recorder (in-process, or a command) → <state>/dictation/<pane>.wav
//! click stop → (graceful stop)   → transcriber (in-process, or a command) → pane input
//! ```
//!
//! **In-process first, commands as the fallback.** This started out as commands only —
//! `ffmpeg`, `sox`/`rec`, `arecord` — on the reasoning that every desktop that can record
//! already ships something that records. It doesn't. A stock macOS has none of the three,
//! and a GUI app cannot see a Homebrew install anyway (see
//! [`crate::speech::engine::resolve`]), so the mic button's whole job became reporting
//! "no recorder found" to someone whose microphone was working fine. [`native`] captures
//! through the OS audio API instead and needs nothing installed; the commands stay as the
//! fallback and the `{wav}` template stays as the override, which is what keeps the
//! pipeline testable headless (point `recordTemplate` at a script that copies a fixture
//! WAV). The bill for it is one build-time dependency on Linux, `libasound2-dev`.
//!
//! The transcript half had the same hole and got the same treatment. `whisper` (a Python
//! package) and `whisper-cli` (Homebrew/apt) are on no stock machine either, so a recorder
//! that finally worked just moved the failure one step later, to "no transcriber found".
//! [`whisper`] compiles whisper.cpp into this binary. What it cannot compile in are the
//! weights — those are downloaded once, checked against a pinned SHA-256, and cached; from
//! then on dictation is entirely offline and entirely local. Nothing is ever uploaded: the
//! audio does not leave the machine, which is the other reason this is in-process rather
//! than a cloud API.
//!
//! Owned modules: [`native`] (in-process capture), [`whisper`] (in-process transcription
//! and its model cache), [`backend`] (detection + argv construction) and [`dictation`] (the per-pane record → stop → transcribe state
//! machine). This file holds only the persisted settings shape.

pub mod backend;
pub mod dictation;
pub mod native;
pub mod whisper;

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Per-installation dictation settings, persisted to `stt.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SttSettings {
    /// Custom recorder command, e.g. `["ffmpeg", "-f", "avfoundation", "-i", ":default", "{wav}"]`.
    /// `{wav}` in any argument is replaced with the output path. Set, it beats in-process
    /// capture too — an override that only overrode the fallbacks would be no override at
    /// all. `None` -> auto-detect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_template: Option<Vec<String>>,
    /// Custom transcriber command, e.g. `["whisper-cli", "-f", "{wav}", "-nt"]`. The
    /// transcript is read from stdout. `None` -> auto-detect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcribe_template: Option<Vec<String>>,
    /// Which model to transcribe with. Either one of [`whisper::MODELS`]' names
    /// (`tiny.en`, `base.en`, `small.en`) — downloaded on demand by the in-process
    /// transcriber — or a path to a `ggml-*.bin` of the user's own, which is also what
    /// `whisper-cli` is passed as `-m`. `None` -> [`whisper::DEFAULT_MODEL`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Press Enter after inserting the transcript. Off by default: dictation is not
    /// reliable enough to send a prompt no human has read back.
    #[serde(default)]
    pub submit: bool,
}

/// Read settings from `path`, falling back to [`SttSettings::default`] on a missing or
/// corrupt file.
#[tracing::instrument(level = "debug", ret)]
pub fn load(path: &Path) -> SttSettings {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return SttSettings::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Persist `settings` to `path`, atomically.
#[tracing::instrument(level = "debug", ret)]
pub fn save(path: &Path, settings: &SttSettings) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::persistence::paths::write_atomic(path, json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("hp-stt-settings-{}-{tag}.json", std::process::id()))
    }

    #[test]
    fn missing_file_yields_defaults() {
        let p = temp_path("missing");
        let _ = std::fs::remove_file(&p);
        assert_eq!(load(&p), SttSettings::default());
    }

    #[test]
    fn corrupt_file_yields_defaults() {
        let p = temp_path("corrupt");
        std::fs::write(&p, "{ not json").unwrap();
        assert_eq!(load(&p), SttSettings::default());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn save_then_load_round_trips() {
        let p = temp_path("round");
        let s = SttSettings {
            record_template: Some(vec!["rec".into(), "{wav}".into()]),
            transcribe_template: Some(vec!["whisper-cli".into(), "-f".into(), "{wav}".into()]),
            model: Some("/models/ggml-base.en.bin".into()),
            submit: true,
        };
        save(&p, &s).unwrap();
        assert_eq!(load(&p), s);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unset_templates_are_omitted_from_json() {
        let json = serde_json::to_string(&SttSettings::default()).unwrap();
        assert!(!json.contains("recordTemplate"), "{json}");
        assert!(!json.contains("transcribeTemplate"), "{json}");
        assert!(json.contains("submit"), "{json}");
    }

    #[test]
    fn dictation_never_submits_unless_asked() {
        // A misheard word in a prompt that was already sent is not recoverable.
        assert!(!SttSettings::default().submit);
    }
}
