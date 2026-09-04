//! Where a dictation goes after it has been transcribed — instead of nowhere.
//!
//! Recordings used to be scratch: `stop` transcribed the WAV and deleted it in the next
//! statement, and the transcript existed only as the characters typed into the pane. That
//! is fine right up until the transcript is wrong, at which point two minutes of speech
//! are gone and there is nothing to compare against — no audio to re-run, no text to
//! paste again, and no way to tell whether the recorder, the model or the pane lost it.
//!
//! So both halves are kept here, side by side under one timestamp:
//!
//! ```text
//! <state>/dictation/2026-09-03T20-14-07Z-pane-3.wav   what the microphone heard
//! <state>/dictation/2026-09-03T20-14-07Z-pane-3.txt   what came back, verbatim
//! ```
//!
//! The `.txt` is the transcript and nothing else — no header, no timestamps — because its
//! job is to be selected and pasted. A dictation that produced no transcript keeps its
//! audio anyway, with the failure in the `.txt`, since that is exactly the case someone
//! needs the audio for.
//!
//! Retention is [`KEEP`] recordings, oldest deleted first. This is raw audio of the user
//! in their own state directory: enough history to explain a bad transcript, not an
//! archive of everything ever said.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many recordings survive. Twenty is a few days of ordinary use and, at 16 kHz mono,
/// well under a gigabyte even if every one of them ran to the five-minute cap.
pub const KEEP: usize = 20;

/// A kept recording: the audio, and the text that came out of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub wav: PathBuf,
    pub text: PathBuf,
}

/// Move `wav` into `archive` and write the transcript beside it.
///
/// `outcome` is the transcript, or the error that replaced it. Best-effort throughout: a
/// dictation that could not be archived is still a dictation that was delivered, so every
/// failure here is reported as `None` and logged, never propagated to the pane.
#[tracing::instrument(level = "debug", ret)]
pub fn keep(
    archive: &Path,
    wav: &Path,
    pane_id: &str,
    outcome: Result<&str, &str>,
) -> Option<Kept> {
    if let Err(e) = std::fs::create_dir_all(archive) {
        tracing::warn!(dir = %archive.display(), error = %e, "cannot create the dictation archive; this recording will not be kept");
        return None;
    }
    let base = format!("{}-{}", stamp_utc(now_secs()), pane_id);
    let kept = Kept {
        wav: archive.join(format!("{base}.wav")),
        text: archive.join(format!("{base}.txt")),
    };

    // Rename first: the recording is in a temp directory, which on most machines is the
    // same filesystem and so a metadata-only move. `/tmp` on a separate volume (or a
    // `TMPDIR` pointed somewhere else entirely) falls back to a copy.
    if std::fs::rename(wav, &kept.wav).is_err() {
        if let Err(e) = std::fs::copy(wav, &kept.wav) {
            tracing::warn!(from = %wav.display(), to = %kept.wav.display(), error = %e, "cannot keep the recording");
            return None;
        }
        let _ = std::fs::remove_file(wav);
    }

    // The transcript, verbatim and alone, so it can be selected and pasted. A failure goes
    // in the same place under a marker: what the reader wants to know is why the audio
    // beside it produced nothing.
    let body = match outcome {
        Ok(text) => text.to_string(),
        Err(err) => format!("[no transcript] {err}\n"),
    };
    if let Err(e) = std::fs::write(&kept.text, body) {
        tracing::warn!(path = %kept.text.display(), error = %e, "cannot write the transcript beside the recording");
    }

    prune(archive, KEEP);
    Some(kept)
}

/// Delete all but the `keep` most recent recordings, each with its transcript.
///
/// Ordering is by the name, not by mtime: the name carries the UTC stamp that produced it,
/// so it sorts chronologically without a stat per file and without trusting a clock that
/// may have moved between recordings.
#[tracing::instrument(level = "debug", ret)]
pub fn prune(archive: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(archive) else {
        return;
    };
    let mut wavs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "wav"))
        .collect();
    if wavs.len() <= keep {
        return;
    }
    wavs.sort();
    let doomed = wavs.len() - keep;
    for wav in wavs.into_iter().take(doomed) {
        let _ = std::fs::remove_file(wav.with_extension("txt"));
        let _ = std::fs::remove_file(wav);
    }
}

