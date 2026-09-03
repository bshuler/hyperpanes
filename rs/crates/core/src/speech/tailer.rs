//! Incremental tailer over a pane's agent session transcript — yields the text of NEW
//! assistant replies only, never history and never terminal output.
//!
//! Five tools write their conversation to a growing JSONL file as it happens, which is
//! exactly what a tailer needs. The paths and the record shapes differ, so
//! [`TranscriptFormat`] names which one a given file speaks; the byte-cursor machinery
//! below is shared:
//!
//! * **claude** — `<config_dir, or ~/.claude>/projects/<encoded-cwd>/<session id>.jsonl`;
//!   records are `{"type":"assistant","message":{"content":[…]}}`.
//! * **cursor-agent** — `~/.cursor/projects/<encoded-cwd>/agent-transcripts/<id>/<id>.jsonl`;
//!   same `message.content` block array, but keyed `"role":"assistant"` with no `type`.
//! * **copilot** — `~/.copilot/session-state/<id>/events.jsonl`; a flat event log whose
//!   `{"type":"assistant.message","data":{"content":"…"}}` carries the reply as a string.
//! * **codex** — `$CODEX_HOME/sessions/<YYYY>/<MM>/<DD>/rollout-<stamp>-<id>.jsonl`; every
//!   line is a `{"timestamp","ordinal","type","payload"}` envelope, and the reply rides in
//!   the `response_item` whose payload is a `role:"assistant"` message.
//! * **gemini** — `~/.gemini/tmp/<project dir>/chats/session-<stamp>-<id prefix>.jsonl`; a
//!   log-structured mutation stream whose plain records are messages and whose `$set`
//!   records rewrite state, with the reply in a `{"type":"gemini","content":"…"}`.
//!
//! Every shape here was read off a real install on this machine, not inferred from docs —
//! the same standard [`crate::tools::session_hook`] holds itself to.
//!
//! Unlike [`crate::claude_history`], which parses a bounded prefix once, [`TranscriptTail`]
//! is built to be polled repeatedly (once per pane, on a timer) and only ever look at bytes
//! appended since the last poll — a "talk" pane must never re-speak old history, so the
//! cursor starts at end-of-file, not at the top of the transcript.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::claude_history::encode_path_str;
use crate::claude_panes::PaneClaudeSession;

/// A single JSONL line longer than this is skipped rather than buffered in full — bounds
/// memory against a pathological or corrupt transcript line instead of growing the tail
/// buffer without limit while waiting for its newline.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Resolve `marker`'s live session to its transcript path: `<config_dir, or ~/.claude>/
/// projects/<encoded cwd>/<session_id>.jsonl`. `None` when the marker has no cwd (nothing
/// to encode) or no home directory is known for the default-account fallback.
#[tracing::instrument(level = "debug", ret)]
pub fn transcript_path(marker: &PaneClaudeSession) -> Option<PathBuf> {
    if marker.cwd.is_empty() {
        return None;
    }
    let projects_root = if marker.config_dir.is_empty() {
        crate::claude_history::claude_projects_root()?
    } else {
        PathBuf::from(&marker.config_dir).join("projects")
    };
    let encoded = encode_path_str(&marker.cwd);
    Some(
        projects_root
            .join(encoded)
            .join(format!("{}.jsonl", marker.session_id)),
    )
}

