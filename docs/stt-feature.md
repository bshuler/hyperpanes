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

The bill, stated plainly: four Cargo dependencies; a Linux **build** now needs
`libasound2-dev` (alsa-sys is pkg-config'd; there is no vendored path) and CI and
`scripts/gui-harness/Dockerfile` install it; and CI now compiles C++ (whisper.cpp, via
cmake + libclang for bindgen — already on all three runners; the Windows jobs set
`LIBCLANG_PATH` explicitly). At **runtime** Linux needs only `libasound.so.2`, which every
desktop with working sound already has, and the first dictation on a fresh machine downloads
one model file. Nothing else.

## Pipeline

```
click mic   → recorder (in-process, or a command)
                                      → <temp>/hyperpanes-dictation-<pid>/<pane>.wav
            → (in the background)     → fetch the speech model if it is not cached yet
click again → graceful stop           → the recorder finalizes its WAV header
            → transcriber (in-process, or a command)
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

### Transcribers (`stt/backend.rs`, `stt/whisper.rs`)

The recorder half had a hole and so did this one, in the same shape: `whisper` (the Python
package) and `whisper-cli` (Homebrew/apt) are on no stock machine, so a recorder that finally
worked just moved the failure one step later — from "no recorder found" to "no transcriber
found". `stt/whisper.rs` compiles whisper.cpp **into the binary** (`whisper-rs`), so the
inference is one code path on macOS, Windows and Linux.

Detection order: `transcribeTemplate` if set → in-process if its model is already downloaded →
`whisper` → `whisper-cli` (only when `model` is set, since it exits without one) → in-process,
this time fetching the model. The built-in engine appears twice on purpose. It is the only one
guaranteed to be present, so it has to be the floor; but a *cached* model beats an external
tool (same engine, no process, no `PATH` lookup) while an *uncached* one does not — someone who
installed `whisper` deliberately should not be made to wait on a download they have no use for.
Both arms are synchronous and touch no network, so `GET /state` can ask on every poll.

| Transcriber | Needs | Notes |
| --- | --- | --- |
| **in-process (`whisper-rs`)** — the default | nothing installed; one model download | CPU only: no Metal/CoreML/CUDA, so all three OSes run identical code |
| `whisper` | the Python package on `PATH` | fetches its own weights |
| `whisper-cli` | whisper.cpp on `PATH` **and** `model` set | `-m <model>` |
| `transcribeTemplate` | whatever you point it at | transcript read from stdout |

Both paths' output goes through `clean_transcript`, which strips whisper's timestamp brackets
and its `[BLANK_AUDIO]` / `(Music)` markers.

#### The model, and why it is not vendored

Weights are data, not code: a few hundred megabytes that would sit in every download whether or
not anyone dictates. So the binary ships the engine and fetches the model once, from
whisper.cpp's own upstream distribution, into `<data>/models/`:

| `model` | Size | SHA-256 pinned in `MODELS` |
| --- | --- | --- |
| `tiny.en` | 78 MB | `921e4cf8…20b1f` |
| `base.en` **(default)** | 148 MB | `a03779c8…6d002` |
| `small.en` | 488 MB | `c6138d6d…41e5d` |

`model` takes one of those names, **or** a path to a `ggml-*.bin` of your own (which is also
what `whisper-cli` gets as `-m`). A name is read as a name, not as a relative path; a
configured path that has gone missing falls back to the default rather than leaving a dead mic
button.

Three things make the download defensible rather than a 148 MB act of faith:

- **It is verified.** Every built-in model has a pinned SHA-256, checked before the blob is
  handed to a C++ inference engine — a truncated download and a substituted one look identical
  from in there. A mismatch is deleted and reported, never used.
- **It is atomic.** The body streams to a `.part` sibling and is renamed only after the digest
  matches, so an interrupted fetch can never leave a short file at the name the loader trusts.
  A `Mutex` means two panes reaching for the mic at once fetch one model, not two into one path.
- **It overlaps the talking.** The fetch starts when the mic *opens*, not when it closes, so
  the first-ever dictation costs a few seconds of waiting rather than the whole download. A
  failure there is ignored — it must not block the recording — and retried, with its error
  reported and a percentage attached, when there is finally a transcript to make.

After that one download there is no network path at all: the audio never leaves the machine,
which is the other reason this is in-process rather than a cloud API.

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
"dictation": { "recorder": "native", "transcriber": "native", "recordingPanes": ["p1"] }
```

Naming both halves lets a client tell "no microphone on this machine" apart from "old server".
`transcriber` is now `"native"` on a stock install rather than `"none"`; `recorder` is `"none"`
only when the machine reports no audio input device at all. The GUI reads the same list each
tick and lights the header microphone.

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
  "model": "base.en",
  "submit": false
}
```

`{wav}` in any argument is replaced with the recording's path. A template runs WITHOUT a
shell. `model` is a built-in name (`tiny.en` / `base.en` / `small.en`) or a path to your own
`ggml-*.bin`. This is also the testing seam: a `recordTemplate` that copies a fixture and a
`transcribeTemplate` that `cat`s a text file make the whole feature observable headless.

## GUI

- A microphone button in every pane header (red while recording), and a **Dictate** row in the
  pane context menu that reads as the stop while recording.
- Command palette: **Dictate: Toggle Microphone** (focused pane).
- Toasts carry the outcome — which recorder started, what the transcriber produced, or why
  nothing happened ("no microphone found", "Dictation needs the control server"). A first
  dictation that is still waiting on the model download says so with a percentage.
- Like talk, dictation lives on the control server: with it disabled in Preferences the button
  says so rather than failing silently.
