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

/// What captures microphone audio to a WAV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorder {
    /// In-process capture through the OS audio API — CoreAudio, WASAPI, ALSA. The only
    /// variant that is not a command, and the only one that works on a machine with
    /// nothing installed. See [`super::native`].
    Native,
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
    #[tracing::instrument(level = "debug", ret)]
    pub fn name(&self) -> &'static str {
        match self {
            Recorder::Native => "native",
            Recorder::Custom(_) => "custom",
            Recorder::Ffmpeg { .. } => "ffmpeg",
            Recorder::Rec => "rec",
            Recorder::Arecord => "arecord",
            Recorder::None => "none",
        }
    }

    /// Only meaningful for the variants that spawn a process; [`Recorder::Native`] is
    /// stopped over a channel, not a signal, so its answer here is never consulted.
    #[tracing::instrument(level = "debug", ret)]
    pub fn stop_kind(&self) -> StopKind {
        match self {
            Recorder::Ffmpeg { .. } => StopKind::FfmpegQuit,
            _ if cfg!(windows) => StopKind::FfmpegQuit,
            _ => StopKind::Interrupt,
        }
    }

    /// The command to spawn, writing its capture to `wav`. `None` for the variants that
    /// spawn nothing: [`Recorder::Native`], [`Recorder::None`], and an empty custom
    /// template.
    #[tracing::instrument(level = "debug", ret)]
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
            // Captured in-process; there is no command. `dictation` branches on this
            // variant before it ever asks for one.
            Recorder::Native | Recorder::None => None,
        }
    }
}

/// Pick a recorder: the configured template if there is one, else in-process capture,
/// else the first thing on `PATH` that can capture on this platform.
#[tracing::instrument(level = "debug", ret)]
pub fn detect_recorder(settings: &SttSettings) -> Recorder {
    if let Some(argv) = settings.record_template.as_ref() {
        if !argv.is_empty() {
            return Recorder::Custom(argv.clone());
        }
    }
    // Before every external tool, because it is the one that does not have to be
    // installed. It steps aside only when this machine has no usable input device at
    // all, in which case an `ffmpeg` capture would have had nothing to open either.
    if super::native::available() {
        return Recorder::Native;
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
#[tracing::instrument(level = "debug", ret)]
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
#[tracing::instrument(level = "debug", ret)]
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
#[tracing::instrument(level = "debug", ret)]
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
    /// whisper.cpp, compiled into this binary ([`super::whisper`]). Has no command:
    /// [`build_command`](Transcriber::build_command) returns `None` and
    /// [`super::dictation`] calls the library directly.
    Native,
    /// The Python `whisper` CLI, which fetches its own weights on first use.
    Whisper,
    /// whisper.cpp's `whisper-cli`, which needs a model file passed to `-m`.
    WhisperCpp { model: String },
    /// Nothing on this machine can transcribe. No longer produced by
    /// [`detect_transcriber`] — [`Transcriber::Native`] always can — but kept because it
    /// is the honest answer for a caller that constructs one by hand.
    None,
}

impl Transcriber {
    #[tracing::instrument(level = "debug", ret)]
    pub fn name(&self) -> &'static str {
        match self {
            Transcriber::Custom(_) => "custom",
            Transcriber::Native => "native",
            Transcriber::Whisper => "whisper",
            Transcriber::WhisperCpp { .. } => "whisper-cli",
            Transcriber::None => "none",
        }
    }

    #[tracing::instrument(level = "debug", ret)]
    pub fn build_command(&self, wav: &Path) -> Option<Command> {
        let wav_s = wav.to_string_lossy().to_string();
        match self {
            Transcriber::Custom(argv) => custom_command(argv, WAV_PLACEHOLDER, &wav_s),
            // In-process: there is no subprocess to describe.
            Transcriber::Native => None,
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

/// Pick a transcriber. A configured template wins; then the in-process engine if its
/// model is already downloaded; then an installed `whisper`, preferred over `whisper-cli`
/// because it needs no model path — and `whisper-cli` only when a model was actually
/// configured, since without one it exits before reading the audio. Failing all of those,
/// the in-process engine again, this time fetching its model.
///
/// The in-process engine appears twice on purpose. It is the only one guaranteed to be
/// here, so it must be the floor; but a cached model beats an external tool (same engine,
/// no process, no PATH lookup) while an *uncached* one does not — someone who installed
/// `whisper` deliberately should not be made to wait on a 142 MB download they have no
/// use for. Both arms are cheap and synchronous: neither touches the network, so `/state`
/// can ask this on every poll.
#[tracing::instrument(level = "debug", ret)]
pub fn detect_transcriber(settings: &SttSettings) -> Transcriber {
    if let Some(argv) = settings.transcribe_template.as_ref() {
        if !argv.is_empty() {
            return Transcriber::Custom(argv.clone());
        }
    }
    if super::whisper::ready(settings) {
        return Transcriber::Native;
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
    Transcriber::Native
}

/// Strip a transcriber's decoration down to the words that were said.
///
/// Whisper prints `[00:00.000 --> 00:02.000]  text` per segment, and both builds emit
/// bracketed non-speech markers (`[BLANK_AUDIO]`, `[Music]`) that must never be typed
/// into a pane as if the human had said them.
#[tracing::instrument(level = "debug", ret)]
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
#[tracing::instrument(level = "debug", ret)]
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
#[tracing::instrument(level = "debug", ret)]
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
#[tracing::instrument(level = "debug", ret)]
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
#[tracing::instrument(level = "debug", ret)]
fn on_path(cmd: &str) -> bool {
    crate::speech::engine::on_path(cmd)
}

/// Spawn a tool through its resolved absolute path — see
/// [`crate::speech::engine::command_for`]. A GUI launched from the Dock has no Homebrew
/// on `PATH`, so a bare `Command::new("ffmpeg")` fails on a machine that plainly has it.
#[tracing::instrument(level = "debug", ret)]
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
        // Both of these leave `model` unusable, which is the whole point: `whisper-cli`
        // must not be chosen when the one argument it cannot start without is missing.
        for s in [
            SttSettings::default(),
            SttSettings {
                model: Some("".into()),
                ..Default::default()
            },
        ] {
            assert!(!matches!(
                detect_transcriber(&s),
                Transcriber::WhisperCpp { .. }
            ));
        }
    }

    #[test]
    fn there_is_always_a_transcriber_now() {
        // The regression this whole module exists to prevent: a machine with nothing
        // installed used to get `None`, i.e. a mic button that records and then reports
        // "no transcriber found". Whatever else detection decides, it never decides that.
        assert_ne!(
            detect_transcriber(&SttSettings::default()),
            Transcriber::None
        );
    }

    #[test]
    fn the_in_process_transcriber_has_no_command_to_run() {
        assert_eq!(Transcriber::Native.name(), "native");
        assert!(Transcriber::Native
            .build_command(Path::new("/tmp/a.wav"))
            .is_none());
    }

    #[test]
    fn a_custom_template_still_outranks_the_built_in_engine() {
        // Even when the built-in model is sitting in the cache — an override that the
        // batteries-included path could silently win against would be no override.
        let s = SttSettings {
            transcribe_template: Some(vec!["stt".into(), "{wav}".into()]),
            ..Default::default()
        };
        assert!(matches!(detect_transcriber(&s), Transcriber::Custom(_)));
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