/// Which on-disk record shape a transcript file speaks. One variant per tool that keeps a
/// live, append-only conversation log; tools without one are spoken from their terminal
/// output instead (see `crate::control::speech_service`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptFormat {
    /// `{"type":"assistant","message":{"content":[{"type":"text","text":"…"}]}}`
    ClaudeJsonl,
    /// `{"role":"assistant","message":{"content":[{"type":"text","text":"…"}]}}`
    CursorJsonl,
    /// `{"type":"assistant.message","data":{"content":"…"}}`
    CopilotEvents,
    /// `{"type":"response_item","payload":{"type":"message","role":"assistant",
    /// "content":[{"type":"output_text","text":"…"}]}}` — one line of a codex rollout.
    ///
    /// The same reply also appears twice more in the same file, and both are deliberately
    /// ignored: an `event_msg`/`item_completed` carrying an `AgentMessage` (the UI event
    /// stream) and an `event_msg`/`task_complete` carrying `last_agent_message` (the final
    /// message only). Reading any second one would speak every reply twice. `response_item`
    /// is the model transcript proper — exactly one record per assistant message, no
    /// streaming deltas, and every message of a multi-message turn rather than just the last.
    CodexRollout,
    /// `{"type":"gemini","content":"…"}` — one message of a gemini chat log.
    ///
    /// `content` is a plain string here; on a `"type":"user"` record the same key holds an
    /// array of `{"text":…}` blocks instead, so the type gate is doing real work rather
    /// than being a formality.
    ///
    /// The file is not a plain append-only log of messages: gemini also writes `$set`
    /// records that rewrite state, and one of them carries a whole `messages` array
    /// (the session-context preamble). Those are ignored entirely — reading into a `$set`
    /// would re-speak replies already spoken, which is gemini's version of the triple-record
    /// trap [`TranscriptFormat::CodexRollout`] documents.
    GeminiChat,
}

/// Resolve a hooked tool's live conversation to its transcript file.
///
/// `tool_id` is a [`crate::tools::registry`] id and `session_id`/`cwd` come from that
/// tool's session-hook marker. `None` for a tool with no tailable log — the caller falls
/// back to the pane's terminal output rather than going silent.
#[tracing::instrument(level = "debug", ret)]
pub fn tool_transcript(tool_id: &str, session_id: &str, cwd: &str) -> Option<TranscriptRef> {
    match tool_id {
        "cursor-agent" => {
            if cwd.is_empty() {
                return None;
            }
            Some(TranscriptRef {
                path: crate::tools::history::cursor::cursor_root()?
                    .join("projects")
                    .join(encode_path_str(cwd))
                    .join("agent-transcripts")
                    .join(session_id)
                    .join(format!("{session_id}.jsonl")),
                format: TranscriptFormat::CursorJsonl,
            })
        }
        // Copilot's store is keyed by session id alone — its `sessions` row records `cwd`
        // verbatim, so nothing about the path has to be re-derived from the directory.
        "copilot" => Some(TranscriptRef {
            path: crate::tools::history::copilot::copilot_root()?
                .join("session-state")
                .join(session_id)
                .join("events.jsonl"),
            format: TranscriptFormat::CopilotEvents,
        }),
        // Codex's rollout filename embeds the session id but also the session's start
        // *time*, which no marker records — so the file is found by searching the dated
        // tree newest-first rather than derived. Its `SessionStart` hook does hand over a
        // `transcript_path`, but the marker contract here is id + cwd for every tool, and
        // an exact match on the id in the filename gets the same file without widening it.
        "codex" => Some(TranscriptRef {
            path: crate::tools::history::codex::rollout_for_session(
                &crate::tools::history::codex::codex_root()?,
                session_id,
            )?,
            format: TranscriptFormat::CodexRollout,
        }),
        // Gemini's chat filename embeds the session's start *time* and only the first 8
        // characters of the id, and its parent directory is named by a first-seen-wins
        // scheme that no marker records — so, like codex, the file is found by searching
        // and confirming rather than derived. See `tools::history::gemini`.
        "gemini" => Some(TranscriptRef {
            path: crate::tools::history::gemini::chat_for_session(
                &crate::tools::history::gemini::gemini_root()?,
                session_id,
            )?,
            format: TranscriptFormat::GeminiChat,
        }),
        _ => None,
    }
}

/// A transcript file plus the record shape it speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRef {
    pub path: PathBuf,
    pub format: TranscriptFormat,
}

