# Per-pane dictation (speak into a pane)

A microphone button in every pane header. Click it, talk, click it again: the recording is
transcribed by a local speech-to-text engine and **typed into that pane's pty** as if you had
typed it. The mirror image of [talk](talk-feature.md), which speaks the assistant's replies
back out.

**Status: BUILT** — core state machine + control API + GUI. Nothing is sent anywhere: both
halves are local commands you already have (or don't, in which case the feature says so and
stays out of the way).

## Why in-process first, with commands as the fallback

This shipped as commands only — `ffmpeg`, `sox`/`rec`, `arecord` — on the reasoning that every
desktop that can record already ships something that records, exactly as every desktop ships
something that speaks. **That reasoning was wrong**, and it was wrong in the way that mattered:
a stock macOS has none of the three, a stock Windows has none of the three, and a GUI app
launched from the Dock cannot see a Homebrew install even when the user has one (see
`speech::engine::resolve`). The mic button's entire behaviour on a fresh machine was to report
"no recorder found" to someone whose microphone was working perfectly.

`stt/native.rs` captures through the OS audio API instead — CoreAudio, WASAPI, ALSA, via `cpal`
— and writes the WAV with `hound`. It needs nothing installed, and it is tried **first**. The
commands stay as the fallback for a machine cpal cannot open a device on, and `recordTemplate`
still overrides everything, which is what keeps the pipeline testable headless (point it at a
script that copies a fixture WAV).