/// Seconds since the epoch, or 0 on a clock set before 1970.
#[tracing::instrument(level = "debug", ret)]
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `2026-09-03T20-14-07Z`: sortable, readable, and legal as a filename on all three
/// platforms — which is why the time separators are hyphens and not the colons ISO-8601
/// would use.
///
/// UTC on purpose. A local stamp would reorder the archive twice a year, and the sort in
/// [`prune`] is what decides which recording is deleted.
#[tracing::instrument(level = "debug", ret)]
pub fn stamp_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}-{:02}-{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days-since-epoch to a calendar date, by Howard Hinnant's `civil_from_days`. Written out
/// rather than pulled in: one date, once per recording, is not worth a dependency whose
/// timezone database has to be kept current.
#[tracing::instrument(level = "debug", ret)]
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hp-stt-archive-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn a_wav(at: &Path, name: &str) -> PathBuf {
        let p = at.join(name);
        std::fs::write(&p, b"RIFFfake").unwrap();
        p
    }

    #[test]
    fn the_epoch_and_a_date_this_code_was_written_on_both_format_correctly() {
        assert_eq!(stamp_utc(0), "1970-01-01T00-00-00Z");
        // 2026-09-03T20:14:07Z
        assert_eq!(stamp_utc(1_788_466_447), "2026-09-03T20-14-07Z");
        // A leap day, which is the one the hand-written calendar could plausibly get wrong.
        assert_eq!(stamp_utc(1_709_164_800), "2024-02-29T00-00-00Z");
    }

    #[test]
    fn a_stamp_sorts_chronologically_as_a_string() {
        let mut v = [
            stamp_utc(2_000_000_000),
            stamp_utc(0),
            stamp_utc(1_788_596_047),
        ];
        v.sort();
        assert_eq!(v[0], stamp_utc(0));
        assert_eq!(v[2], stamp_utc(2_000_000_000));
    }

    #[test]
    fn a_transcribed_recording_keeps_both_the_audio_and_the_text() {
        let scratch = dir("ok-scratch");
        let archive = dir("ok-archive");
        let wav = a_wav(&scratch, "pane-3.wav");

        let kept = keep(&archive, &wav, "pane-3", Ok("hello there")).expect("kept");

        assert!(
            !wav.exists(),
            "the scratch recording must be moved, not copied"
        );
        assert_eq!(std::fs::read(&kept.wav).unwrap(), b"RIFFfake");
        // Verbatim and alone: this file is meant to be selected and pasted.
        assert_eq!(std::fs::read_to_string(&kept.text).unwrap(), "hello there");
        assert!(kept
            .wav
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-pane-3.wav"));
    }

    #[test]
    fn a_failed_transcription_keeps_the_audio_because_that_is_when_it_matters_most() {
        let scratch = dir("fail-scratch");
        let archive = dir("fail-archive");
        let wav = a_wav(&scratch, "pane-1.wav");

        let kept = keep(&archive, &wav, "pane-1", Err("no speech in the recording")).expect("kept");

        assert!(kept.wav.exists());
        assert!(std::fs::read_to_string(&kept.text)
            .unwrap()
            .contains("no speech in the recording"));
    }

    #[test]
    fn only_the_newest_recordings_survive_and_each_takes_its_transcript_with_it() {
        let archive = dir("prune");
        for i in 0..8 {
            std::fs::write(archive.join(format!("2026-09-0{i}T00-00-00Z-p.wav")), b"x").unwrap();
            std::fs::write(archive.join(format!("2026-09-0{i}T00-00-00Z-p.txt")), b"x").unwrap();
        }
        prune(&archive, 3);

        let mut left: Vec<String> = std::fs::read_dir(&archive)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "2026-09-05T00-00-00Z-p.txt",
                "2026-09-05T00-00-00Z-p.wav",
                "2026-09-06T00-00-00Z-p.txt",
                "2026-09-06T00-00-00Z-p.wav",
                "2026-09-07T00-00-00Z-p.txt",
                "2026-09-07T00-00-00Z-p.wav",
            ]
        );
    }

    #[test]
    fn pruning_an_archive_that_does_not_exist_is_not_an_error() {
        prune(Path::new("/nonexistent/hyperpanes/dictation"), 5);
    }

    #[test]
    fn keeping_is_best_effort_and_a_missing_recording_never_panics() {
        let archive = dir("missing");
        assert_eq!(
            keep(&archive, Path::new("/nonexistent.wav"), "p", Ok("x")),
            None
        );
    }
}