/// An incremental cursor over one transcript file. Starts at end-of-file (see
/// [`TranscriptTail::start_at_end`]) so [`poll`](TranscriptTail::poll) only ever surfaces
/// assistant replies written after the tail was created.
pub struct TranscriptTail {
    path: PathBuf,
    format: TranscriptFormat,
    /// Byte offset in the file already read (both emitted complete lines and any bytes
    /// folded into `partial`). The next [`poll`](Self::poll) reads from here.
    cursor: u64,
    /// Bytes of the trailing, not-yet-newline-terminated line from the last poll.
    partial: Vec<u8>,
    /// Set when `partial` was dropped for exceeding [`MAX_LINE_BYTES`] before its newline
    /// arrived — the next newline seen ends that oversized line and is discarded, rather
    /// than being (wrongly) treated as the start of a fresh, already-complete line.
    skipping: bool,
}

impl TranscriptTail {
    /// Start tailing `path` from its current end — pre-existing content (all prior history)
    /// is never returned by [`poll`](Self::poll). A missing file starts at offset 0, so it
    /// picks up everything once the file (and any assistant replies) appear.
    #[tracing::instrument(level = "debug")]
    pub fn start_at_end(path: PathBuf, format: TranscriptFormat) -> TranscriptTail {
        let cursor = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        TranscriptTail {
            path,
            format,
            cursor,
            partial: Vec::new(),
            skipping: false,
        }
    }

    /// The transcript path this tail is following.
    /// The record shape this tail is parsing.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn format(&self) -> TranscriptFormat {
        self.format
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read whatever was appended to the transcript since the last poll and return the
    /// speakable text of each complete NEW assistant record, in order. A trailing partial
    /// line (append still in flight) is buffered and completed on a later poll. A missing
    /// or unreadable file yields an empty vec and leaves the cursor untouched — the file may
    /// simply not exist yet. If the file shrank (rotated/truncated) since the last poll, the
    /// cursor resets to the new length and the buffered partial is dropped, since whatever
    /// was appended after our old cursor is no longer there.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn poll(&mut self) -> Vec<String> {
        let mut texts = Vec::new();
        let Ok(mut file) = fs::File::open(&self.path) else {
            return texts;
        };
        let Ok(meta) = file.metadata() else {
            return texts;
        };
        let len = meta.len();
        if len < self.cursor {
            self.cursor = len;
            self.partial.clear();
            self.skipping = false;
            return texts;
        }
        if len == self.cursor {
            return texts;
        }
        if file.seek(SeekFrom::Start(self.cursor)).is_err() {
            return texts;
        }
        let mut new_bytes = Vec::new();
        if file.read_to_end(&mut new_bytes).is_err() {
            return texts;
        }
        self.cursor += new_bytes.len() as u64;

        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(&new_bytes);

        let mut start = 0usize;
        while let Some(rel_nl) = data[start..].iter().position(|&b| b == b'\n') {
            let end = start + rel_nl;
            if self.skipping {
                self.skipping = false;
            } else {
                let line_bytes = &data[start..end];
                if line_bytes.len() <= MAX_LINE_BYTES {
                    if let Ok(line) = std::str::from_utf8(line_bytes) {
                        if let Some(text) = extract_assistant_text(line, self.format) {
                            texts.push(text);
                        }
                    }
                }
            }
            start = end + 1;
        }

        let tail = &data[start..];
        if tail.len() > MAX_LINE_BYTES {
            // Drop the oversized in-flight line rather than keep buffering it; `skipping`
            // remembers to discard the rest of it once its newline finally arrives.
            self.partial = Vec::new();
            self.skipping = true;
        } else {
            self.partial = tail.to_vec();
        }

        texts
    }
}

