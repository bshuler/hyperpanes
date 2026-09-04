//! The per-pane dictation state machine: at most one recording per pane, stopped
//! gracefully, transcribed, and handed back as text for the caller to type into the pane.
//!
//! Deliberately blocking and free of any runtime: `stop` waits for a recorder to finalize
//! its WAV and then for a transcriber to read it, which on a cold Whisper model is
//! seconds. Callers run it on a worker thread — the control server hands it to
//! `tokio::task::spawn_blocking` — so nothing about this file has to be async to be
//! correct.

use super::archive;
use super::backend::{
    clean_transcript, detect_recorder, detect_transcriber, Recorder, StopKind, Transcriber,
};
use super::native::{self, NativeCapture};
use super::SttSettings;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a stopped recorder is given to finalize its file before it is killed. Well
/// past what ffmpeg needs to rewrite a WAV header; short enough that a wedged recorder
/// does not hang the pane's mic button.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// Poll interval while waiting for the recorder to exit.
const POLL: Duration = Duration::from_millis(20);
/// Hard ceiling on one recording. A mic left on by accident should stop itself rather
/// than fill a disk. The external recorders take it as an argument; the in-process one
/// takes it here, so the cap survives a GUI that crashes without ever sending a stop.
const MAX_RECORD: Duration = Duration::from_secs(300);
/// A WAV smaller than this is a header and nothing else — the mic was released before it
/// captured anything, which is a "say something" message, not a transcriber's problem.
const MIN_WAV_BYTES: u64 = 2048;

/// Whatever is currently holding the microphone.
///
/// Two shapes, because there are two kinds of recorder: one that is a process to be
/// stopped and reaped, and one that is a thread inside this process to be signalled and
/// joined. Everything downstream of the mic button is identical either way — that is the
/// point of putting the split here and nowhere else.
enum Capture {
    /// In-process capture ([`super::native`]).
    Native(NativeCapture),
    /// A spawned recorder: `ffmpeg`, `rec`, `arecord`, or a user template.
    Process { child: Child, stop: StopKind },
}

/// A recording in progress.
struct Recording {
    capture: Capture,
    wav: PathBuf,
    started: Instant,
}

/// Every pane's dictation state. One instance per process, shared behind an `Arc`.
pub struct Dictation {
    /// Pane id → its in-flight recording. A pane not present here is not recording.
    live: Mutex<HashMap<String, Recording>>,
    /// Where a recording is captured to. Scratch, and per-recording: see [`Self::start`].
    dir: PathBuf,
    /// Where a finished recording and its transcript are kept, by [`super::archive`].
    archive: PathBuf,
}

/// Distinguishes one recording's scratch file from the next. See [`Dictation::start`] for
/// why a per-pane name was not enough.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// What a finished dictation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub text: String,
    /// Which transcriber produced it, for the result the control API reports back.
    pub backend: &'static str,
    /// Where the recording and this text were kept, so the caller can point the user at
    /// their own words. `None` only when the archive could not be written.
    pub kept: Option<PathBuf>,
}

impl Dictation {
    /// Capture to `dir`, and keep finished recordings in `dir/kept`.
    #[tracing::instrument(level = "debug")]
    pub fn new(dir: PathBuf) -> Self {
        let archive = dir.join("kept");
        Self::new_with_archive(dir, archive)
    }

    /// Capture to `dir`, but keep finished recordings somewhere else — which is what the
    /// real service does, because `dir` is a temporary directory that the OS is entitled
    /// to empty and the whole point of the archive is that it survives.
    #[tracing::instrument(level = "debug")]
    pub fn new_with_archive(dir: PathBuf, archive: PathBuf) -> Self {
        Self {
            live: Mutex::new(HashMap::new()),
            dir,
            archive,
        }
    }

