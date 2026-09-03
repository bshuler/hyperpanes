//! In-process microphone capture: the recorder that needs nothing installed.
//!
//! Every other recorder in [`super::backend`] is a command — `ffmpeg`, `rec`, `arecord` —
//! and a command the user did not install is a mic button that reports "no recorder
//! found". That is what this module exists to stop. `cpal` talks to CoreAudio, WASAPI and
//! ALSA directly, so the mic works on a machine straight out of the box, and `hound`
//! writes the WAV header, which is the one thing a killed `ffmpeg` used to get wrong.
//!
//! The shape is a thread, not a struct with a stream in it, and that is deliberate:
//! `cpal::Stream` is `!Send` on several backends, so it can never leave the thread that
//! built it. The thread builds the stream, plays it, then blocks on a channel until it is
//! told to stop or the cap expires; the caller holds only the sending end and a join
//! handle. Everything the audio callback touches — the writer — is behind a mutex it
//! shares with that thread and nothing else.
//!
//! What comes out is mono 16 kHz signed 16-bit, matching what the external recorders are
//! asked for and what every Whisper build wants, whatever the device itself offers.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Mono 16 kHz: what every Whisper build wants, and what keeps a minute of dictation
/// under 2 MB. Deliberately the same number as [`super::backend`]'s `SAMPLE_RATE`, in the
/// one type each side needs it in.
pub const OUT_RATE: u32 = 16_000;

type Writer = hound::WavWriter<std::io::BufWriter<std::fs::File>>;
/// Shared with the audio callback. `None` once the capture has been finalized, so a
/// callback that lands after the stream is torn down writes nowhere instead of panicking.
type SharedWriter = Arc<Mutex<Option<Writer>>>;

/// Is there a microphone this process can open?
///
/// Deliberately more than "a device exists": a device whose default config cannot be read
/// is one whose stream would fail to build, and finding that out during detection means
/// falling through to `ffmpeg` rather than failing at the moment the user clicks the mic.
#[tracing::instrument(level = "debug", ret)]
pub fn available() -> bool {
    cpal::default_host()
        .default_input_device()
        .is_some_and(|d| d.default_input_config().is_ok())
}

/// A capture running on its own thread.
pub struct NativeCapture {
    /// Send to ask the thread to stop. Dropping it stops the thread too — the recv sees a
    /// disconnect — so a `NativeCapture` that is simply dropped never leaks a live mic.
    stop: Sender<()>,
    join: Option<JoinHandle<Result<(), String>>>,
}

impl NativeCapture {
    /// Stop capturing and wait for the WAV to be finalized.
    ///
    /// Blocking, and it must be: the header carries the data length, so a caller that
    /// returned before this finished would hand a transcriber a file whose header says
    /// zero samples.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn finish(mut self) -> Result<(), String> {
        let _ = self.stop.send(());
        match self.join.take() {
            Some(h) => h
                .join()
                .unwrap_or_else(|_| Err("recorder thread panicked".into())),
            None => Ok(()),
        }
    }
}

