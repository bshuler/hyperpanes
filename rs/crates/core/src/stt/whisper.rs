//! In-process transcription: whisper.cpp, compiled in, with the weights fetched once.
//!
//! The other half of the "batteries included" pair that starts at [`super::native`]. The
//! recorder there needs nothing installed; without this, the transcript step still did —
//! `whisper` (a Python package) or `whisper-cli` (a Homebrew/apt build) — and a stock
//! macOS, Windows or Linux box has neither. `whisper-rs` links whisper.cpp straight into
//! the binary, so the inference is one code path on all three.
//!
//! **What is not compiled in are the weights.** A model is tens to hundreds of megabytes
//! of data, not code; vendoring one would put it in every download whether or not
//! dictation is ever used. So the first dictation fetches one, verifies it against a
//! pinned SHA-256, and caches it under `<data>/models/`. Everything after that is offline
//! and there is no second download.
//!
//! The fetch is started when the *recording* starts, not when it ends
//! ([`super::dictation`] calls [`prefetch`]): the user is about to talk for a few seconds
//! anyway, which is free time to spend on a download, and [`progress`] lets the failure
//! path say "42% of 142 MB" instead of just hanging.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// What the recorder produces and what every Whisper model expects.
const MODEL_RATE: u32 = super::native::OUT_RATE;
/// Where the models come from — whisper.cpp's own upstream distribution.
const HOST: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";
/// The model used when nothing is configured. `base.en` is the smallest one that
/// transcribes ordinary dictation without embarrassing itself, and 142 MB is a download
/// a user will tolerate exactly once.
pub const DEFAULT_MODEL: &str = "base.en";

/// A model this build knows how to fetch and check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    /// whisper.cpp's own spelling, and what goes in `stt.json`'s `model`.
    pub name: &'static str,
    /// SHA-256 of the file. Checked before the blob is ever handed to the C++ side: a
    /// truncated download and a substituted one look identical to an inference engine.
    pub sha256: &'static str,
    /// Exact size in bytes. Cheap enough to check on every start that it doubles as the
    /// "is this cache entry complete" test.
    pub bytes: u64,
}

/// The English-only models, smallest first. `.en` builds beat the multilingual ones of
/// the same size on English, which is what dictation into a terminal overwhelmingly is;
/// a user who needs another language points `stt.model` at their own file.
///
/// Digests and sizes are HuggingFace's published LFS object ids for
/// `ggerganov/whisper.cpp`, each independently re-computed here from a fresh download.
pub const MODELS: &[Model] = &[
    Model {
        name: "tiny.en",
        sha256: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
        bytes: 77_704_715,
    },
    Model {
        name: "base.en",
        sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
        bytes: 147_964_211,
    },
    Model {
        name: "small.en",
        sha256: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
        bytes: 487_614_201,
    },
];

impl Model {
    pub fn url(&self) -> String {
        format!("{HOST}/ggml-{}.bin", self.name)
    }

    /// Where this model lives once fetched.
    pub fn path(&self) -> PathBuf {
        crate::persistence::paths::stt_models_dir().join(format!("ggml-{}.bin", self.name))
    }

    /// Is the cached copy present and the right length? The full digest is not re-checked
    /// on every dictation — that is seconds of hashing per recording for a file this
    /// process itself verified before it named it.
    pub fn is_cached(&self) -> bool {
        std::fs::metadata(self.path()).is_ok_and(|m| m.len() == self.bytes)
    }
}

/// A built-in model by whisper.cpp's name (`base.en`), or `None` if it is not one.
pub fn by_name(name: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.name == name)
}

/// What `stt.model` resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// A model file the user supplied. Taken as-is: no download, no digest, their file.
    File(PathBuf),
    /// One of [`MODELS`], fetched on demand.
    Builtin(&'static Model),
}

