//! Which external commands do the recording and the transcribing, and how to build
//! their argv.
//!
//! Both halves follow [`crate::speech::engine`]'s backend rules: a user template always
//! wins, auto-detection walks `PATH` in preference order, and "nothing installed" is a
//! first-class outcome ([`Recorder::None`] / [`Transcriber::None`]) rather than an error —
//! the GUI turns it into one notice and the mic button stays inert.

use super::SttSettings;
use std::path::Path;
use std::process::Command;
#[cfg(windows)]
use std::process::Stdio;

/// Placeholder replaced with the WAV path in a user-supplied template.
const WAV_PLACEHOLDER: &str = "{wav}";
/// Mono 16 kHz: what every Whisper build wants, and what keeps a minute of dictation
/// under 2 MB.
const SAMPLE_RATE: &str = "16000";
/// Hard ceiling on one recording, in seconds. A mic left on by accident should stop
/// itself rather than fill a disk; the recorder enforces it so the cap survives even a
/// crashed GUI that never sends a stop.
const MAX_RECORD_SECS: &str = "300";

// =========================== recording ===========================

/// How a running recorder is asked to finish the file it is writing.
///
/// This is the whole reason the recorder is not just "spawn and kill": a WAV's header
/// carries the data length, so a recorder killed mid-write leaves a file whose header
/// says zero and which most decoders refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    /// ffmpeg finalizes and exits when it reads `q` on stdin — the one graceful stop
    /// that works identically on Windows, where there is no SIGINT to send.
    FfmpegQuit,
    /// SIGINT, the only stop `rec` and `arecord` understand. Unix-only, which is fine:
    /// neither runs anywhere else.
    Interrupt,
}

/// The command that captures microphone audio to a WAV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorder {
    /// User-supplied argv; `{wav}` in any argument is replaced with the output path.
    /// Stopped like ffmpeg on Windows and by signal elsewhere — a custom recorder is
    /// most often a shell script, and a script reacts to SIGINT.
    Custom(Vec<String>),
    /// `ffmpeg`, with the platform's capture device. `dev` is the input spec: `:default`
    /// (avfoundation), `default` (pulse), or the enumerated dshow device name.
    Ffmpeg { dev: String, format: &'static str },
    /// `rec`, sox's recording front end.
    Rec,
    /// ALSA's `arecord`.
    Arecord,
    /// Nothing on this machine can record.
    None,
}

impl Recorder {
    pub fn name(&self) -> &'static str {
        match self {
            Recorder::Custom(_) => "custom",
            Recorder::Ffmpeg { .. } => "ffmpeg",
            Recorder::Rec => "rec",
            Recorder::Arecord => "arecord",
            Recorder::None => "none",
        }
    }

    pub fn stop_kind(&self) -> StopKind {
        match self {
            Recorder::Ffmpeg { .. } => StopKind::FfmpegQuit,
            _ if cfg!(windows) => StopKind::FfmpegQuit,
            _ => StopKind::Interrupt,
        }
    }

    /// The command to spawn, writing its capture to `wav`. `None` for [`Recorder::None`]
    /// and for an empty custom template.
    pub fn build_command(&self, wav: &Path) -> Option<Command> {
        let wav_s = wav.to_string_lossy().to_string();
        match self {
            Recorder::Custom(argv) => custom_command(argv, WAV_PLACEHOLDER, &wav_s),
            Recorder::Ffmpeg { dev, format } => {
                let mut c = command_for("ffmpeg");
                c.args(["-hide_banner", "-loglevel", "error", "-f", format, "-i"])
                    .arg(dev)
                    .args(["-ac", "1", "-ar", SAMPLE_RATE, "-t", MAX_RECORD_SECS, "-y"])
                    .arg(&wav_s);
                Some(c)
            }
            Recorder::Rec => {
                let mut c = command_for("rec");
                c.args(["-q", "-c", "1", "-r", SAMPLE_RATE])
                    .arg(&wav_s)
                    .args(["trim", "0", MAX_RECORD_SECS]);
                Some(c)
            }
            Recorder::Arecord => {
                let mut c = command_for("arecord");
                c.args([
                    "-q",
                    "-f",
                    "S16_LE",
                    "-c",
                    "1",
                    "-r",
                    SAMPLE_RATE,
                    "-d",
                    MAX_RECORD_SECS,
                ])
                .arg(&wav_s);
                Some(c)
            }
            Recorder::None => None,
        }
    }
}