    /// Where finished recordings and their transcripts are kept.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn archive_dir(&self) -> &Path {
        &self.archive
    }

    /// Panes currently recording, for the read-model's mic indicator.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn recording_panes(&self) -> Vec<String> {
        let mut v: Vec<String> = self.live.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn is_recording(&self, pane_id: &str) -> bool {
        self.live.lock().unwrap().contains_key(pane_id)
    }

    /// Begin capturing audio for `pane_id`. Returns the recorder's name.
    ///
    /// Starting a pane that is already recording is a no-op rather than an error: the mic
    /// button is a toggle, and two clicks racing must not leave an orphaned recorder
    /// nobody can stop.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn start(&self, pane_id: &str, settings: &SttSettings) -> Result<&'static str, String> {
        let mut live = self.live.lock().unwrap();
        if live.contains_key(pane_id) {
            return Ok("already-recording");
        }
        let recorder = detect_recorder(settings);
        if recorder == Recorder::None {
            // In-process capture needs nothing installed, so reaching here means the
            // machine has no usable microphone at all — not that something is missing
            // from PATH. Say the thing that is actually true.
            return Err(format!(
                "no microphone found: this machine reports no usable audio input \
                 device, and none of {} is installed either. Plug in or enable a \
                 microphone, or set stt.recordTemplate to a command that captures \
                 to {{wav}}.",
                if cfg!(target_os = "linux") {
                    "ffmpeg, rec, arecord"
                } else {
                    "ffmpeg, rec"
                }
            ));
        }
        // Started here, while the mic is opening, rather than at transcribe time: the
        // model download and the person talking then overlap, so the first-ever dictation
        // costs a few seconds of wait instead of the full 142 MB of it. A no-op once the
        // model is cached, and a failure here is deliberately ignored — the recording must
        // not be blocked by it, and the same fetch is retried, with its error reported,
        // when there is finally a transcript to make.
        super::whisper::prefetch(settings);

        std::fs::create_dir_all(&self.dir).map_err(|e| format!("dictation dir: {e}"))?;
        // Per *recording*, not per pane. A fixed per-pane path plus the `remove_file`
        // that used to sit here is a way to lose most of a dictation: unlink leaves the
        // running recorder writing happily to an inode that no longer has a name, the
        // next recorder creates a fresh file at the same path, and the transcriber reads
        // whichever one it finds — the tail of the recording rather than the whole of it,
        // with nothing anywhere saying so. A name nothing else can claim makes that
        // impossible rather than unlikely.
        let wav = self.dir.join(format!(
            "{}-{}.wav",
            sanitize(pane_id),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));

        let capture = if recorder == Recorder::Native {
            Capture::Native(native::start(&wav, MAX_RECORD)?)
        } else {
            let mut cmd = recorder
                .build_command(&wav)
                .ok_or_else(|| "recorder has no command".to_string())?;
            // stdin stays open and piped for every recorder, not just ffmpeg: it is how
            // the graceful stop is delivered, and a recorder that inherits the app's
            // stdin can steal keystrokes from whatever launched it.
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let child = cmd
                .spawn()
                .map_err(|e| format!("{} failed to start: {e}", recorder.name()))?;
            Capture::Process {
                child,
                stop: recorder.stop_kind(),
            }
        };

        live.insert(
            pane_id.to_string(),
            Recording {
                capture,
                wav,
                started: Instant::now(),
            },
        );
        Ok(recorder.name())
    }

    /// Stop `pane_id`'s recording and transcribe it. Blocking.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn stop(&self, pane_id: &str, settings: &SttSettings) -> Result<Transcript, String> {
        let Some(rec) = self.live.lock().unwrap().remove(pane_id) else {
            return Err(format!("pane is not recording: {pane_id}"));
        };
        let wav = rec.wav.clone();
        let elapsed = rec.started.elapsed();
        finish_recording(rec);

        let result = transcribe(&wav, elapsed, settings);

        // The one line that says which half of this pipeline is at fault. Held recording
        // time against captured audio time answers "did the microphone hear all of it";
        // captured audio against transcript length answers "did the model transcribe all
        // of it"; and the caller logs what it managed to type. Cheap, and worth more than
        // any amount of after-the-fact reasoning about a recording nobody kept.
        let (bytes, seconds) = wav_size(&wav);
        match &result {
            Ok(t) => tracing::info!(
                pane = %pane_id,
                held_s = elapsed.as_secs_f32(),
                audio_s = seconds,
                wav_bytes = bytes,
                chars = t.text.len(),
                backend = t.backend,
                "dictation transcribed"
            ),
            Err(e) => tracing::warn!(
                pane = %pane_id,
                held_s = elapsed.as_secs_f32(),
                audio_s = seconds,
                wav_bytes = bytes,
                error = %e,
                "dictation produced no transcript"
            ),
        }

        // Both halves are kept, and a failure is kept hardest: a recording that produced
        // nothing is the one someone will actually want to listen to.
        let outcome = match &result {
            Ok(t) => Ok(t.text.as_str()),
            Err(e) => Err(e.as_str()),
        };
        let kept = archive::keep(&self.archive, &wav, &sanitize(pane_id), outcome);
        if let Some(k) = &kept {
            tracing::info!(wav = %k.wav.display(), text = %k.text.display(), "dictation kept");
        }

        match result {
            Ok(t) => Ok(Transcript {
                kept: kept.map(|k| k.text),
                ..t
            }),
            // The error is what the user sees, so it is where the path has to go — being
            // told the words are gone is very different from being told where they are.
            Err(e) => Err(match kept {
                Some(k) => format!("{e} — the recording was kept at {}", k.wav.display()),
                None => e,
            }),
        }
    }

    /// Throw away `pane_id`'s recording without transcribing it — the pane closed, or the
    /// user cancelled. Never errors: cancelling something that is not running is the
    /// state the caller wanted.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn cancel(&self, pane_id: &str) {
        let Some(rec) = self.live.lock().unwrap().remove(pane_id) else {
            return;
        };
        let wav = rec.wav.clone();
        finish_recording(rec);
        // Kept too. A cancel is usually a closing pane, but it is also what happens when
        // the app goes down mid-sentence, and that recording is not the app's to discard.
        archive::keep(
            &self.archive,
            &wav,
            &sanitize(pane_id),
            Err("cancelled before transcription"),
        );
    }

    /// Cancel every recording — process shutdown, so no recorder outlives the app.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn cancel_all(&self) {
        let ids = self.recording_panes();
        for id in ids {
            self.cancel(&id);
        }
    }
}

