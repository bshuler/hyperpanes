//! The control layer's dictation surface: persisted [`SttSettings`], the per-pane
//! [`Dictation`] state machine, and the one rule that makes a transcript safe to type
//! into a live terminal.
//!
//! Unlike its sibling [`speech_service`](crate::control::speech_service), this has no
//! background loop. Dictation is entirely user-driven — a click starts it, a click stops
//! it — so there is nothing to poll and an install where nobody ever presses the mic does
//! no work at all.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use crate::control::input::SUBMIT_DELAY_MS;
use crate::control::server::Shared;
use crate::permissions::{self, Grant, Right};
use crate::stt::dictation::{Dictation, Transcript};
use crate::stt::{self, SttSettings};

/// A point-in-time snapshot for `/state`'s `dictation` field.
pub struct DictationStatus {
    pub recorder: String,
    pub transcriber: String,
    pub recording_panes: Vec<String>,
}

/// Owns dictation settings and every pane's recording state.
pub struct DictationService {
    settings_path: PathBuf,
    settings: Mutex<SttSettings>,
    dictation: Dictation,
}

impl DictationService {
    /// `settings_path` is `stt.json`'s path. Recordings are scratch and go to a
    /// pid-scoped temp directory — never beside the settings, and never anywhere a
    /// backup or a dotfile sync would pick up raw audio of the user.
    pub fn new(settings_path: PathBuf) -> Self {
        let settings = stt::load(&settings_path);
        let wav_dir =
            std::env::temp_dir().join(format!("hyperpanes-dictation-{}", std::process::id()));
        DictationService {
            settings_path,
            settings: Mutex::new(settings),
            dictation: Dictation::new(wav_dir),
        }
    }

    fn settings_snapshot(&self) -> SttSettings {
        self.settings
            .lock()
            .expect("stt settings lock poisoned")
            .clone()
    }

    /// Re-read `stt.json` — the settings routes call this after a user edits it, so a new
    /// `transcribeTemplate` takes effect without a restart.
    pub fn reload(&self) {
        let fresh = stt::load(&self.settings_path);
        *self.settings.lock().expect("stt settings lock poisoned") = fresh;
    }

    pub fn recording_panes(&self) -> Vec<String> {
        self.dictation.recording_panes()
    }

    pub fn is_recording(&self, pane_id: &str) -> bool {
        self.dictation.is_recording(pane_id)
    }

    /// Whether a finished transcript should be submitted (Enter) as well as typed.
    pub fn submit_after_insert(&self) -> bool {
        self.settings_snapshot().submit
    }

    pub fn start(&self, pane_id: &str) -> Result<&'static str, String> {
        // Raise the OS's own consent dialog from the feature that needs it, at the moment it
        // needs it — macOS shows each one once ever, so a mic prompt spent on a settings list
        // is one dictation never gets. Where the OS has no dialog this is a status read.
        if permissions::prompt(Right::Microphone) == Grant::Denied {
            return Err(format!(
                "{} access is denied — grant it in system settings",
                Right::Microphone.label()
            ));
        }
        self.dictation.start(pane_id, &self.settings_snapshot())
    }

    /// Take the user to the OS's microphone setting.
    ///
    /// This exists because "nothing was recorded" is, on macOS, far more often a denied mic
    /// than a broken recorder — and [`permissions::status`] there is `Undetermined`, since
    /// reading the real answer means loading AVFoundation. Offering the door is honest;
    /// asserting a grant we cannot read would not be.
    pub fn open_microphone_settings(&self) -> Result<(), String> {
        permissions::request(Right::Microphone)
    }

    /// Stop and transcribe. **Blocking** — seconds, on a cold model — so the HTTP layer
    /// runs it on a blocking task rather than the async executor.
    pub fn stop(&self, pane_id: &str) -> Result<Transcript, String> {
        self.dictation.stop(pane_id, &self.settings_snapshot())
    }

    pub fn cancel(&self, pane_id: &str) {
        self.dictation.cancel(pane_id);
    }

    pub fn cancel_all(&self) {
        self.dictation.cancel_all();
    }

    pub fn status(&self) -> DictationStatus {
        let s = self.settings_snapshot();
        DictationStatus {
            recorder: crate::stt::backend::detect_recorder(&s).name().to_string(),
            transcriber: crate::stt::backend::detect_transcriber(&s)
                .name()
                .to_string(),
            recording_panes: self.dictation.recording_panes(),
        }
    }
}