/// Pick a recorder: the configured template if there is one, else the first thing on
/// `PATH` that can capture on this platform.
pub fn detect_recorder(settings: &SttSettings) -> Recorder {
    if let Some(argv) = settings.record_template.as_ref() {
        if !argv.is_empty() {
            return Recorder::Custom(argv.clone());
        }
    }
    if on_path("ffmpeg") {
        if let Some((dev, format)) = ffmpeg_input() {
            return Recorder::Ffmpeg { dev, format };
        }
    }
    if cfg!(unix) && on_path("rec") {
        return Recorder::Rec;
    }
    if cfg!(target_os = "linux") && on_path("arecord") {
        return Recorder::Arecord;
    }
    Recorder::None
}

/// ffmpeg's `-f <format> -i <device>` pair for this platform.
///
/// macOS and Linux both name a default device, so the pair is a constant. Windows does
/// not: dshow has no "default microphone" spelling, so the device has to be enumerated
/// before the first recording, and a machine with no capture device yields `None`.
fn ffmpeg_input() -> Option<(String, &'static str)> {
    #[cfg(target_os = "macos")]
    {
        Some((":default".to_string(), "avfoundation"))
    }
    #[cfg(target_os = "linux")]
    {
        Some(("default".to_string(), "pulse"))
    }
    #[cfg(windows)]
    {
        let out = command_for("ffmpeg")
            .args([
                "-hide_banner",
                "-list_devices",
                "true",
                "-f",
                "dshow",
                "-i",
                "dummy",
            ])
            .stdin(Stdio::null())
            .output()
            .ok()?;
        // The enumeration is written to stderr and always exits non-zero (the `dummy`
        // input never opens), so the status is deliberately ignored.
        let name = first_dshow_audio_device(&String::from_utf8_lossy(&out.stderr))?;
        Some((format!("audio={name}"), "dshow"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        None
    }
}

/// The first quoted device name after ffmpeg's "audio devices" heading.
///
/// Every entry is followed by an `Alternative name "@device_cm_{…}"` line — the GUID
/// spelling, which works but is unreadable in a settings file the user may want to
/// override, so friendly names win.
pub fn first_dshow_audio_device(stderr: &str) -> Option<String> {
    let mut in_audio = false;
    for line in stderr.lines() {
        if line.contains("audio devices") {
            in_audio = true;
            continue;
        }
        if line.contains("video devices") {
            in_audio = false;
            continue;
        }
        if !in_audio || line.contains("Alternative name") {
            continue;
        }
        if let Some(name) = quoted(line) {
            return Some(name);
        }
    }
    None
}

/// The contents of the first `"…"` pair in `line`.
fn quoted(line: &str) -> Option<String> {
    let start = line.find('"')? + 1;
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

// =========================== transcribing ===========================

/// The command that turns a WAV into text on stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transcriber {
    /// User-supplied argv; `{wav}` is replaced with the recording's path.
    Custom(Vec<String>),
    /// The Python `whisper` CLI, which fetches its own weights on first use.
    Whisper,
    /// whisper.cpp's `whisper-cli`, which needs a model file passed to `-m`.
    WhisperCpp { model: String },
    /// Nothing on this machine can transcribe.
    None,
}

impl Transcriber {
    pub fn name(&self) -> &'static str {
        match self {
            Transcriber::Custom(_) => "custom",
            Transcriber::Whisper => "whisper",
            Transcriber::WhisperCpp { .. } => "whisper-cli",
            Transcriber::None => "none",
        }
    }

    pub fn build_command(&self, wav: &Path) -> Option<Command> {
        let wav_s = wav.to_string_lossy().to_string();
        match self {
            Transcriber::Custom(argv) => custom_command(argv, WAV_PLACEHOLDER, &wav_s),
            Transcriber::Whisper => {
                let mut c = command_for("whisper");
                // `--output_format txt` still prints the timed transcript to stdout;
                // `clean_transcript` strips the timestamps, so no output file is needed
                // and nothing is left behind to clean up.
                c.arg(&wav_s)
                    .args(["--model", "base.en", "--output_format", "txt"]);
                Some(c)
            }
            Transcriber::WhisperCpp { model } => {
                let mut c = command_for("whisper-cli");
                c.args(["-m", model, "-f", &wav_s, "-nt", "-np"]);
                Some(c)
            }
            Transcriber::None => None,
        }
    }
}

/// Pick a transcriber. A configured template wins; otherwise `whisper` is preferred over
/// `whisper-cli` because it needs no model path — and `whisper-cli` is only offered when
/// a model was actually configured, since without one it exits before reading the audio.
pub fn detect_transcriber(settings: &SttSettings) -> Transcriber {
    if let Some(argv) = settings.transcribe_template.as_ref() {
        if !argv.is_empty() {
            return Transcriber::Custom(argv.clone());
        }
    }
    if on_path("whisper") {
        return Transcriber::Whisper;
    }
    if on_path("whisper-cli") {
        if let Some(model) = settings.model.as_ref().filter(|m| !m.is_empty()) {
            return Transcriber::WhisperCpp {
                model: model.clone(),
            };
        }
    }
    Transcriber::None
}

/// Strip a transcriber's decoration down to the words that were said.
///
/// Whisper prints `[00:00.000 --> 00:02.000]  text` per segment, and both builds emit
/// bracketed non-speech markers (`[BLANK_AUDIO]`, `[Music]`) that must never be typed
/// into a pane as if the human had said them.
pub fn clean_transcript(stdout: &str) -> String {
    let mut words: Vec<&str> = Vec::new();
    for line in stdout.lines() {
        let line = strip_timestamp(line.trim());
        if line.is_empty() || is_bracketed_marker(line) {
            continue;
        }
        words.extend(line.split_whitespace());
    }
    words.join(" ")
}

/// Drop a leading `[… --> …]` segment stamp, if present.
fn strip_timestamp(line: &str) -> &str {
    let Some(rest) = line.strip_prefix('[') else {
        return line;
    };
    let Some(close) = rest.find(']') else {
        return line;
    };
    if !rest[..close].contains("-->") {
        return line;
    }
    rest[close + 1..].trim()
}

/// Is the whole line a single `[…]` or `(…)` non-speech marker?
fn is_bracketed_marker(line: &str) -> bool {
    let inner = match (line.starts_with('['), line.starts_with('(')) {
        (true, _) => line.strip_prefix('[').and_then(|s| s.strip_suffix(']')),
        (_, true) => line.strip_prefix('(').and_then(|s| s.strip_suffix(')')),
        _ => None,
    };
    inner.is_some_and(|i| !i.contains('[') && !i.contains('('))
}

// =========================== shared ===========================

/// Build a command from a user template, substituting `placeholder` in every argument.
fn custom_command(argv: &[String], placeholder: &str, value: &str) -> Option<Command> {
    let (prog, rest) = argv.split_first()?;
    let mut c = command_for(&prog.replace(placeholder, value));
    for a in rest {
        c.arg(a.replace(placeholder, value));
    }
    Some(c)
}

/// Is `cmd` an executable somewhere on `PATH`? Shares [`crate::speech::engine`]'s
/// `PATHEXT`-aware probe so a Windows box resolves `ffmpeg.EXE` from a bare name.
fn on_path(cmd: &str) -> bool {
    crate::speech::engine::on_path(cmd)
}

/// Spawn a tool through its resolved absolute path — see
/// [`crate::speech::engine::command_for`]. A GUI launched from the Dock has no Homebrew
/// on `PATH`, so a bare `Command::new("ffmpeg")` fails on a machine that plainly has it.
fn command_for(cmd: &str) -> Command {
    crate::speech::engine::command_for(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(c: &Command) -> Vec<String> {
        std::iter::once(c.get_program())
            .chain(c.get_args())
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    // ---- recorder ----

    #[test]
    fn a_custom_record_template_wins_over_the_platform_recorder() {
        let s = SttSettings {
            record_template: Some(vec!["mycap".into(), "-o".into(), "{wav}".into()]),
            ..Default::default()
        };
        let r = detect_recorder(&s);
        assert_eq!(r.name(), "custom");
        let c = r.build_command(Path::new("/tmp/a.wav")).unwrap();
        assert_eq!(argv(&c), vec!["mycap", "-o", "/tmp/a.wav"]);
    }

    #[test]
    fn an_empty_template_is_not_a_recorder() {
        // An empty array in the settings file is a mistake, not an instruction to run
        // nothing — fall through to detection rather than spawning ""[0].
        let s = SttSettings {
            record_template: Some(vec![]),
            ..Default::default()
        };
        assert_ne!(detect_recorder(&s).name(), "custom");
    }

    #[test]
    fn every_recorder_but_none_builds_a_command_writing_to_the_wav() {
        let wav = Path::new("/tmp/rec.wav");
        for r in [
            Recorder::Custom(vec!["s".into(), "{wav}".into()]),
            Recorder::Ffmpeg {
                dev: ":default".into(),
                format: "avfoundation",
            },
            Recorder::Rec,
            Recorder::Arecord,
        ] {
            let c = r
                .build_command(wav)
                .unwrap_or_else(|| panic!("{} builds no command", r.name()));
            assert!(
                argv(&c).contains(&"/tmp/rec.wav".to_string()),
                "{} never names its output",
                r.name()
            );
        }
        assert!(Recorder::None.build_command(wav).is_none());
    }

    #[test]
    fn ffmpeg_is_always_stopped_by_stdin_never_by_signal() {
        // TerminateProcess on Windows and SIGKILL anywhere would leave a WAV whose
        // header claims zero bytes of audio.
        let f = Recorder::Ffmpeg {
            dev: "default".into(),
            format: "pulse",
        };
        assert_eq!(f.stop_kind(), StopKind::FfmpegQuit);
    }

    #[cfg(unix)]
    #[test]
    fn sox_and_alsa_stop_on_a_signal() {
        assert_eq!(Recorder::Rec.stop_kind(), StopKind::Interrupt);
        assert_eq!(Recorder::Arecord.stop_kind(), StopKind::Interrupt);
    }

    // ---- dshow enumeration ----

    const DSHOW: &str = r#"[dshow @ 0x1] "USB Webcam" (video)
[dshow @ 0x1]   Alternative name "@device_pnp_\\?\usb#vid_046d"
[dshow @ 0x1] DirectShow audio devices
[dshow @ 0x1] "Microphone (Realtek Audio)"
[dshow @ 0x1]   Alternative name "@device_cm_{33D9A762}\Microphone"
[dshow @ 0x1] "Line In (Realtek Audio)""#;

    #[test]
    fn the_first_friendly_audio_device_wins_not_the_guid() {
        assert_eq!(
            first_dshow_audio_device(DSHOW).as_deref(),
            Some("Microphone (Realtek Audio)")
        );
    }

    #[test]
    fn a_machine_with_no_audio_device_enumerates_to_nothing() {
        let video_only = "[dshow @ 0x1] DirectShow video devices\n[dshow @ 0x1] \"Cam\"";
        assert!(first_dshow_audio_device(video_only).is_none());
        assert!(first_dshow_audio_device("").is_none());
    }

    // ---- transcriber ----

    #[test]
    fn a_custom_transcribe_template_wins_and_gets_the_wav() {
        let s = SttSettings {
            transcribe_template: Some(vec!["stt".into(), "--in={wav}".into()]),
            ..Default::default()
        };
        let t = detect_transcriber(&s);
        let c = t.build_command(Path::new("/tmp/a.wav")).unwrap();
        assert_eq!(argv(&c), vec!["stt", "--in=/tmp/a.wav"]);
    }

    #[test]
    fn whisper_cpp_is_never_offered_without_the_model_it_cannot_run_without() {
        let s = SttSettings::default();
        assert!(!matches!(
            detect_transcriber(&s),
            Transcriber::WhisperCpp { .. }
        ));
        let with_model = SttSettings {
            model: Some("".into()),
            ..Default::default()
        };
        assert!(!matches!(
            detect_transcriber(&with_model),
            Transcriber::WhisperCpp { .. }
        ));
    }

    #[test]
    fn whisper_cpp_passes_its_model_to_dash_m() {
        let t = Transcriber::WhisperCpp {
            model: "/m/base.bin".into(),
        };
        let a = argv(&t.build_command(Path::new("/tmp/a.wav")).unwrap());
        let m = a.iter().position(|x| x == "-m").expect("no -m");
        assert_eq!(a[m + 1], "/m/base.bin");
    }

    // ---- transcript cleanup ----

    #[test]
    fn segment_timestamps_are_stripped_and_segments_joined() {
        let out = "[00:00.000 --> 00:02.000]  Open the file\n\
                   [00:02.000 --> 00:04.500]  and run the tests.\n";
        assert_eq!(clean_transcript(out), "Open the file and run the tests.");
    }

    #[test]
    fn non_speech_markers_are_never_typed_into_a_pane() {
        let out = "[BLANK_AUDIO]\n[00:00.000 --> 00:01.000]  hello\n(Music)\n[ Silence ]\n";
        assert_eq!(clean_transcript(out), "hello");
    }

    #[test]
    fn plain_untimed_output_survives_untouched() {
        // whisper-cli's `-nt` prints bare prose; nothing about it looks like a stamp.
        assert_eq!(clean_transcript("  just words  \n"), "just words");
    }

    #[test]
    fn a_bracket_that_is_not_a_stamp_is_left_alone() {
        assert_eq!(
            clean_transcript("[1] plus [2] equals three"),
            "[1] plus [2] equals three"
        );
    }

    #[test]
    fn silence_transcribes_to_nothing_at_all() {
        assert_eq!(clean_transcript("[BLANK_AUDIO]\n\n"), "");
        assert_eq!(clean_transcript(""), "");
    }
}