/// Ask a recorder to finalize its file, then make sure it is gone.
///
/// A WAV's header carries the length of the audio that follows it, so a recorder killed
/// mid-write leaves a file no decoder will open. Every recorder is therefore asked
/// politely first — `q` on stdin for ffmpeg, SIGINT for the rest — and killed only after
/// [`STOP_GRACE`] proves it is not going to exit on its own.
#[tracing::instrument(level = "debug", ret, skip(rec))]
fn finish_recording(rec: Recording) {
    let mut child = match rec.capture {
        // The in-process recorder finalizes its own header on the way out, and `finish`
        // does not return until it has. No grace period to wait out, nothing to kill.
        Capture::Native(cap) => {
            let _ = cap.finish();
            return;
        }
        Capture::Process { child, stop } => {
            let mut child = child;
            match stop {
                StopKind::FfmpegQuit => {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(b"q\n");
                        let _ = stdin.flush();
                    }
                }
                StopKind::Interrupt => {
                    // Closing stdin first: a recorder reading from it exits on EOF, which
                    // covers custom shell-script recorders with no SIGINT handler.
                    drop(child.stdin.take());
                    interrupt(&child);
                }
            }
            child
        }
    };
    let deadline = Instant::now() + STOP_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            // Killed, or unwaitable: either way stop waiting on it.
            _ => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Send SIGINT to `child`. No-op off Unix, where the graceful stop is stdin-based.