impl Drop for NativeCapture {
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn drop(&mut self) {
        // Only reached when `finish` was never called — a cancelled recording, or a
        // panic on the way out. Stop the device anyway; a mic left open outlives the
        // pane that opened it.
        let _ = self.stop.send(());
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

/// Begin capturing to `wav`, stopping on its own after `max`.
///
/// Returns once the stream is actually running, so a caller that gets `Ok` knows the
/// microphone is live — a device that is busy or refused by the OS is an error here, not
/// a silent empty file discovered a minute later.
#[tracing::instrument(level = "debug")]
pub fn start(wav: &Path, max: Duration) -> Result<NativeCapture, String> {
    let wav: PathBuf = wav.to_path_buf();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

    let join = std::thread::Builder::new()
        .name("hyperpanes-mic".to_string())
        .spawn(move || {
            let late = ready_tx.clone();
            let r = capture(&wav, max, stop_rx, ready_tx);
            // Only lands if the failure came before the stream started; once `capture`
            // has reported readiness the receiver is gone and this send is a no-op.
            if let Err(e) = &r {
                let _ = late.send(Err(e.clone()));
            }
            r
        })
        .map_err(|e| format!("recorder thread: {e}"))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(NativeCapture {
            stop: stop_tx,
            join: Some(join),
        }),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => {
            let _ = join.join();
            Err("recorder thread ended without starting".to_string())
        }
    }
}

/// The whole life of one capture, on the recorder thread.
#[tracing::instrument(level = "debug", ret)]
fn capture(
    wav: &Path,
    max: Duration,
    stop_rx: Receiver<()>,
    ready: Sender<Result<(), String>>,
) -> Result<(), String> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or_else(|| "no microphone: the system reports no audio input device".to_string())?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("microphone config: {e}"))?;
    let src_rate = supported.sample_rate();
    let channels = supported.channels().max(1) as usize;
    let format = supported.sample_format();
    let config: StreamConfig = supported.config();

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: OUT_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let writer: SharedWriter = Arc::new(Mutex::new(Some(
        hound::WavWriter::create(wav, spec).map_err(|e| format!("wav: {e}"))?,
    )));

    let stream = build_stream(&device, &config, format, channels, src_rate, &writer)?;
    stream.play().map_err(|e| format!("microphone: {e}"))?;
    let _ = ready.send(Ok(()));

    // Whichever comes first: the stop signal, the caller dropping its end, or the cap. A
    // mic left on by accident stops itself rather than filling a disk.
    let _ = stop_rx.recv_timeout(max);

    // Order matters: tearing the stream down first guarantees no callback is still
    // holding the writer when it is finalized.
    drop(stream);
    let w = writer.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(w) = w {
        w.finalize().map_err(|e| format!("wav finalize: {e}"))?;
    }
    Ok(())
}

/// Build the input stream for whatever sample type the device hands us.
///
/// Devices offer wildly different formats — f32 on CoreAudio, i16 on most of ALSA, u8 on
/// some cheap USB mics — and the conversion to the i16 the WAV wants is `dasp`'s job, not
/// ours. The exhaustive-looking arm list is one line per format cpal knows.
#[tracing::instrument(level = "debug", skip_all)]
fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
    channels: usize,
    src_rate: u32,
    writer: &SharedWriter,
) -> Result<cpal::Stream, String> {
    macro_rules! s {
        ($t:ty) => {
            input_stream::<$t>(device, config, channels, src_rate, writer)
        };
    }
    match format {
        SampleFormat::I8 => s!(i8),
        SampleFormat::I16 => s!(i16),
        SampleFormat::I32 => s!(i32),
        SampleFormat::I64 => s!(i64),
        SampleFormat::U8 => s!(u8),
        SampleFormat::U16 => s!(u16),
        SampleFormat::U32 => s!(u32),
        SampleFormat::U64 => s!(u64),
        SampleFormat::F32 => s!(f32),
        SampleFormat::F64 => s!(f64),
        other => Err(format!(
            "microphone speaks an unsupported format: {other:?}"
        )),
    }
}

#[tracing::instrument(level = "debug", skip_all)]
fn input_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: usize,
    src_rate: u32,
    writer: &SharedWriter,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    i16: FromSample<T>,
{
    let writer = Arc::clone(writer);
    let mut phase: u32 = 0;
    device
        .build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], _| {
                let mut guard = match writer.lock() {
                    Ok(g) => g,
                    Err(e) => e.into_inner(),
                };
                let Some(w) = guard.as_mut() else { return };
                for frame in data.chunks(channels) {
                    let mono = downmix(frame);
                    // Nearest-neighbour resample, phase-accumulated so it is exact over
                    // any run length and works in both directions (48 kHz down to 16,
                    // 8 kHz up to 16). Speech into a recognizer does not need better,
                    // and anything better needs a resampler crate and a latency budget.
                    phase += OUT_RATE;
                    while phase >= src_rate {
                        phase -= src_rate;
                        let _ = w.write_sample(mono);
                    }
                }
            },
            // A device unplugged mid-sentence: the samples stop, the WAV still finalizes,
            // and the user gets whatever was said before it went away. Nothing here can
            // usefully be surfaced from inside a realtime callback.
            |_e| {},
            None,
        )
        .map_err(|e| format!("microphone stream: {e}"))
}