/// What a finished dictation put into a pane.
pub struct Delivered {
    pub text: String,
    pub backend: &'static str,
    pub submitted: bool,
}

/// Stop `pane_id`'s recording, transcribe it, and type the result into `uid`'s pty.
///
/// **Blocking** — a cold transcriber is seconds. Both callers run it off their own thread:
/// the control route on `spawn_blocking`, the GUI's mic button on a worker, which is also
/// why the submit delay here is a plain sleep rather than a timer. They share this function
/// so a transcript can never reach a pane by two subtly different routes.
pub fn stop_and_deliver(shared: &Shared, pane_id: &str, uid: &str) -> Result<Delivered, String> {
    let transcript = shared.dictation.stop(pane_id)?;
    let text = sanitize_for_pane(&transcript.text);
    if text.is_empty() {
        return Err("no speech in the recording".to_string());
    }
    let submitted = shared.dictation.submit_after_insert();
    shared.sessions.write(uid, &text);
    if submitted {
        // A separate, later write — exactly as `/panes/{id}/input` does it — so a
        // bracketed-paste TUI reads the Enter as a keypress and not as pasted content.
        std::thread::sleep(Duration::from_millis(SUBMIT_DELAY_MS));
        shared.sessions.write(uid, "\r");
    }
    Ok(Delivered {
        text,
        backend: transcript.backend,
        submitted,
    })
}

/// Make a transcript safe to write into a live terminal.
///
/// Whisper transcribes what it hears, and what it hears is arbitrary sound in a room. A
/// transcript is therefore untrusted input on its way to a shell: a stray control
/// character could execute a line the user never finished dictating, and an embedded
/// newline could submit it. Both are flattened to spaces — the human presses Enter.
pub fn sanitize_for_pane(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = false;
    for c in text.chars() {
        let c = if c.is_control() || c == '\u{7f}' {
            ' '
        } else {
            c
        };
        if c == ' ' {
            if !last_space && !out.is_empty() {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_settings(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hp-dictation-svc-{}-{tag}.json",
            std::process::id()
        ))
    }

    #[test]
    fn a_fresh_service_is_recording_nothing() {
        let d = DictationService::new(temp_settings("fresh"));
        assert!(d.recording_panes().is_empty());
        assert!(!d.is_recording("p1"));
        assert!(
            !d.submit_after_insert(),
            "dictation never auto-submits by default"
        );
    }

    #[test]
    fn status_names_both_halves_even_with_nothing_installed() {
        let d = DictationService::new(temp_settings("status"));
        let s = d.status();
        assert!(!s.recorder.is_empty());
        assert!(!s.transcriber.is_empty());
        assert!(s.recording_panes.is_empty());
    }

    #[test]
    fn reload_picks_up_an_edited_settings_file() {
        let p = temp_settings("reload");
        let _ = std::fs::remove_file(&p);
        let d = DictationService::new(p.clone());
        assert!(!d.submit_after_insert());

        stt::save(
            &p,
            &SttSettings {
                submit: true,
                ..Default::default()
            },
        )
        .unwrap();
        d.reload();
        assert!(d.submit_after_insert());
        let _ = std::fs::remove_file(&p);
    }

    // ---- sanitize_for_pane ----

    #[test]
    fn a_newline_in_a_transcript_never_submits_the_line() {
        // The whole risk of dictation into a shell: a CR is Enter.
        assert_eq!(sanitize_for_pane("rm -rf /\nyes"), "rm -rf / yes");
        assert_eq!(sanitize_for_pane("a\rb"), "a b");
    }

    #[test]
    fn control_characters_are_flattened_not_passed_through() {
        assert_eq!(sanitize_for_pane("hi\u{1b}[2Jthere"), "hi [2Jthere");
        assert_eq!(sanitize_for_pane("tab\there"), "tab here");
        assert_eq!(sanitize_for_pane("del\u{7f}x"), "del x");
    }

    #[test]
    fn ordinary_prose_is_left_exactly_as_dictated() {
        assert_eq!(
            sanitize_for_pane("open the file and run the tests"),
            "open the file and run the tests"
        );
    }

    #[test]
    fn leading_and_repeated_whitespace_collapses() {
        assert_eq!(sanitize_for_pane("   two    words  "), "two words");
        assert_eq!(sanitize_for_pane(""), "");
    }

    #[test]
    fn non_ascii_speech_survives() {
        // Whisper transcribes many languages; nothing here is ASCII-only.
        assert_eq!(sanitize_for_pane("café ☕ 日本語"), "café ☕ 日本語");
    }
}