#[tracing::instrument(level = "debug", ret)]
fn interrupt(child: &Child) {
    #[cfg(unix)]
    {
        // Safety: `kill(2)` with a pid this process owns and a signal number; the child
        // is still un-reaped here, so the pid cannot have been recycled.
        unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    }
    #[cfg(not(unix))]
    {
        let _ = child;
    }
}

/// Run the configured transcriber over `wav`.
#[tracing::instrument(level = "debug", ret)]
fn transcribe(wav: &Path, elapsed: Duration, settings: &SttSettings) -> Result<Transcript, String> {
    let size = std::fs::metadata(wav).map(|m| m.len()).unwrap_or(0);
    if size < MIN_WAV_BYTES {
        return Err(format!(
            "nothing was recorded ({} ms of audio) — check the microphone permission",
            elapsed.as_millis()
        ));
    }
    let transcriber = detect_transcriber(settings);
    let backend = transcriber.name();
    if transcriber == Transcriber::Native {
        // No process, no PATH, no stdout to parse — but the output still goes through
        // `clean_transcript`, because whisper.cpp emits the same `[BLANK_AUDIO]` markers
        // whichever way it is driven.
        let raw = super::whisper::transcribe(wav, settings)?;
        let text = clean_transcript(&raw);
        if text.is_empty() {
            return Err("no speech in the recording".to_string());
        }
        return Ok(Transcript {
            text,
            backend,
            kept: None,
        });
    }
    // Unreachable from detection now that the in-process engine is the floor; what is
    // left is a `transcribeTemplate` that resolved to no runnable command.
    let mut cmd = transcriber.build_command(wav).ok_or_else(|| {
        "stt.transcribeTemplate is not a command that can be run — fix it, or remove it to          use the built-in transcriber"
            .to_string()
    })?;
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{backend} failed to start: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let detail = err.lines().last().unwrap_or("").trim();
        return Err(format!("{backend} failed: {detail}"));
    }
    let text = clean_transcript(&String::from_utf8_lossy(&out.stdout));
    if text.is_empty() {
        return Err("no speech in the recording".to_string());
    }
    Ok(Transcript {
        text,
        backend,
        kept: None,
    })
}

/// Bytes on disk and seconds of audio in the header.
///
/// The two are reported together because only the pair is diagnostic: bytes alone cannot
/// tell a five-second recording from a five-minute one that was captured at the wrong
/// sample rate. Seconds is `None` for a file `hound` will not open, which is itself the
/// answer when a recorder was killed before it wrote its length back into the header.
#[tracing::instrument(level = "debug", ret)]
fn wav_size(wav: &Path) -> (u64, Option<f32>) {
    let bytes = std::fs::metadata(wav).map(|m| m.len()).unwrap_or(0);
    let seconds = hound::WavReader::open(wav)
        .ok()
        .map(|r| r.duration() as f32 / r.spec().sample_rate.max(1) as f32);
    (bytes, seconds)
}