/// Average a frame's channels into one sample. A stereo mic that has one dead channel
/// still transcribes; picking channel 0 would have made it silence.
#[tracing::instrument(level = "debug", ret, skip(frame))]
fn downmix<T>(frame: &[T]) -> i16
where
    T: SizedSample,
    i16: FromSample<T>,
{
    if frame.is_empty() {
        return 0;
    }
    let sum: i32 = frame.iter().map(|s| i16::from_sample(*s) as i32).sum();
    (sum / frame.len() as i32) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_averaged_not_sampled_so_one_dead_channel_is_not_silence() {
        assert_eq!(downmix::<i16>(&[1000, 0]), 500);
        assert_eq!(downmix::<i16>(&[-100, -100, -100]), -100);
        assert_eq!(downmix::<i16>(&[]), 0);
    }

    #[test]
    fn f32_samples_land_on_the_i16_full_scale() {
        assert_eq!(downmix::<f32>(&[0.0]), 0);
        assert!(downmix::<f32>(&[1.0]) > 32_000);
        assert!(downmix::<f32>(&[-1.0]) < -32_000);
    }

    /// The resampler is a loop over a phase accumulator inside a realtime callback, so
    /// this reproduces it exactly rather than reaching into the closure: what matters is
    /// that a second of input becomes a second of 16 kHz output, at any device rate.
    fn resampled_count(src_rate: u32, input_frames: u32) -> u32 {
        let mut phase = 0u32;
        let mut out = 0u32;
        for _ in 0..input_frames {
            phase += OUT_RATE;
            while phase >= src_rate {
                phase -= src_rate;
                out += 1;
            }
        }
        out
    }

    #[test]
    fn a_second_of_audio_is_a_second_of_audio_whatever_the_device_rate() {
        for rate in [8_000, 16_000, 22_050, 44_100, 48_000, 96_000] {
            let got = resampled_count(rate, rate);
            assert!(
                got.abs_diff(OUT_RATE) <= 1,
                "{rate} Hz produced {got} samples per second, wanted {OUT_RATE}"
            );
        }
    }

    /// The only test that proves the thing this module exists for. Ignored by default
    /// because it needs a real microphone and, on macOS, a granted permission — neither
    /// of which CI has. Run it by hand on a machine with a mic:
    /// `cargo test -p hyperpanes-core -- --ignored native`
    #[test]
    #[ignore = "needs a real microphone"]
    fn a_real_microphone_produces_a_readable_mono_16k_wav() {
        let wav = std::env::temp_dir().join("hyperpanes-native-capture-test.wav");
        let _ = std::fs::remove_file(&wav);
        let cap = start(&wav, Duration::from_secs(30)).expect("start");
        std::thread::sleep(Duration::from_millis(1200));
        cap.finish().expect("finish");

        let reader = hound::WavReader::open(&wav).expect("the wav must be readable");
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, OUT_RATE);
        assert_eq!(spec.bits_per_sample, 16);
        // A second of 16 kHz is 16000 samples; allow generous slack for stream startup.
        let n = reader.len();
        assert!(
            (8_000..=24_000).contains(&n),
            "1.2s of capture produced {n} samples"
        );
        // Not asserted — a muted mic in a silent room is a legitimate zero, and a test
        // that failed on it would be a test about the room. Printed because it is the one
        // number that separates "captured audio" from "captured a denied-permission
        // silence", which is exactly what a human running this by hand wants to see.
        let peak = hound::WavReader::open(&wav)
            .expect("reopen")
            .samples::<i16>()
            .filter_map(Result::ok)
            .map(|s| (s as i32).unsigned_abs())
            .max()
            .unwrap_or(0);
        eprintln!("peak amplitude: {peak} / 32768");
        let _ = std::fs::remove_file(&wav);
    }

    #[test]
    fn detection_does_not_panic_on_a_machine_with_no_sound_card() {
        // CI has no microphone; a developer's laptop does. Both answers are correct —
        // what must never happen is a panic out of the host enumeration.
        let _ = available();
    }
}
