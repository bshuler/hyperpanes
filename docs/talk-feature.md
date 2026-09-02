# Per-pane talk (spoken assistant replies)

A per-pane **Talk** toggle: while it is on, that pane speaks every NEW assistant reply aloud
through a local TTS backend. Off by default, per pane, persisted in the workspace snapshot. The
text comes from the conversation's own transcript (the pane→session marker JSONL), never from
scraping the terminal — so what is spoken is the assistant's actual reply, normalized from
whatever text format it was written in — plain text, Markdown, HTML, JSON, CSV/TSV — to
listenable prose. All panes share ONE serialized speech queue with pane-label
prefixes, a global mute/stop, and an optional focused-pane-only mode.

**Status: BUILT** — core pipeline + control API + GUI + MCP tools; verified end-to-end against
the headless binary (see "Verifying" below). Generalized past Claude-only and Unix-only on
2026-09-02: five transcript formats, four TTS backends across three platforms.

## Which tools can talk, and why not all of them

Talk needs the tool's own record of what it said. Five tools keep one Hyperpanes can bind to a
pane and tail:

| Tool | Pane→session binding | Transcript | Record shape |
| --- | --- | --- | --- |
| claude | `claude-sessions/<paneId>.json` (SessionStart hook) | `<configDir>/projects/<encoded-cwd>/<sid>.jsonl` | `{"type":"assistant","message":{"content":[…]}}` |
| cursor-agent | `tool-sessions/cursor-agent/<paneId>.json` (`~/.cursor/hooks.json`) | `~/.cursor/projects/<Encoded-Cwd>/agent-transcripts/<sid>/<sid>.jsonl` | `{"role":"assistant","message":{"content":[…]}}` — no `type` on message records; `type` marks control records like `turn_ended` |
| copilot | `tool-sessions/copilot/<paneId>.json` (`~/.copilot/settings.json`) | `~/.copilot/session-state/<sid>/events.jsonl` | `{"type":"assistant.message","data":{"content":"…"}}` — flat event log |
| codex | `tool-sessions/codex/<paneId>.json` (`$CODEX_HOME/hooks.json`) | `$CODEX_HOME/sessions/<YYYY>/<MM>/<DD>/rollout-<stamp>-<sid>.jsonl` | `{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text",…}]}}` — every line is a `{timestamp,ordinal,type,payload}` envelope |
| gemini | `tool-sessions/gemini/<paneId>.json` (`~/.gemini/settings.json`) | `~/.gemini/tmp/<project dir>/chats/session-<stamp>-<first 8 of sid>.jsonl` | `{"type":"gemini","content":"…"}` — `content` is a bare **string** here and an array of `{text}` blocks on a `"type":"user"` record |

Every shape above was read off a real install, not off documentation.

Three things about codex specifically, each of which cost a wrong first guess:

* Its hooks live in **`$CODEX_HOME/hooks.json`**, not in `config.toml` — which silently
  accepts unknown keys, so a hook written there looks accepted and never fires. The
  project-scoped `<cwd>/.codex/hooks.json` did not fire either.
* It takes **Claude's** nested matcher group (`{"hooks":[{"type":"command","command":…}]}`),
  not cursor/copilot's flat `{"command":…}`, and its event names are **PascalCase**
  (`SessionStart`/`SessionEnd`). An event under the wrong casing is never called, with no
  error. `HookShape` in `tools::session_hook` exists for exactly this split.
* Hooks are **trust-gated**. Hyperpanes writes `hooks.json`; codex then asks the human to
  approve it once, and until they do the hook does not run (the escape hatch,
  `--dangerously-bypass-hook-trust`, is for testing). That is a security control, so the
  approval is left to the person at the keyboard — nothing here forges a trust record.

The same reply appears three times in a codex rollout: as the `response_item` (the model
transcript), as an `event_msg`/`item_completed` carrying an `AgentMessage` (the UI event
stream), and as an `event_msg`/`task_complete` carrying `last_agent_message` (the final
message only). Only the first is read; reading a second would speak every reply twice, which
`codex_says_each_reply_once_and_not_three_times` in `speech::tailer` locks down.