/// Pane ids come from the control API, so they reach here unvalidated — keep the WAV name
/// to characters that cannot walk out of the dictation directory.
#[tracing::instrument(level = "debug", ret)]
fn sanitize(pane_id: &str) -> String {
    let s: String = pane_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "pane".to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hp-dictation-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A recorder that writes `bytes` of "audio" and then waits to be stopped.
    #[cfg(unix)]
    fn fake_recorder(bytes: usize) -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("head -c {bytes} /dev/zero > \"$1\"; sleep 30",),
            "sh".into(),
            "{wav}".into(),
        ]
    }

    /// End to end over a real recording, with the real transcriber.
    ///
    /// The unit tests above use a recorder that writes zeros and a transcriber that echoes
    /// a fixed string, which proves the plumbing and nothing about the audio. This one
    /// stands in a genuine spoken WAV for the recorder's output and lets whisper.cpp
    /// actually read it, because the failure that motivated the archive — two minutes of
    /// speech arriving as four letters — is invisible to a fake on both ends.
    ///
    /// `HP_STT_WAV` is a mono 16 kHz WAV of someone talking:
    /// `say -f words.txt -o t.aiff && afconvert -f WAVE -d LEI16@16000 -c 1 t.aiff t.wav`
    #[cfg(unix)]
    #[test]
    #[ignore = "needs HP_STT_WAV and runs the real model"]
    fn a_real_recording_survives_the_whole_dictation_path() {
        let src = std::env::var("HP_STT_WAV").expect("set HP_STT_WAV to a spoken wav file");
        let src_seconds = wav_size(Path::new(&src)).1.expect("HP_STT_WAV is not a readable wav");

        let dir = temp_dir("real");
        let d = Dictation::new(dir.clone());
        let s = SttSettings {
            record_template: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("cp {src} \"$1\"; sleep 30"),
                "sh".into(),
                "{wav}".into(),
            ]),
            ..Default::default()
        };
        d.start("p1", &s).unwrap();
        wait_for_audio(&dir, "p1");
        let t = d.stop("p1", &s).unwrap();

        let kept = t.kept.expect("kept");
        let audio = kept.with_extension("wav");
        // The whole recording is kept, not a fragment of it: this is what the user is
        // meant to be able to go back to.
        let kept_seconds = wav_size(&audio).1.expect("kept audio is not a readable wav");
        assert!(
            (kept_seconds - src_seconds).abs() < 0.05,
            "kept {kept_seconds}s of a {src_seconds}s recording"
        );
        // And the transcript is the length of a transcript, not of a stray last word. A
        // hard number rather than "non-empty" is the point: non-empty is exactly what the
        // four-letter failure was.
        assert!(
            t.text.split_whitespace().count() > (src_seconds as usize) / 2,
            "{} words from {src_seconds}s of speech: {:?}",
            t.text.split_whitespace().count(),
            t.text
        );
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), t.text);
        eprintln!(
            "kept {} ({kept_seconds:.1}s) + {} ({} words)",
            audio.display(),
            kept.display(),
            t.text.split_whitespace().count()
        );
    }

    #[cfg(unix)]
    fn echo_transcriber(text: &str) -> Vec<String> {
        vec!["/bin/echo".into(), text.into()]
    }

    /// Block until the fake recorder has actually written its audio.
    ///
    /// `start` returns as soon as the recorder is spawned, which is the right contract —
    /// the mic lights up immediately — but it means a test that stops on the next line
    /// would be measuring the spawn race, not the pipeline.
    #[cfg(unix)]
    /// Wait for the pane's in-flight recording to hold real audio. Found by prefix, not
    /// by an exact name: each recording gets its own numbered file.
    fn wait_for_audio(dir: &Path, pane_id: &str) {
        let prefix = format!("{}-", sanitize(pane_id));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(w) = scratch_wav(dir, &prefix) {
                if std::fs::metadata(&w).map(|m| m.len()).unwrap_or(0) >= MIN_WAV_BYTES {
                    return;
                }
            }
            std::thread::sleep(POLL);
        }
    }

    /// The pane's scratch recording, if one is there.
    fn scratch_wav(dir: &Path, prefix: &str) -> Option<PathBuf> {
        std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.extension().is_some_and(|x| x == "wav")
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
            })
    }

    // ---- name sanitizing ----

    #[test]
    fn a_pane_id_cannot_steer_the_wav_out_of_the_dictation_dir() {
        assert_eq!(sanitize("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize("pane-1_a"), "pane-1_a");
        assert_eq!(sanitize(""), "pane");
    }

    // ---- lifecycle ----

    #[test]
    fn a_pane_that_never_started_cannot_be_stopped() {
        let d = Dictation::new(temp_dir("nostart"));
        assert!(d.stop("p1", &SttSettings::default()).is_err());
        assert!(!d.is_recording("p1"));
    }

    #[test]
    fn cancelling_a_pane_that_is_not_recording_is_silent() {
        let d = Dictation::new(temp_dir("nocancel"));
        d.cancel("p1"); // must not panic
        d.cancel_all();
    }

    #[test]
    fn no_recorder_is_reported_not_papered_over() {
        let d = Dictation::new(temp_dir("norec"));
        let s = SttSettings {
            // A template naming a program that does not exist still resolves to a
            // Custom recorder — the failure surfaces at spawn, with the reason.
            record_template: Some(vec!["hyperpanes-no-such-recorder".into()]),
            ..Default::default()
        };
        let err = d.start("p1", &s).unwrap_err();
        assert!(err.contains("failed to start"), "{err}");
        assert!(
            !d.is_recording("p1"),
            "a failed start leaves no ghost recording"
        );
    }

    #[cfg(unix)]
    #[test]
    fn record_then_stop_yields_the_transcribers_text() {
        let dir = temp_dir("round");
        let d = Dictation::new(dir.clone());
        let s = SttSettings {
            record_template: Some(fake_recorder(8192)),
            transcribe_template: Some(echo_transcriber("[00:00.000 --> 00:01.000]  hello world")),
            ..Default::default()
        };
        assert_eq!(d.start("p1", &s).unwrap(), "custom");
        assert!(d.is_recording("p1"));
        assert_eq!(d.recording_panes(), vec!["p1".to_string()]);
        wait_for_audio(&dir, "p1");

        let t = d.stop("p1", &s).unwrap();
        assert_eq!(t.text, "hello world");
        assert!(!d.is_recording("p1"), "stopping clears the pane");
    }

    #[cfg(unix)]
    #[test]
    fn a_second_start_does_not_orphan_the_first_recorder() {
        let d = Dictation::new(temp_dir("double"));
        let s = SttSettings {
            record_template: Some(fake_recorder(8192)),
            transcribe_template: Some(echo_transcriber("ok")),
            ..Default::default()
        };
        d.start("p1", &s).unwrap();
        assert_eq!(d.start("p1", &s).unwrap(), "already-recording");
        assert_eq!(d.recording_panes().len(), 1);
        d.cancel("p1");
        assert!(!d.is_recording("p1"));
    }

    #[cfg(unix)]
    #[test]
    fn a_recording_with_no_audio_says_so_instead_of_transcribing_it() {
        let d = Dictation::new(temp_dir("silent"));
        let s = SttSettings {
            record_template: Some(fake_recorder(16)),
            transcribe_template: Some(echo_transcriber("phantom words")),
            ..Default::default()
        };
        d.start("p1", &s).unwrap();
        let err = d.stop("p1", &s).unwrap_err();
        assert!(err.contains("nothing was recorded"), "{err}");
        assert!(
            !err.contains("phantom"),
            "an empty capture must never reach the transcriber"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_transcript_of_only_non_speech_is_not_typed_into_the_pane() {
        let dir = temp_dir("blank");
        let d = Dictation::new(dir.clone());
        let s = SttSettings {
            record_template: Some(fake_recorder(8192)),
            transcribe_template: Some(echo_transcriber("[BLANK_AUDIO]")),
            ..Default::default()
        };
        d.start("p1", &s).unwrap();
        wait_for_audio(&dir, "p1");
        let err = d.stop("p1", &s).unwrap_err();
        assert!(err.contains("no speech"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_dictation_leaves_the_recording_and_its_transcript_behind() {
        let dir = temp_dir("keep");
        let d = Dictation::new(dir.clone());
        let s = SttSettings {
            record_template: Some(fake_recorder(8192)),
            transcribe_template: Some(echo_transcriber("done")),
            ..Default::default()
        };
        d.start("p1", &s).unwrap();
        wait_for_audio(&dir, "p1");
        let t = d.stop("p1", &s).unwrap();

        // The scratch copy is moved out, so a recording is never counted twice.
        assert!(
            scratch_wav(&dir, "p1-").is_none(),
            "the scratch recording should have been moved into the archive"
        );
        // Both halves are there, and the transcript is the pasteable one the caller got.
        let kept = t.kept.expect("the transcript should have been kept");
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), t.text);
        assert!(kept.starts_with(d.archive_dir()));
        assert!(
            kept.with_extension("wav").exists(),
            "the audio should be kept too"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_transcription_that_fails_still_keeps_the_audio_and_says_where() {
        let dir = temp_dir("keepfail");
        let d = Dictation::new(dir.clone());
        let s = SttSettings {
            record_template: Some(fake_recorder(8192)),
            transcribe_template: Some(vec!["/bin/sh".into(), "-c".into(), "exit 1".into()]),
            ..Default::default()
        };
        d.start("p1", &s).unwrap();
        wait_for_audio(&dir, "p1");
        let err = d.stop("p1", &s).unwrap_err();

        // Being told the words are gone is very different from being told where they are.
        assert!(
            err.contains("kept at"),
            "error should point at the recording: {err}"
        );
        let kept: Vec<_> = std::fs::read_dir(d.archive_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(kept.len(), 2, "audio and transcript: {kept:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_recording_is_kept_rather_than_thrown_away() {
        let dir = temp_dir("keepcancel");
        let d = Dictation::new(dir.clone());
        let s = SttSettings {
            record_template: Some(fake_recorder(8192)),
            ..Default::default()
        };
        d.start("p1", &s).unwrap();
        wait_for_audio(&dir, "p1");
        d.cancel("p1");

        assert!(scratch_wav(&dir, "p1-").is_none());
        let kept: Vec<_> = std::fs::read_dir(d.archive_dir())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "wav"))
            .collect();
        assert_eq!(kept.len(), 1, "the audio should still be there: {kept:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_second_recording_cannot_overwrite_the_first_ones_audio() {
        // The shape of the bug this guards: a fixed per-pane path meant a restarted
        // recorder wrote over the file the running one had open, and the transcriber read
        // the tail. Two recordings, two names, no overlap.
        let dir = temp_dir("uniq");
        let d = Dictation::new(dir.clone());
        let s = SttSettings {
            record_template: Some(fake_recorder(8192)),
            transcribe_template: Some(echo_transcriber("done")),
            ..Default::default()
        };
        for _ in 0..2 {
            d.start("p1", &s).unwrap();
            wait_for_audio(&dir, "p1");
            d.stop("p1", &s).unwrap();
        }
        let wavs: Vec<_> = std::fs::read_dir(d.archive_dir())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "wav"))
            .collect();
        assert_eq!(
            wavs.len(),
            2,
            "each recording keeps its own audio: {wavs:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_transcriber_reports_its_own_error() {
        let dir = temp_dir("failt");
        let d = Dictation::new(dir.clone());
        let s = SttSettings {
            record_template: Some(fake_recorder(8192)),
            transcribe_template: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo 'model not found' >&2; exit 1".into(),
            ]),
            ..Default::default()
        };
        d.start("p1", &s).unwrap();
        wait_for_audio(&dir, "p1");
        let err = d.stop("p1", &s).unwrap_err();
        assert!(err.contains("model not found"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn two_panes_record_independently() {
        let dir = temp_dir("two");
        let d = Dictation::new(dir.clone());
        let s = SttSettings {
            record_template: Some(fake_recorder(8192)),
            transcribe_template: Some(echo_transcriber("hi")),
            ..Default::default()
        };
        d.start("p1", &s).unwrap();
        d.start("p2", &s).unwrap();
        assert_eq!(
            d.recording_panes(),
            vec!["p1".to_string(), "p2".to_string()]
        );
        wait_for_audio(&dir, "p1");
        d.stop("p1", &s).unwrap();
        assert_eq!(d.recording_panes(), vec!["p2".to_string()]);
        d.cancel_all();
        assert!(d.recording_panes().is_empty());
    }
}