The bill, stated plainly: two Cargo dependencies, and a Linux **build** now needs
`libasound2-dev` (alsa-sys is pkg-config'd; there is no vendored path). CI and
`scripts/gui-harness/Dockerfile` install it. At **runtime** Linux needs only `libasound.so.2`,
which every desktop with working sound already has.

## Pipeline

```
click mic   → recorder command        → <temp>/hyperpanes-dictation-<pid>/<pane>.wav
click again → graceful stop           → the recorder finalizes its WAV header
            → transcriber command     → stdout
            → sanitize                → sessions.write(uid, text)      [the pane's pty]
            → (optional) 40ms later   → sessions.write(uid, "\r")      [submit]
```

### Recorders (`stt/backend.rs`, `stt/native.rs`)

Detection order is: `recordTemplate` if set → in-process capture if this machine has a usable
input device → the commands below.

| Platform | Detected recorder | Stop |
| --- | --- | --- |
| all | **in-process (`cpal`)** — the default, needs nothing installed | signal a channel, join the thread |
| macOS | `ffmpeg -f avfoundation -i :default` | write `q\n` to stdin |
| Linux | `ffmpeg -f pulse`, else `rec` (sox), else `arecord` | `q\n` / SIGINT |
| Windows | `ffmpeg -f dshow` with the first enumerated audio device | `q\n` (the only stop that works there) |

Whatever the device offers — f32 on CoreAudio, i16 on most of ALSA, u8 on cheap USB mics, at
8/44.1/48/96 kHz, mono or multichannel — the in-process recorder hands on the same **mono
16 kHz signed 16-bit** WAV the external recorders are asked for: channels averaged (so one dead
channel of a stereo mic is not silence) and resampled by a phase accumulator that is exact over
any run length.

**The stop is the whole design problem.** A WAV header states the data length, and a recorder
killed mid-write leaves one that claims zero bytes — a file every transcriber reads as
silence. So each recorder carries a `StopKind`: ffmpeg gets `q\n` on stdin (also the only
graceful stop available on Windows, which has no SIGINT to send), sox/arecord get SIGINT.
`SIGKILL` happens only after `STOP_GRACE` (3s) of the process refusing to exit, and a WAV
under `MIN_WAV_BYTES` (2048) is reported as "no audio captured" rather than handed to a
transcriber that would return an empty string.

`MAX_RECORD_SECS` (300) is enforced **by the recorder itself** (`-t` / `trim` / `-d`, and a
`recv_timeout` on the capture thread), not by a timer in the GUI — so the cap survives a
crashed GUI. A forgotten microphone stops on its own.

### Transcribers

`whisper` (the Python one, which fetches its own weights) or `whisper-cli` (whisper.cpp, which
needs a model file — set `model` in `stt.json`). First on `PATH` wins. `whisper`'s progress
chatter and timestamp brackets are stripped (`clean_transcript`).

## The transcript is untrusted input

Whisper transcribes whatever sound is in the room — a podcast, a colleague, a video call — and
the destination is a shell. So every transcript is flattened before it reaches the pty:
control characters and newlines become spaces, runs collapse. **A dictated newline can never
submit a line**; only `submit: true` does, and it is off by default because dictation is not
reliable enough to send a prompt no human has read back. The submit, when on, is a *separate*
bare-CR write 40ms later, so a bracketed-paste TUI reads it as Enter rather than as part of
the pasted text.

## Where the state lives

Recording state lives on the `DictationService` (beside the control server's `SpeechService`),
**not** on the read-model's `PaneInfo`. The GUI republishes every pane wholesale each sync
tick, so a per-pane flag in the read model would be stamped straight back out. `GET /state`
therefore reports it as one additive top-level block:

```json
"dictation": { "recorder": "native", "transcriber": "none", "recordingPanes": ["p1"] }
```

Naming both halves even when nothing is installed lets a client tell "no recorder here" apart
from "old server". The GUI reads the same list each tick and lights the header microphone.

## Control API

| Command | Effect |
| --- | --- |
| `{"type":"startDictation","paneId":…}` | begin recording; result names the resolved recorder |
| `{"type":"stopDictation","paneId":…}` | stop, transcribe, type it in; result carries `text`, `backend`, `submitted` |
| `{"type":"cancelDictation","paneId":…}` | stop and throw the audio away |

These three are handled in `routes.rs`, **not** `dispatch.rs`, for two reasons: they need
`Shared` (the service *and* the pty write), and `stopDictation` blocks for as long as the
transcriber takes — `dispatch` runs holding the read-model mutex, which must never be held
across a whisper run. The route hands it to `spawn_blocking`; the GUI hands it to a worker
thread and toasts the result on the next tick. Both go through the one
`dictation_service::stop_and_deliver`, so a transcript can never reach a pane by two subtly
different routes.

Dictation types into a pane, so it obeys `allowInput`: a read-only deployment cannot be
bypassed by speaking into it. `cancelDictation` is infallible by design — it is the teardown
path (`closePane` calls it, and so does the GUI when a recording pane disappears), and
teardown must never have to know whether a mic was live.

## Permissions

`DictationService::start` is the first caller of `Right::Microphone`. It calls
`permissions::prompt` — raising the OS's own consent dialog **from the feature that needs it**,
because macOS shows each such dialog exactly once, ever; spending it on a settings screen the
user opened out of curiosity would burn it. Once spent, `open_microphone_settings()` opens the
exact Settings pane instead. The macOS bundle already declares
`NSMicrophoneUsageDescription` and `com.apple.security.device.audio-input`.

Note the bundle is ad-hoc signed, so macOS drops every permission grant on rebuild — a fresh
install will ask again.

## Settings — `<config dir>/stt.json`

Beside `speech.json`; all fields optional:

```json
{
  "recordTemplate": ["ffmpeg", "-f", "avfoundation", "-i", ":default", "-t", "300", "{wav}"],
  "transcribeTemplate": ["whisper-cli", "-f", "{wav}", "-nt"],
  "model": "/path/to/ggml-base.en.bin",
  "submit": false
}
```

`{wav}` in any argument is replaced with the recording's path. A template runs WITHOUT a
shell. This is also the testing seam: a `recordTemplate` that copies a fixture and a
`transcribeTemplate` that `cat`s a text file make the whole feature observable headless.

## GUI

- A microphone button in every pane header (red while recording), and a **Dictate** row in the
  pane context menu that reads as the stop while recording.
- Command palette: **Dictate: Toggle Microphone** (focused pane).
- Toasts carry the outcome — which recorder started, what the transcriber produced, or why
  nothing happened ("no recorder installed", "Dictation needs the control server").
- Like talk, dictation lives on the control server: with it disabled in Preferences the button
  says so rather than failing silently.