Three things about gemini, likewise:

* Its hooks go in **`~/.gemini/settings.json`** under a top-level `"hooks"` key, in the same
  nested, PascalCase shape codex takes — gemini ships a `gemini hooks migrate` that imports
  Claude Code's config, which is the corroboration for that. Unlike codex it does **not**
  trust-gate hooks: writing the file is enough, with nothing for the human to approve.
* Its config override is **`GEMINI_CLI_HOME`**, and it names the **home directory** — the
  `.gemini` is still appended. (`GEMINI_DIR` appears in the bundle and is a compile-time
  constant holding the string `.gemini`, not an environment variable.) `RootEnv` in
  `tools::session_hook` carries that distinction against codex's `CODEX_HOME`, which names
  the config directory itself. Reading either as the other writes a valid hook into a file
  the tool never opens, and an unregistered hook does not error — it just never fires.
* `<project dir>` is **not derivable from the cwd**. It is the cwd's basename, suffixed
  `-1`, `-2`, … when an earlier directory already claimed that basename, allocated
  first-seen-wins and recorded in `~/.gemini/projects.json`. Two checkouts both called `api`
  map to `api` and `api-1` depending on which gemini saw first. So the transcript is found
  by searching `~/.gemini/tmp/*/chats` for the filename's 8-character id prefix and then
  confirming the **full** id on the file's first line — the prefix narrows, the header
  decides.

A gemini chat file is also not a plain log of messages: it is a mutation stream, and its
`$set` records rewrite state. One of them carries an entire `messages` array (the session
context preamble). Those are ignored outright — gemini's version of codex's triple-record
trap, locked down by `gemini_set_records_are_never_spoken`.

### On Windows, one hook script instead of five

The POSIX hooks are `/bin/sh` wrappers around `python3`. Neither part survives a default
Windows install: `/bin/sh` is not there, `python3` is not on `PATH` (the Store stub that *is*
opens a shop page rather than running), and the state directory those scripts compute with
`uname` resolves under a Git-Bash-ish shell to the XDG path rather than
`%APPDATA%\hyperpanes` — so shipping them there would write markers to a directory the
reader never looks in, and, like every unregistered or failed hook, would do it silently.

Windows therefore registers **one** script, `resources/hooks/hp-session-hook.ps1`, told which
tool it is running for on the command line. The five payload shapes are small enough to be a
`switch`, and the part that is genuinely easy to get wrong — BOM-less UTF-8, an atomic
replace — is then written once instead of five times. The registered command is

```
powershell -NoProfile -ExecutionPolicy Bypass -File "<install>\resources\hooks\hp-session-hook.ps1" -Tool <id>
```

Each flag is load-bearing. `-ExecutionPolicy Bypass` because the default policy on a fresh
install is `Restricted`, which refuses the script before its first line; `-NoProfile` so a
user profile cannot slow down or break something that runs at every session start; the quotes
because an install under `C:\Program Files\…` contains a space. `powershell` rather than
`pwsh`, because Windows PowerShell 5.1 ships in the box and PowerShell 7 does not.

**What is verified and what is not.** The script's own logic is tested — the payload switch,
the marker it writes, and the command string (including the `Program Files` spaces and the
restricted-policy case) all have unit coverage, and the script has been run against real
payloads with `pwsh` on macOS. What has **not** been checked on a Windows machine is whether
each agent CLI actually spawns the registered command as written. That is the one remaining
gap in the Windows story, and it needs a Windows host, not another test.

A pane running anything else — aider, goose, a plain shell — resolves to no
transcript and **stays silent**. That is deliberate, not a gap waiting to be filled by reading
the terminal: a terminal carries spinners, progress bars, box drawing and the human's own
echoed keystrokes with no way to tell them from prose, so a scraped tier would make Talk worse,
not better. Adding a tool means adding a hook that writes a pane marker plus a
`TranscriptFormat` arm — nothing else.