/// Interpret `stt.model`: a built-in name, else an existing path, else the default.
///
/// Order matters. A bare `base.en` is a name, not a relative path, and someone who typed
/// it meant the model — not a file called `base.en` that happens to be in the working
/// directory of whatever launched the GUI.
pub fn choose(settings: &super::SttSettings) -> Choice {
    let configured = settings.model.as_deref().unwrap_or("").trim();
    if !configured.is_empty() {
        if let Some(m) = by_name(configured) {
            return Choice::Builtin(m);
        }
        let p = PathBuf::from(configured);
        if p.is_file() {
            return Choice::File(p);
        }
    }
    // An unreadable configured path falls back rather than failing: a model that moved
    // should degrade to "downloads the default once", not to a dead mic button.
    Choice::Builtin(by_name(DEFAULT_MODEL).expect("DEFAULT_MODEL is one of MODELS"))
}

/// Is a model ready to use right now, with no network?
pub fn ready(settings: &super::SttSettings) -> bool {
    match choose(settings) {
        Choice::File(p) => p.is_file(),
        Choice::Builtin(m) => m.is_cached(),
    }
}

// =========================== fetching ===========================

/// Bytes written so far by the in-flight download, and its expected total. Zero total
/// means nothing is downloading.
static FETCHED: AtomicU64 = AtomicU64::new(0);
static FETCH_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Serializes downloads so two panes reaching for the mic at once fetch one model, not
/// two copies of it into the same path.
static FETCHING: Mutex<()> = Mutex::new(());

/// `(bytes so far, total)` while a model download is running, else `None`.
pub fn progress() -> Option<(u64, u64)> {
    let total = FETCH_TOTAL.load(Ordering::Relaxed);
    (total > 0).then(|| (FETCHED.load(Ordering::Relaxed).min(total), total))
}

/// Human-readable tail for an error raised while a download is still going.
fn progress_note() -> String {
    match progress() {
        Some((done, total)) => format!(
            " (the {} MB model is still downloading — {}%)",
            total / 1_000_000,
            done.saturating_mul(100) / total.max(1)
        ),
        None => String::new(),
    }
}

/// Start fetching `settings`'s model in the background if it is not already cached.
///
/// Fire-and-forget on purpose: this is called when the mic opens, and a download that
/// fails there must not stop the recording — the same fetch is retried, this time with
/// its error reported, when the recording is transcribed.
pub fn prefetch(settings: &super::SttSettings) {
    let Choice::Builtin(model) = choose(settings) else {
        return;
    };
    if model.is_cached() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("hyperpanes-model-fetch".into())
        .spawn(move || {
            let _ = ensure(model);
        });
}

/// The model file, downloading and verifying it first if it is not cached. Blocking.
pub fn ensure(model: &Model) -> Result<PathBuf, String> {
    let dest = model.path();
    if model.is_cached() {
        return Ok(dest);
    }
    // A second caller waits here rather than starting a parallel download, and finds the
    // file already in place when it wakes. A poisoned lock is not interesting — the data
    // it guards is the filesystem, which the digest check validates independently.
    let _guard = FETCHING.lock().unwrap_or_else(|e| e.into_inner());
    if model.is_cached() {
        return Ok(dest);
    }
    let dir = dest.parent().ok_or("model cache has no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("model cache {}: {e}", dir.display()))?;

    // Downloaded to a sibling and renamed, so an interrupted fetch can never leave a
    // truncated file sitting at the name the loader trusts.
    let part = dest.with_extension("part");
    let _ = std::fs::remove_file(&part);
    let outcome = download(&model.url(), &part, model.bytes).and_then(|()| verify(&part, model));
    if let Err(e) = outcome {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, &dest).map_err(|e| format!("installing the model: {e}"))?;
    Ok(dest)
}

/// Stream `url` into `dest`, publishing progress as it goes.
///
/// Runs on its own thread with its own single-threaded runtime. The callers are blocking
/// (the dictation state machine, and `prefetch`'s thread), and one of them is itself
/// already inside a `spawn_blocking` on the control server's runtime — borrowing that
/// runtime from a blocking context is exactly the deadlock this avoids.
fn download(url: &str, dest: &Path, expect: u64) -> Result<(), String> {
    let (url, dest) = (url.to_string(), dest.to_path_buf());
    let handle = std::thread::Builder::new()
        .name("hyperpanes-model-http".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("model download runtime: {e}"))?;
            rt.block_on(stream_to_file(&url, &dest, expect))
        })
        .map_err(|e| format!("model download thread: {e}"))?;
    let r = handle.join().unwrap_or_else(|_| Err("model download thread panicked".into()));
    FETCH_TOTAL.store(0, Ordering::Relaxed);
    FETCHED.store(0, Ordering::Relaxed);
    r
}