/// Pull the speakable text out of one transcript line in `format`, or `None` if the line
/// isn't a complete assistant record with text content.
///
/// Malformed JSON, non-assistant records, and assistant records carrying no text (a
/// tool-only turn) all yield `None` — the tailer speaks nothing rather than guessing.
#[tracing::instrument(level = "debug", ret)]
pub fn extract_assistant_text(line: &str, format: TranscriptFormat) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    match format {
        TranscriptFormat::ClaudeJsonl => {
            if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
                return None;
            }
            content_block_text(&v)
        }
        // Cursor marks the speaker with `role` and leaves `type` absent on message records
        // (it uses `type` for control records such as `turn_ended`), so keying on `role`
        // is what separates a reply from a turn marker.
        TranscriptFormat::CursorJsonl => {
            if v.get("role").and_then(|t| t.as_str()) != Some("assistant") {
                return None;
            }
            content_block_text(&v)
        }
        TranscriptFormat::CopilotEvents => {
            if v.get("type").and_then(|t| t.as_str()) != Some("assistant.message") {
                return None;
            }
            let text = v.get("data")?.get("content")?.as_str()?;
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        TranscriptFormat::CodexRollout => {
            if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
                return None;
            }
            let payload = v.get("payload")?;
            // `type` gates out the `function_call`, `reasoning` and tool records that share
            // the `response_item` envelope; `role` gates out the developer and user turns,
            // which are `input_text` and are not ours to speak.
            if payload.get("type").and_then(|t| t.as_str()) != Some("message")
                || payload.get("role").and_then(|r| r.as_str()) != Some("assistant")
            {
                return None;
            }
            output_text(payload)
        }
        TranscriptFormat::GeminiChat => {
            // A `$set` record has no `type` of its own, so this gate excludes it for free —
            // but it is the reason the gate is on `type` rather than on the presence of
            // `content`: the preamble `$set` carries a whole `messages` array, and reaching
            // into that would speak a reply a second time.
            if v.get("type").and_then(|t| t.as_str()) != Some("gemini") {
                return None;
            }
            // A string on an assistant record; an array of `{text}` blocks on a user one.
            // `as_str` therefore does the speaker check a second time, for free.
            let text = v.get("content")?.as_str()?;
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
    }
}