## Pipeline

```
pane (talk on)                       core, every 750ms (speech_service::run_ticker)
  └─ resolve_transcript(paneId)      claude marker first, then any hooked tool's marker
       └─ TranscriptRef {path, format}  path + which of the five record shapes to read
            └─ TranscriptTail        byte-cursor tail, starts at EOF (history is never spoken)
                 └─ extract_assistant_text   per-format; tool_use/tool_result dropped
                      └─ normalize_for_speech  any text format → prose (see below)
                           └─ SpeechEngine     ONE global queue, one utterance at a time
                                └─ backend      custom | spd-say | espeak-ng | say | SAPI
```

A pane whose agent exits and restarts points at a different file (or a different tool's
format); the tail notices and re-starts at THAT file's end, so the new conversation's backlog is
not spoken from the top.

```
```

Every stage is core-owned (`rs/crates/core/src/speech/` + `control/speech_service.rs`), spawned
beside the control server — so the GUI app and the headless daemon (`--bin headless`) behave
identically, and the whole feature is testable without audio or a display.

- **Default-off costs nothing**: the engine thread spawns lazily on the first enabled pane, and
  the ticker's no-talkers fast path does no filesystem work at all.
- **Normalization** (`speech/normalize.rs` + `speech/markup.rs`): a tool writes down whatever
  format its answer was in, and a synthesizer reads every one of them literally, so each is
  recognized and reduced:
  - **terminal noise** first, because it turns up inside any of the others: ANSI colour and
    cursor sequences, OSC titles, box drawing, braille spinner frames, zero-width and
    bidi characters. Each dropped character becomes a space, so two cells either side of a
    box edge do not fuse into one word.
  - **JSON and CSV/TSV** next, whole-document, *before* anything is stripped — their
    punctuation is their structure. JSON is flattened to its readable leaves with `snake_case`
    keys read as words ("exit code"); delimited rows become `", "`-joined cells, `". "`-joined.
    Both guesses are deliberately hard to trip: English is full of commas, so a row that reads
    like a sentence (a sentence break inside a field, a field over 12 words, a final field
    ending in `.`/`!`/`?`), ragged rows, a lone column, or anything wearing Markdown's clothes
    all veto the CSV reading.
  - **Markdown**, the common case: fenced code becomes the phrase "code block omitted.";
    headings, rules and setext underlines, blockquotes, list markers, task boxes, tables,
    emphasis, strikethrough and link syntax are stripped to their text; an image's alt text is
    spoken (it was written for someone who cannot see the image); bare URLs, autolinks and link
    reference definitions are dropped.
  - **HTML**, which arrives both on its own and embedded in Markdown: detection is by tag
    *name*, so `Vec<String>` and `a < b` survive as prose. Tag text is kept with block tags
    breaking paragraphs and inline tags not breaking words (`re<b>run</b>` is one word);
    `<script>`, `<style>`, `<svg>`, `<template>` and comments are dropped whole; `<td>`/`<th>`
    become `", "`; `<img alt>` is spoken; named, Latin-1 and numeric entities are decoded, and
    an entity that is not recognized is left verbatim rather than guessed at.
  - finally whitespace collapses and long replies truncate at a sentence boundary (~1200 chars)
    with a spoken "Truncated."

  Every guess fails soft: detection is conservative and each transform is close to the identity
  on text that is really just prose, because the listener hears the output and the input is
  already gone.
- **Serialization** (`speech/engine.rs`): one utterance plays at a time, FIFO (bounded queue,
  cap 64, drop-oldest). With two or more panes talking, each utterance is prefixed with its
  pane's label ("build. Tests are green."). `stop_all` kills the in-flight backend process
  immediately and discards the backlog; mute is the sticky variant.

## Control API