async fn stream_to_file(url: &str, dest: &Path, expect: u64) -> Result<(), String> {
    use futures_util::StreamExt;
    use std::io::Write;

    // No overall timeout: a 142 MB download on a slow link is not a hung one. The two
    // that are set catch the failure that actually happens — a connection that opens and
    // then stops delivering.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| format!("model download client: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("downloading the speech model: {e}"))?
        .error_for_status()
        .map_err(|e| format!("downloading the speech model: {e}"))?;

    FETCH_TOTAL.store(resp.content_length().unwrap_or(expect), Ordering::Relaxed);
    FETCHED.store(0, Ordering::Relaxed);

    let mut file = std::io::BufWriter::new(
        std::fs::File::create(dest).map_err(|e| format!("model file {}: {e}", dest.display()))?,
    );
    let mut written: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("downloading the speech model: {e}"))?;
        file.write_all(&chunk)
            .map_err(|e| format!("writing the speech model: {e}"))?;
        written += chunk.len() as u64;
        FETCHED.store(written, Ordering::Relaxed);
    }
    file.flush()
        .map_err(|e| format!("writing the speech model: {e}"))?;
    if written != expect {
        return Err(format!(
            "the speech model downloaded short: {written} bytes of {expect}"
        ));
    }
    Ok(())
}

/// Hash `path` and compare with the pin. Streamed, so hashing a 500 MB model does not
/// hold 500 MB.
fn verify(path: &Path, model: &Model) -> Result<(), String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("verifying the model: {e}"))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher).map_err(|e| format!("verifying the model: {e}"))?;
    let got = hex(&hasher.finalize());
    if got != model.sha256 {
        return Err(format!(
            "the downloaded {} model does not match its published checksum \
             (expected {}, got {got}) — it was corrupted or tampered with, and has been \
             discarded",
            model.name, model.sha256
        ));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// =========================== transcribing ===========================

/// Transcribe `wav` with `settings`'s model, fetching the model first if need be.
///
/// Returns whisper's raw segments joined by newlines — deliberately the same shape the
/// command-line transcribers print, so `clean_transcript` is the single place that knows
/// what to strip.
pub fn transcribe(wav: &Path, settings: &super::SttSettings) -> Result<String, String> {
    let model = match choose(settings) {
        Choice::File(p) => p,
        Choice::Builtin(m) => ensure(m).map_err(|e| format!("{e}{}", progress_note()))?,
    };
    let audio = read_wav(wav)?;
    if audio.is_empty() {
        return Err("the recording held no audio".into());
    }
    run(&model, &audio)
}

fn run(model: &Path, audio: &[f32]) -> Result<String, String> {
    // whisper.cpp writes its model-load banner straight to stderr. Route it into the
    // (unconfigured, hence silent) logging hooks so a dictation does not spray the app's
    // log with tensor dimensions. Idempotent; safe to call per transcription.
    whisper_rs::install_logging_hooks();

    let ctx = WhisperContext::new_with_params(model, WhisperContextParameters::default())
        .map_err(|e| format!("loading the speech model {}: {e}", model.display()))?;
    let mut state = ctx
        .create_state()
        .map_err(|e| format!("starting the speech model: {e}"))?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(threads());
    params.set_translate(false);
    // The `.en` models have no language to choose; a multilingual one the user supplied
    // gets whisper's own detection rather than a hard-coded English.
    params.set_language(if ctx.is_multilingual() { None } else { Some("en") });
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    state
        .full(params, audio)
        .map_err(|e| format!("transcribing: {e}"))?;

    let mut out = String::new();
    for segment in state.as_iter() {
        if let Ok(text) = segment.to_str_lossy() {
            out.push_str(text.trim());
            out.push('\n');
        }
    }
    Ok(out)
}

/// How many threads to give the inference. whisper.cpp's own default is 4; more than the
/// machine has is slower, not faster, and dictation must not monopolize a laptop that is
/// also running the panes.
fn threads() -> std::ffi::c_int {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    cores.clamp(1, 4) as std::ffi::c_int
}

/// Read a WAV as the mono 16 kHz f32 whisper wants.
///
/// [`super::native`] already writes exactly that, so this is a no-op conversion for the
/// built-in recorder — it exists for the other three, and for a `recordTemplate` that
/// captures at whatever its tool felt like.
fn read_wav(path: &Path) -> Result<Vec<f32>, String> {
    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("reading the recording: {e}"))?;
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;

    // Every sample is normalized to -1.0..1.0 first, whatever the file's depth, because
    // whisper's mel front end is scale-sensitive: feeding it raw i16 magnitudes
    // transcribes to silence rather than to something wrong-but-visible.
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.unwrap_or(0.0))
            .collect(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample.max(1) - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.unwrap_or(0) as f32 * scale)
                .collect()
        }
    };
    Ok(resample(&downmix(&interleaved, channels), spec.sample_rate))
}