/// Space-join the `{"type":"text"}` blocks of a `message.content` array. `tool_use` /
/// `tool_result` blocks are silently skipped — they are not speech.
#[tracing::instrument(level = "debug", ret)]
fn content_block_text(v: &serde_json::Value) -> Option<String> {
    let blocks = v.get("message")?.get("content")?.as_array()?;
    let mut out = String::new();
    for block in blocks {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(t);
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Space-join the `{"type":"output_text"}` blocks of a codex message payload's `content`
/// array — the sibling of [`content_block_text`] for a payload that holds its blocks
/// directly rather than under `message`. Refusal blocks and any future non-text block kind
/// are skipped for the same reason `tool_use` is above: they are not speech.
#[tracing::instrument(level = "debug", ret)]
fn output_text(payload: &serde_json::Value) -> Option<String> {
    let blocks = payload.get("content")?.as_array()?;
    let mut out = String::new();
    for block in blocks {
        if block.get("type").and_then(|t| t.as_str()) == Some("output_text") {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(t);
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hp-speech-tailer-{}-{tag}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn assistant_line(text: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}}}\n",
            serde_json::to_string(text).unwrap()
        )
    }

    // ---- transcript_path ----

    #[test]
    fn transcript_path_honors_config_dir() {
        let marker = PaneClaudeSession {
            session_id: "sess-1".into(),
            cwd: "/home/me/dev/x".into(),
            config_dir: "/home/me/.claude-alt".into(),
        };
        let path = transcript_path(&marker).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/home/me/.claude-alt/projects/-home-me-dev-x/sess-1.jsonl")
        );
    }

    #[test]
    fn transcript_path_falls_back_to_default_account_when_config_dir_empty() {
        let marker = PaneClaudeSession {
            session_id: "sess-1".into(),
            cwd: "/home/me/dev/x".into(),
            config_dir: String::new(),
        };
        let path = transcript_path(&marker).unwrap();
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .unwrap();
        let expected = PathBuf::from(home)
            .join(".claude")
            .join("projects")
            .join("-home-me-dev-x")
            .join("sess-1.jsonl");
        assert_eq!(path, expected);
    }

    #[test]
    fn transcript_path_none_without_cwd() {
        let marker = PaneClaudeSession {
            session_id: "sess-1".into(),
            cwd: String::new(),
            config_dir: String::new(),
        };
        assert!(transcript_path(&marker).is_none());
    }

    // ---- extract_assistant_text ----

    #[test]
    fn extracts_text_blocks_and_ignores_tool_blocks() {
        let line = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[\
            {\"type\":\"text\",\"text\":\"hello\"},\
            {\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"x\",\"input\":{}},\
            {\"type\":\"text\",\"text\":\"world\"}]}}";
        assert_eq!(
            extract_assistant_text(line, TranscriptFormat::ClaudeJsonl).as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn ignores_non_assistant_records() {
        let user = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}";
        assert!(extract_assistant_text(user, TranscriptFormat::ClaudeJsonl).is_none());
        let tool_result = "{\"type\":\"tool_result\",\"content\":\"x\"}";
        assert!(extract_assistant_text(tool_result, TranscriptFormat::ClaudeJsonl).is_none());
    }

    #[test]
    fn tool_only_assistant_record_has_no_text() {
        let line = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[\
            {\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"x\",\"input\":{}}]}}";
        assert!(extract_assistant_text(line, TranscriptFormat::ClaudeJsonl).is_none());
    }

    #[test]
    fn malformed_json_is_none() {
        assert!(extract_assistant_text("not json", TranscriptFormat::ClaudeJsonl).is_none());
    }

    // ---- extract_assistant_text: cursor-agent ----

    #[test]
    fn cursor_records_are_keyed_on_role_not_type() {
        // Cursor writes no `type` on a message record, so a format that looked for one
        // would speak nothing at all.
        let line = "{\"role\":\"assistant\",\"message\":{\"content\":[\
            {\"type\":\"text\",\"text\":\"cursor\"},\
            {\"type\":\"text\",\"text\":\"speaks\"}]}}";
        assert_eq!(
            extract_assistant_text(line, TranscriptFormat::CursorJsonl).as_deref(),
            Some("cursor speaks")
        );
    }

    #[test]
    fn cursor_control_records_are_not_speech() {
        // `type` on a cursor record marks a turn boundary, never a reply.
        let turn = "{\"type\":\"turn_ended\",\"status\":\"success\"}";
        assert!(extract_assistant_text(turn, TranscriptFormat::CursorJsonl).is_none());
        let user =
            "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}";
        assert!(extract_assistant_text(user, TranscriptFormat::CursorJsonl).is_none());
    }

    // ---- extract_assistant_text: copilot ----

    #[test]
    fn copilot_reply_text_is_a_flat_string() {
        let line = "{\"type\":\"assistant.message\",\"data\":{\"content\":\"ok\"}}";
        assert_eq!(
            extract_assistant_text(line, TranscriptFormat::CopilotEvents).as_deref(),
            Some("ok")
        );
    }

    #[test]
    fn copilot_non_message_events_are_not_speech() {
        for line in [
            "{\"type\":\"session.start\",\"data\":{}}",
            "{\"type\":\"user.message\",\"data\":{\"content\":\"hi\"}}",
            "{\"type\":\"assistant.turn_start\",\"data\":{}}",
            "{\"type\":\"assistant.message\",\"data\":{\"content\":\"\"}}",
        ] {
            assert!(
                extract_assistant_text(line, TranscriptFormat::CopilotEvents).is_none(),
                "{line} is not a spoken reply"
            );
        }
    }

    // ---- extract_assistant_text: codex ----

    #[test]
    fn codex_reply_text_comes_from_the_response_item() {
        // Verbatim from a real `codex exec` rollout (0.151.0), trimmed of its timestamp.
        let line = r#"{"type":"response_item","ordinal":9,"payload":{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"Hello from the stub."}]}}"#;
        assert_eq!(
            extract_assistant_text(line, TranscriptFormat::CodexRollout).as_deref(),
            Some("Hello from the stub.")
        );
    }

    #[test]
    fn codex_joins_multiple_output_text_blocks() {
        let line = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"One."},{"type":"refusal","refusal":"no"},{"type":"output_text","text":"Two."}]}}"#;
        assert_eq!(
            extract_assistant_text(line, TranscriptFormat::CodexRollout).as_deref(),
            Some("One. Two.")
        );
    }

    #[test]
    fn codex_says_each_reply_once_and_not_three_times() {
        // The load-bearing negative case. A codex rollout carries the SAME reply text in
        // three records; only the `response_item` above is read. If either of these ever
        // started matching, every codex reply would be spoken two or three times over.
        let item_completed = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"msg_1","content":[{"type":"Text","text":"Hello from the stub."}]}}}"#;
        let task_complete = r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Hello from the stub.","duration_ms":91}}"#;
        assert!(extract_assistant_text(item_completed, TranscriptFormat::CodexRollout).is_none());
        assert!(extract_assistant_text(task_complete, TranscriptFormat::CodexRollout).is_none());
    }

    #[test]
    fn codex_non_assistant_records_are_not_speech() {
        for line in [
            // The developer preamble and the human's own prompt: same envelope, same
            // `message` payload type, different role — and `input_text`, not `output_text`.
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"You are Codex."}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"say hello"}]}}"#,
            // Tool calls and reasoning share the envelope with real messages.
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}"}}"#,
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":[]}}"#,
            // The session envelope and the non-`response_item` line kinds.
            r#"{"type":"session_meta","payload":{"id":"01a0","cli_version":"0.151.0"}}"#,
            r#"{"type":"turn_context","payload":{"cwd":"/tmp"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            // An assistant message with no text block yields nothing rather than "".
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}"#,
        ] {
            assert!(
                extract_assistant_text(line, TranscriptFormat::CodexRollout).is_none(),
                "{line} is not a spoken reply"
            );
        }
    }

    #[test]
    fn a_format_only_reads_its_own_shape() {
        // The same line under the wrong format yields nothing rather than garbage —
        // which is what makes re-tailing on a format change safe.
        let claude = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}";
        assert!(extract_assistant_text(claude, TranscriptFormat::CopilotEvents).is_none());
        assert!(extract_assistant_text(claude, TranscriptFormat::CursorJsonl).is_none());
        assert!(extract_assistant_text(claude, TranscriptFormat::CodexRollout).is_none());
        let codex = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#;
        assert!(extract_assistant_text(codex, TranscriptFormat::ClaudeJsonl).is_none());
        assert!(extract_assistant_text(codex, TranscriptFormat::CursorJsonl).is_none());
        assert!(extract_assistant_text(codex, TranscriptFormat::CopilotEvents).is_none());
        assert!(extract_assistant_text(codex, TranscriptFormat::GeminiChat).is_none());
        let gemini = r#"{"id":"86d5","type":"gemini","content":"hi","thoughts":[]}"#;
        assert!(extract_assistant_text(gemini, TranscriptFormat::ClaudeJsonl).is_none());
        assert!(extract_assistant_text(gemini, TranscriptFormat::CursorJsonl).is_none());
        assert!(extract_assistant_text(gemini, TranscriptFormat::CopilotEvents).is_none());
        assert!(extract_assistant_text(gemini, TranscriptFormat::CodexRollout).is_none());
    }

    // ---- gemini ----

    #[test]
    fn gemini_reply_text_comes_from_the_message_record() {
        // Shape captured from a real gemini 0.58.0 run: `content` is a bare string on an
        // assistant record, not the block array every other format uses.
        let line = r#"{"id":"86d50b21-9dbb-4c50-8ba6-99f2b0d2f6b7","timestamp":"2026-09-02T09:39:06.826Z","type":"gemini","content":"Hello from the stub. Second sentence.","thoughts":[],"tokens":{"total":3},"model":"stub"}"#;
        assert_eq!(
            extract_assistant_text(line, TranscriptFormat::GeminiChat).as_deref(),
            Some("Hello from the stub. Second sentence.")
        );
    }

    #[test]
    fn gemini_set_records_are_never_spoken() {
        // The load-bearing negative case, and gemini's analogue of codex's triple record.
        // The chat file is a mutation log: `$set` records rewrite session state, and the
        // preamble one carries an entire `messages` array. Speaking into a `$set` would
        // re-speak replies already spoken — and the big one would dictate the session
        // context preamble at the human.
        let preamble = r#"{"$set": {"messages": [{"id":"a","type":"user","content":[{"text":"<session_context>"}]},{"id":"b","type":"gemini","content":"already said this"}]}}"#;
        let touch = r#"{"$set": {"lastUpdated":"2026-09-02T09:39:06.830Z"}}"#;
        assert!(extract_assistant_text(preamble, TranscriptFormat::GeminiChat).is_none());
        assert!(extract_assistant_text(touch, TranscriptFormat::GeminiChat).is_none());
    }

    #[test]
    fn gemini_non_assistant_records_are_not_speech() {
        for line in [
            // The human's turn: same file, same `content` key, but an array of `{text}`
            // blocks rather than a string — so this must fail the type gate, and would fail
            // `as_str` even if it did not.
            r#"{"id":"67cc","timestamp":"2026-09-02T09:39:05.101Z","type":"user","content":[{"text":"say hi"}]}"#,
            // The header line gemini writes before any message.
            r#"{"sessionId":"70c6bdeb-e601-4fe5-8349-45d82a818ea7","projectHash":"cb3c9bf1","kind":"main"}"#,
            // An assistant record with nothing in it yields nothing rather than "".
            r#"{"id":"86d5","type":"gemini","content":""}"#,
            // A future record type must be silent, not spoken on the strength of `content`.
            r#"{"id":"9f01","type":"tool","content":"ran ls"}"#,
        ] {
            assert!(
                extract_assistant_text(line, TranscriptFormat::GeminiChat).is_none(),
                "{line} is not a spoken reply"
            );
        }
    }

    // ---- tool_transcript ----

    #[test]
    fn cursor_transcript_is_nested_under_the_encoded_cwd() {
        let Some(r) = tool_transcript("cursor-agent", "sid-1", "/tmp/proj") else {
            return; // no cursor install on this machine
        };
        assert_eq!(r.format, TranscriptFormat::CursorJsonl);
        let s = r.path.to_string_lossy().to_string();
        assert!(s.ends_with("agent-transcripts/sid-1/sid-1.jsonl"), "{s}");
        assert!(s.contains(&encode_path_str("/tmp/proj")), "{s}");
    }

    #[test]
    fn cursor_needs_a_cwd_to_find_the_project_dir() {
        assert!(tool_transcript("cursor-agent", "sid-1", "").is_none());
    }

    #[test]
    fn copilot_transcript_is_keyed_on_the_session_id_alone() {
        let Some(r) = tool_transcript("copilot", "sid-2", "") else {
            return; // no copilot install on this machine
        };
        assert_eq!(r.format, TranscriptFormat::CopilotEvents);
        let s = r.path.to_string_lossy().to_string();
        assert!(s.ends_with("session-state/sid-2/events.jsonl"), "{s}");
    }

    #[test]
    fn a_tool_with_no_transcript_store_resolves_to_nothing() {
        // aider, goose, plain shells: no store, so Talk stays silent for them rather than
        // being spoken from a guess. (aider does keep a log, but a Markdown one whose
        // records span lines — `extract_assistant_text` is per-line, so it is not tailable
        // by this machinery as written.)
        for tool in ["aider", "goose", ""] {
            assert!(
                tool_transcript(tool, "sid", "/tmp/proj").is_none(),
                "{tool}"
            );
        }
    }

    // ---- TranscriptTail ----

    #[test]
    fn start_at_end_skips_preexisting_content() {
        let dir = temp_dir("preexisting");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, assistant_line("old reply")).unwrap();

        let mut tail = TranscriptTail::start_at_end(path.clone(), TranscriptFormat::ClaudeJsonl);
        assert!(
            tail.poll().is_empty(),
            "pre-existing content never returned"
        );

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(assistant_line("new reply").as_bytes()).unwrap();
        drop(f);

        assert_eq!(tail.poll(), vec!["new reply".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_at_end_missing_file_starts_at_zero() {
        let dir = temp_dir("missing");
        let path = dir.join("missing.jsonl");
        let mut tail = TranscriptTail::start_at_end(path.clone(), TranscriptFormat::ClaudeJsonl);
        assert!(tail.poll().is_empty(), "still missing, nothing to read");

        std::fs::write(&path, assistant_line("first reply")).unwrap();
        assert_eq!(tail.poll(), vec!["first reply".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appended_record_returned_exactly_once() {
        let dir = temp_dir("once");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone(), TranscriptFormat::ClaudeJsonl);

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(assistant_line("only once").as_bytes()).unwrap();
        drop(f);

        assert_eq!(tail.poll(), vec!["only once".to_string()]);
        assert!(tail.poll().is_empty(), "not returned again on next poll");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_line_then_completion() {
        let dir = temp_dir("partial");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone(), TranscriptFormat::ClaudeJsonl);

        let full = assistant_line("split reply");
        let (first_half, second_half) = full.split_at(full.len() / 2);

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(first_half.as_bytes()).unwrap();
        f.flush().unwrap();
        assert!(
            tail.poll().is_empty(),
            "partial line withheld until newline"
        );

        f.write_all(second_half.as_bytes()).unwrap();
        drop(f);
        assert_eq!(tail.poll(), vec!["split reply".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_and_tool_result_records_ignored() {
        let dir = temp_dir("ignored");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone(), TranscriptFormat::ClaudeJsonl);

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n")
            .unwrap();
        f.write_all(b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"content\":\"x\"}]}}\n")
            .unwrap();
        f.write_all(assistant_line("real reply").as_bytes())
            .unwrap();
        drop(f);

        assert_eq!(tail.poll(), vec!["real reply".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_use_blocks_ignored_text_blocks_kept() {
        let dir = temp_dir("mixed");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone(), TranscriptFormat::ClaudeJsonl);

        let line = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[\
            {\"type\":\"text\",\"text\":\"before\"},\
            {\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"x\",\"input\":{}},\
            {\"type\":\"text\",\"text\":\"after\"}]}}\n";
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(line.as_bytes()).unwrap();
        drop(f);

        assert_eq!(tail.poll(), vec!["before after".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shrink_resets_cursor() {
        let dir = temp_dir("shrink");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, assistant_line("first")).unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone(), TranscriptFormat::ClaudeJsonl);

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(assistant_line("second").as_bytes()).unwrap();
        drop(f);
        assert_eq!(tail.poll(), vec!["second".to_string()]);

        // Truncate the file back down below the tail's cursor (rotation/reset).
        std::fs::write(&path, assistant_line("after shrink")).unwrap();
        assert!(
            tail.poll().is_empty(),
            "shrink resets cursor to new EOF, doesn't re-surface the new content as if old"
        );

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(assistant_line("post-shrink reply").as_bytes())
            .unwrap();
        drop(f);
        assert_eq!(tail.poll(), vec!["post-shrink reply".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multiple_appends_preserve_order() {
        let dir = temp_dir("order");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone(), TranscriptFormat::ClaudeJsonl);

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(assistant_line("one").as_bytes()).unwrap();
        f.write_all(assistant_line("two").as_bytes()).unwrap();
        drop(f);
        assert_eq!(tail.poll(), vec!["one".to_string(), "two".to_string()]);

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(assistant_line("three").as_bytes()).unwrap();
        drop(f);
        assert_eq!(tail.poll(), vec!["three".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_accessor_returns_the_tailed_path() {
        let dir = temp_dir("path-acc");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let tail = TranscriptTail::start_at_end(path.clone(), TranscriptFormat::ClaudeJsonl);
        assert_eq!(tail.path(), path.as_path());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_single_line_is_skipped() {
        let dir = temp_dir("oversize");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone(), TranscriptFormat::ClaudeJsonl);

        // A single line far past MAX_LINE_BYTES, followed by a normal record.
        let huge = assistant_line(&"x".repeat(MAX_LINE_BYTES + 1024));
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(huge.as_bytes()).unwrap();
        f.write_all(assistant_line("normal reply").as_bytes())
            .unwrap();
        drop(f);

        assert_eq!(
            tail.poll(),
            vec!["normal reply".to_string()],
            "oversized line skipped, normal line after it still surfaces"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