| Command | Effect |
| --- | --- |
| `{"type":"setTalk","paneId":…,"enabled":bool}` | toggle a pane's talk; result reports the resolved state + backend (+ a warning when no backend exists) |
| `{"type":"stopSpeech"}` | global one-shot: kill in-flight speech, drop the backlog |
| `{"type":"setSpeechMuted","muted":bool}` | global sticky mute (persisted) |
| `{"type":"setSpeechFocusedOnly","focusedOnly":bool}` | speak only the focused pane (persisted) |

The three global commands are pane-less (handled before window resolution, like `queuePrompt`).
`GET /state` reports `talk: true` on each talking pane and a top-level
`speech: {muted, focusedOnly, backend, speakingPane}` block. The read-model is the single
source of truth: GUI toggles publish into it each tick, control-originated changes reconcile
back onto the GUI (`control_host.rs`), so the two surfaces can never disagree.

## Settings — `<config dir>/speech.json`

A user setting (beside `ai-settings.json`), all fields optional:

```json
{
  "commandTemplate": ["/bin/sh", "/path/to/speak.sh", "{text}"],
  "muted": false,
  "focusedOnly": false
}
```

`commandTemplate` overrides backend auto-detection: an argv array run per utterance WITHOUT a
shell; `{text}` in any element is replaced with the spoken text, and a template with no
`{text}` receives it on stdin. This is also the testing seam — point it at a script that
appends to a file and the whole pipeline is observable headless. **No backend at all** degrades
to a one-time GUI notice + no-op; nothing crashes and the toggle still round-trips.

### Backends (`speech/engine.rs`)

| Platform | Detected backend | Notes |
| --- | --- | --- |
| Linux | `spd-say`, else `espeak-ng` | first one on `PATH` wins |
| macOS | `say` | always present |
| Windows | PowerShell `System.Speech` | `Speak` is synchronous, so the process exits when the utterance ends — which is what the queue's one-at-a-time contract needs |

The Windows backend passes the utterance in the environment (`HYPERPANES_SPEECH_TEXT`), never
on the command line: a `-Command` string would have to survive both PowerShell's parser and
Windows' single-string argv, and an assistant reply is arbitrary text. The `PATH` probe is
`PATHEXT`-aware there — `powershell` on disk is `powershell.EXE`.

## GUI

- Pane context menu → **Talk (speak replies)** checkable row; a speaker glyph shows in the
  pane header while talk is on.
- Command palette: **Speech: Stop Now / Mute / Only Focused Pane**.
- One-time toasts: enabling talk with no TTS backend, or while the control server (which hosts
  the speech service) is disabled in Preferences.
- Persistence: `PaneSpec.talk` in the workspace snapshot (`workspace/model.rs`), written by
  `to_session_file`/`to_workspace_file` and restored on relaunch.

## MCP tools (hyperpanes-mcp)

`set_talk {paneId, enabled}`, `set_speech {muted?, focusedOnly?}`, `stop_speech {}` — thin
wrappers over the commands above (`src/control-tools.ts`).

## Verifying

`scripts/talk-demo.sh` boots an ISOLATED headless instance (XDG overrides +
`HYPERPANES_CONTROL_FILE` pinned into the sandbox — the env var otherwise leaks in from a pane
and clobbers the live app's discovery file), fakes two panes' session markers + transcripts,
points `commandTemplate` at an evidence-file writer, and asserts: talk toggles are observable
in `/state`; interleaved appends to two transcripts come out as serialized, pane-labelled,
non-interleaved utterances of normalized prose; pre-existing history is never spoken; and
`stopSpeech` kills an in-flight utterance. Unit coverage lives in `speech/{markup,normalize,
engine,tailer}.rs` and the `setTalk`/`setSpeech*`/`stopSpeech` dispatch tests. The format tests
carry the negative cases as well as the positive ones, because the failure that matters here is
mangling ordinary prose: `Vec<String>` must not be HTML, a paragraph with commas must not be
CSV, `exit_code` and `~/.config` must come through intact, and an unknown entity must be left
alone rather than guessed at.