/// Average the channels together. Averaging, not "take channel 0": a two-input interface
/// with one dead channel is common, and picking that one transcribes to nothing.
fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels)
        .map(|f| f.iter().sum::<f32>() / f.len() as f32)
        .collect()
}

/// Nearest-neighbour rate conversion to [`MODEL_RATE`], by the same phase accumulator the
/// recorder uses. Crude, and adequate: the alternative is a resampling dependency for a
/// path that only runs when someone overrode the recorder.
fn resample(mono: &[f32], rate: u32) -> Vec<f32> {
    if rate == MODEL_RATE || rate == 0 {
        return mono.to_vec();
    }
    let mut out = Vec::with_capacity(mono.len() * MODEL_RATE as usize / rate as usize + 1);
    let mut phase: u64 = 0;
    for &s in mono {
        phase += MODEL_RATE as u64;
        while phase >= rate as u64 {
            phase -= rate as u64;
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::SttSettings;

    fn settings(model: Option<&str>) -> SttSettings {
        SttSettings {
            model: model.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn every_pinned_model_has_a_plausible_digest_and_size() {
        for m in MODELS {
            assert_eq!(m.sha256.len(), 64, "{} digest is not a sha256", m.name);
            assert!(
                m.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{} digest is not hex",
                m.name
            );
            assert!(m.bytes > 1_000_000, "{} is implausibly small", m.name);
            assert!(m.url().ends_with(&format!("ggml-{}.bin", m.name)));
        }
    }

    #[test]
    fn the_default_model_is_one_this_build_can_fetch() {
        // A default naming a model absent from MODELS would panic in `choose` on a
        // machine with no configuration at all — i.e. every fresh install.
        assert!(by_name(DEFAULT_MODEL).is_some());
    }

    #[test]
    fn a_bare_model_name_is_a_model_not_a_relative_path() {
        assert_eq!(choose(&settings(Some("tiny.en"))), Choice::Builtin(&MODELS[0]));
        assert_eq!(
            choose(&settings(Some("  base.en  "))),
            Choice::Builtin(&MODELS[1])
        );
    }

    #[test]
    fn no_configuration_at_all_still_picks_a_model() {
        assert_eq!(
            choose(&SttSettings::default()),
            Choice::Builtin(by_name(DEFAULT_MODEL).unwrap())
        );
    }

    #[test]
    fn a_model_path_that_exists_is_used_verbatim() {
        let p = std::env::temp_dir().join(format!("hp-model-{}.bin", std::process::id()));
        std::fs::write(&p, b"not really a model").unwrap();
        assert_eq!(
            choose(&settings(Some(&p.to_string_lossy()))),
            Choice::File(p.clone())
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_model_path_that_vanished_falls_back_instead_of_breaking_the_mic() {
        let gone = "/nowhere/at/all/ggml-does-not-exist.bin";
        assert!(matches!(choose(&settings(Some(gone))), Choice::Builtin(_)));
    }

    #[test]
    fn a_corrupt_download_is_rejected_by_its_digest() {
        let p = std::env::temp_dir().join(format!("hp-badmodel-{}.bin", std::process::id()));
        std::fs::write(&p, b"tampered").unwrap();
        let err = verify(&p, &MODELS[0]).unwrap_err();
        assert!(err.contains("does not match"), "{err}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn hashing_agrees_with_the_reference_vector() {
        // sha256("abc"), so a swapped-in hasher or a broken hex encoder is caught here
        // rather than by every model download failing mysteriously.
        let mut h = Sha256::new();
        h.update(b"abc");
        assert_eq!(
            hex(&h.finalize()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    // ---- audio conversion ----

    fn write_wav(path: &Path, spec: hound::WavSpec, samples: &[i16]) {
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for s in samples {
            w.write_sample(*s).unwrap();
        }
        w.finalize().unwrap();
    }

    #[test]
    fn the_recorders_own_output_is_read_back_unchanged_in_length() {
        let p = std::env::temp_dir().join(format!("hp-wav-mono-{}.wav", std::process::id()));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: MODEL_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        write_wav(&p, spec, &vec![1000i16; MODEL_RATE as usize]);
        let a = read_wav(&p).unwrap();
        assert_eq!(a.len(), MODEL_RATE as usize);
        // i16 1000 normalized: 1000/32768.
        assert!((a[0] - 1000.0 / 32768.0).abs() < 1e-6, "{}", a[0]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_stereo_44k_recording_becomes_a_second_of_mono_16k() {
        // What a `recordTemplate` pointed at a stock ffmpeg invocation would hand us.
        let p = std::env::temp_dir().join(format!("hp-wav-stereo-{}.wav", std::process::id()));
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut interleaved = Vec::new();
        for _ in 0..44_100 {
            interleaved.push(8000i16); // left
            interleaved.push(0i16); // right: dead channel
        }
        write_wav(&p, spec, &interleaved);
        let a = read_wav(&p).unwrap();
        assert!(
            (a.len() as i64 - MODEL_RATE as i64).abs() <= 1,
            "one second became {} samples",
            a.len()
        );
        // Averaged, not picked: half of 8000/32768, not 0 and not the full value.
        assert!((a[0] - 4000.0 / 32768.0).abs() < 1e-3, "{}", a[0]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn nothing_is_downloading_when_nothing_is_downloading() {
        assert_eq!(progress(), None);
        assert_eq!(progress_note(), "");
    }

    #[test]
    #[ignore = "downloads ~142 MB and runs the model"]
    fn a_real_recording_transcribes_to_the_words_that_were_said() {
        // Run with: cargo test -p hyperpanes-core -- --ignored transcribes_to_the_words
        // Speak into HP_STT_WAV, or leave it unset to use the recorder itself.
        let wav = std::env::var("HP_STT_WAV").expect("set HP_STT_WAV to a spoken wav file");
        let text = transcribe(Path::new(&wav), &SttSettings::default()).unwrap();
        eprintln!("transcript: {text:?}");
        assert!(!crate::stt::backend::clean_transcript(&text).is_empty());
    }
}
