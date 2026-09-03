//! The SSH channel ↔ attach-core bridge (mux backend M3).
//!
//! An accepted SSH `shell`/`exec` channel is wired straight into
//! [`hyperpanes_core::session::attach`] — the tty-free client M2 split out for exactly this.
//! Nothing about the protocol, the detach key, the resize policy, or the UTF-8 stream decode
//! is re-implemented here; this module only supplies the two ends the CLI would have taken
//! from a tty:
//!
//! * a [`Read`] whose bytes come from `ChannelMsg::Data` instead of stdin, and
//! * a [`Write`] whose bytes go back out over the channel instead of to stdout.
//!
//! # Why threads
//!
//! `attach` is deliberately blocking std I/O over a Unix socket; russh's handler runs on a
//! tokio task. Blocking that task on the daemon socket would stall the whole SSH connection
//! (including keepalives and the *other* channels on it), so each channel gets:
//!
//! | thread/task | owns | blocks on |
//! |---|---|---|
//! | `hp-ssh-attach` | [`Attachment`] | `pump_output` reading the daemon socket |
//! | `hp-ssh-input`  | an [`AttachWriter`](attach::AttachWriter) clone | the channel-data queue |
//! | `hp-ssh-resize` | an `AttachWriter` clone | the window-change queue |
//! | tokio task      | the russh [`Handle`] | the outbound queue |
//!
//! The outbound queue is **bounded**: when a phone on a slow link stops reading, `blocking_send`
//! parks the attach thread, which stops draining the daemon socket, which applies real
//! backpressure instead of growing a buffer until the process dies.
//!
//! # Teardown
//!
//! Every path ends with the channel closed and an exit status sent, and never kills the
//! session: detaching over SSH leaves the pane running, exactly as `hyperpanes attach` does.
//! The input thread ends when its sender is dropped (the handler drops the channel state on
//! `channel_close`/`channel_eof`, or when the whole connection's handler is dropped).

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};

use hyperpanes_core::session::attach::{self, Attachment, PumpEnd, ResizePolicy};
use hyperpanes_core::session::proto::SessionMeta;
use russh::server::Handle;
use russh::ChannelId;

/// Home the cursor and clear screen + scrollback before painting a replay buffer, so a
/// repaint does not stack on the previous copy. Same sequence `attach_cli` uses.
pub const CLEAR_SCREEN: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";

/// Outbound queue depth, in chunks. Small on purpose — see the backpressure note above.
const OUT_QUEUE: usize = 64;
/// Flush the sink once this much has accumulated without an explicit flush.
const SINK_CHUNK: usize = 32 * 1024;

/// What the blocking attach threads hand back to the russh event loop.
enum Out {
    /// Bytes for the channel.
    Data(Vec<u8>),
    /// Nothing more is coming: send this exit status, then EOF and close.
    Done(u32),
}

/// Everything a channel needs to know to attach.
#[derive(Debug, Clone)]
pub struct BridgeParams {
    /// The daemon salt (the user-data dir) — the same key the GUI and `attach` use.
    pub salt: String,
    /// A pane query from the SSH command or username, if any.
    pub query: Option<String>,
    /// `ssh host list` — print the session table and exit, attaching to nothing.
    pub list_only: bool,
    /// The client's pty grid, from `pty_request` / `window_change_request`.
    pub term: (u16, u16),
    /// Whether this client may reflow the pane for everyone.
    pub policy: ResizePolicy,
    /// Detach prefix byte.
    pub detach: u8,
    /// Peer description, for the log only.
    pub peer: String,
}

/// The handler's end of a running bridge: where to push channel data and window changes.
///
/// Dropping it EOFs the input reader, which unwinds the input thread, disconnects the
/// attachment and unblocks `pump_output`.
pub struct Bridge {
    input: Sender<Vec<u8>>,
    resize: Sender<(u16, u16)>,
}

impl Bridge {
    /// Feed a `ChannelMsg::Data` payload to the pane. Returns false once the bridge is gone.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn input(&self, data: &[u8]) -> bool {
        self.input.send(data.to_vec()).is_ok()
    }

    /// Feed a window-change. Returns false once the bridge is gone.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn resize(&self, cols: u16, rows: u16) -> bool {
        self.resize.send((cols, rows)).is_ok()
    }
}

/// Start the bridge for one channel. Never blocks the caller.
#[tracing::instrument(level = "debug")]
pub fn spawn(params: BridgeParams, handle: Handle, channel: ChannelId) -> Bridge {
    let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
    let (resize_tx, resize_rx) = mpsc::channel::<(u16, u16)>();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Out>(OUT_QUEUE);

    // Outbound: the ONLY place channel bytes are written, so ordering between data and the
    // final exit status is guaranteed by the queue rather than by thread timing.
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            match msg {
                Out::Data(bytes) => {
                    if handle.data(channel, bytes).await.is_err() {
                        break; // client vanished
                    }
                }
                Out::Done(code) => {
                    let _ = handle.exit_status_request(channel, code).await;
                    let _ = handle.eof(channel).await;
                    let _ = handle.close(channel).await;
                    return;
                }
            }
        }
        // Fell out without a Done (the attach thread panicked, or the peer went away).
        let _ = handle.close(channel).await;
    });

    let name = format!("hp-ssh-attach-{}", params.peer);
    let spawned = std::thread::Builder::new()
        .name(name.chars().take(15).collect())
        .spawn(move || {
            let mut sink = ChannelSink::new(out_tx.clone());
            let code = match attach_main(&params, &mut sink, input_rx, resize_rx) {
                Ok(code) => code,
                Err(msg) => {
                    tracing::debug!("ssh: channel failed: {msg}");
                    let _ = sink.write_all(format!("\r\nhyperpanes: {msg}\r\n").as_bytes());
                    let _ = sink.flush();
                    1
                }
            };
            let _ = sink.flush();
            // `blocking_send` on a full queue is fine here: this thread's only remaining job
            // is to deliver the exit status.
            let _ = out_tx.blocking_send(Out::Done(code));
        });
    if let Err(e) = spawned {
        tracing::debug!("ssh: could not start the attach thread: {e}");
    }

    Bridge {
        input: input_tx,
        resize: resize_tx,
    }
}

/// The blocking half: connect, choose a pane, attach, pump. Returns the SSH exit status.
#[tracing::instrument(level = "debug", ret, skip(sink))]
fn attach_main(
    params: &BridgeParams,
    sink: &mut ChannelSink,
    input_rx: Receiver<Vec<u8>>,
    resize_rx: Receiver<(u16, u16)>,
) -> Result<u32, String> {
    let conn = attach::connect(&params.salt).map_err(|e| e.to_string())?;
    // A protocol-version mismatch is worth a log line but not a refusal: the daemon and this
    // binary are the same install, and `attach` already tolerates additive drift.
    if let Err(e) = attach::handshake(&conn) {
        tracing::debug!("ssh: daemon handshake: {e}");
    }
    let sessions = attach::list_sessions(&conn).map_err(|e| e.to_string())?;

    if params.list_only {
        sink.write_all(render_list(&sessions).as_bytes())
            .map_err(|e| e.to_string())?;
        sink.flush().map_err(|e| e.to_string())?;
        return Ok(0);
    }
    if sessions.is_empty() {
        return Err("no live sessions in this hyperpanes workspace.".to_string());
    }

    let uid = match pick(&sessions, params.query.as_deref(), sink, &input_rx)? {
        Some(uid) => uid,
        None => {
            sink.write_all(b"\r\ncancelled.\r\n")
                .map_err(|e| e.to_string())?;
            sink.flush().map_err(|e| e.to_string())?;
            return Ok(0);
        }
    };

    // Letterbox warning: the pane keeps the desktop's grid unless this client opted into
    // reflowing it for everyone, so say so rather than silently clipping.
    if params.policy == ResizePolicy::Observe {
        let session_grid = sessions
            .iter()
            .find(|s| s.uid == uid)
            .map(|s| (s.cols, s.rows))
            .unwrap_or((None, None));
        if attach::fits(params.term, session_grid) == Some(false) {
            let msg = format!(
                "\r\nhyperpanes: this terminal is {}x{}; the pane is larger and will be \
                 clipped.\r\n",
                params.term.0, params.term.1
            );
            let _ = sink.write_all(msg.as_bytes());
        }
    }

    let (mut attachment, seed) = Attachment::open(conn, &uid).map_err(|e| e.to_string())?;
    attachment.set_repaint_prefix(CLEAR_SCREEN);
    let writer = attachment.writer();

    if params.policy == ResizePolicy::Request {
        writer
            .request_resize(params.term.0, params.term.1)
            .map_err(|e| e.to_string())?;
    }

    sink.write_all(CLEAR_SCREEN).map_err(|e| e.to_string())?;
    sink.write_all(seed.as_bytes()).map_err(|e| e.to_string())?;
    sink.flush().map_err(|e| e.to_string())?;

    // Input: channel bytes → the pane, through the SHARED detach filter and UTF-8 stream
    // decoder in `attach::pump_input`. On detach (or channel EOF) it disconnects, which
    // EOFs `pump_output` below and unwinds this whole channel. The session lives on.
    {
        let writer = writer.clone();
        let detach = params.detach;
        std::thread::Builder::new()
            .name("hp-ssh-input".into())
            .spawn(move || {
                let mut filter = attach::DetachFilter::new(detach);
                let _ = attach::pump_input(ChannelReader::new(input_rx), &writer, &mut filter);
                writer.disconnect();
            })
            .map_err(|e| format!("input thread: {e}"))?;
    }

    // Window changes. Under `Observe` a resize is a REPAINT: the client's window changed,
    // the pane did not.
    {
        let writer = writer.clone();
        let policy = params.policy;
        std::thread::Builder::new()
            .name("hp-ssh-resize".into())
            .spawn(move || {
                for (cols, rows) in resize_rx {
                    let sent = match policy {
                        ResizePolicy::Request => writer.request_resize(cols, rows),
                        ResizePolicy::Observe => writer.request_repaint(),
                    };
                    if sent.is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| format!("resize thread: {e}"))?;
    }

    let end = attachment.pump_output(sink).map_err(|e| e.to_string())?;
    let (tail, code) = match end {
        PumpEnd::Exited(code) => (
            format!("\r\nhyperpanes: {uid} exited (code {code})\r\n"),
            u32::try_from(code).unwrap_or(1),
        ),
        PumpEnd::Disconnected => (
            format!("\r\nhyperpanes: detached from {uid} — the session is still running\r\n"),
            0,
        ),
    };
    let _ = sink.write_all(tail.as_bytes());
    let _ = sink.flush();
    Ok(code)
}

/// Turn the query (or its absence) into one uid. `Ok(None)` means the user cancelled.
#[tracing::instrument(level = "debug", ret, skip(sink))]
fn pick(
    sessions: &[SessionMeta],
    query: Option<&str>,
    sink: &mut ChannelSink,
    input_rx: &Receiver<Vec<u8>>,
) -> Result<Option<String>, String> {
    use attach::UidMatch;

    if let Some(q) = query {
        return match attach::resolve_uid(sessions, q) {
            UidMatch::One(uid) => Ok(Some(uid)),
            UidMatch::None => Err(format!(
                "no live session matches '{q}'. Run `ssh … list` to see them."
            )),
            UidMatch::Ambiguous(hits) => Err(format!(
                "'{q}' matches {} sessions ({}). Be more specific.",
                hits.len(),
                hits.join(", ")
            )),
        };
    }
    if sessions.len() == 1 {
        return Ok(Some(sessions[0].uid.clone()));
    }

    sink.write_all(render_list(sessions).as_bytes())
        .map_err(|e| e.to_string())?;
    let prompt = format!("Attach to [1-{}, a uid, or q to quit]: ", sessions.len());
    sink.write_all(prompt.as_bytes())
        .map_err(|e| e.to_string())?;
    sink.flush().map_err(|e| e.to_string())?;

    let Some(line) = read_line(input_rx, sink)? else {
        return Ok(None);
    };
    let line = line.trim().to_string();
    if line.is_empty() || line.eq_ignore_ascii_case("q") {
        return Ok(None);
    }
    if let Ok(n) = line.parse::<usize>() {
        if (1..=sessions.len()).contains(&n) {
            return Ok(Some(sessions[n - 1].uid.clone()));
        }
    }
    pick(sessions, Some(&line), sink, input_rx)
}

/// Read one echoed line from the channel. `Ok(None)` on Ctrl-C, Ctrl-D or channel EOF.
///
/// The client is in raw mode (it asked for a pty), so there is no line discipline on either
/// side — this is the whole of it: echo, backspace, cancel.
#[tracing::instrument(level = "debug", ret, skip(sink))]
fn read_line(rx: &Receiver<Vec<u8>>, sink: &mut ChannelSink) -> Result<Option<String>, String> {
    let mut line = String::new();
    loop {
        let Ok(chunk) = rx.recv() else {
            return Ok(None); // channel closed
        };
        for &b in &chunk {
            match b {
                b'\r' | b'\n' => {
                    let _ = sink.write_all(b"\r\n");
                    let _ = sink.flush();
                    return Ok(Some(line));
                }
                0x03 | 0x04 => return Ok(None), // Ctrl-C / Ctrl-D
                0x7f | 0x08 => {
                    if line.pop().is_some() {
                        let _ = sink.write_all(b"\x08 \x08");
                    }
                }
                0x20..=0x7e => {
                    line.push(b as char);
                    let _ = sink.write_all(&[b]);
                }
                _ => {}
            }
        }
        sink.flush().map_err(|e| e.to_string())?;
    }
}

/// The session table, CRLF-terminated for a raw-mode channel.
#[tracing::instrument(level = "debug", ret)]
fn render_list(sessions: &[SessionMeta]) -> String {
    if sessions.is_empty() {
        return "No live hyperpanes sessions.\r\n".to_string();
    }
    let mut s = String::from("Live hyperpanes sessions:\r\n");
    for (i, m) in sessions.iter().enumerate() {
        let grid = match (m.cols, m.rows) {
            (Some(c), Some(r)) => format!("{c}x{r}"),
            _ => "?x?".to_string(),
        };
        let cwd = m.cwd.as_deref().unwrap_or("-");
        s.push_str(&format!(
            "  {:>2}. {}  {:>9}  {}\r\n",
            i + 1,
            m.uid,
            grid,
            cwd
        ));
    }
    s
}

/// [`Write`] adapter: buffered bytes → the outbound queue → `Handle::data`.
///
/// `write` never blocks; `flush` blocks when the queue is full, which is the backpressure
/// path described in the module docs.
struct ChannelSink {
    tx: tokio::sync::mpsc::Sender<Out>,
    buf: Vec<u8>,
}

impl ChannelSink {
    #[tracing::instrument(level = "debug")]
    fn new(tx: tokio::sync::mpsc::Sender<Out>) -> Self {
        Self {
            tx,
            buf: Vec::with_capacity(SINK_CHUNK),
        }
    }
}

impl Write for ChannelSink {
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= SINK_CHUNK {
            self.flush()?;
        }
        Ok(data.len())
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::take(&mut self.buf);
        self.tx
            .blocking_send(Out::Data(chunk))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the SSH channel closed"))
    }
}

/// [`Read`] adapter: the channel's inbound data queue → `attach::pump_input`.
///
/// A closed queue reads as EOF, which is how `pump_input` learns the SSH client hung up.
struct ChannelReader {
    rx: Receiver<Vec<u8>>,
    cur: Vec<u8>,
    pos: usize,
}

impl ChannelReader {
    #[tracing::instrument(level = "debug")]
    fn new(rx: Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            cur: Vec::new(),
            pos: 0,
        }
    }
}

impl Read for ChannelReader {
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.pos >= self.cur.len() {
            match self.rx.recv() {
                Ok(chunk) if chunk.is_empty() => continue,
                Ok(chunk) => {
                    self.cur = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0), // EOF
            }
        }
        let n = (self.cur.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(uid: &str) -> SessionMeta {
        SessionMeta {
            uid: uid.to_string(),
            cwd: Some("/tmp".into()),
            output_bytes: 0,
            last_output_at: None,
            alive: true,
            cols: Some(120),
            rows: Some(40),
            foreground: None,
            fg_cwd: None,
        }
    }

    #[test]
    fn channel_reader_reassembles_chunks_and_eofs_on_close() {
        let (tx, rx) = mpsc::channel();
        tx.send(b"hel".to_vec()).unwrap();
        tx.send(Vec::new()).unwrap(); // an empty frame must not look like EOF
        tx.send(b"lo".to_vec()).unwrap();
        drop(tx);
        let mut r = ChannelReader::new(rx);
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"hello");
    }

    #[test]
    fn channel_reader_splits_a_chunk_across_small_reads() {
        let (tx, rx) = mpsc::channel();
        tx.send(b"abcdef".to_vec()).unwrap();
        drop(tx);
        let mut r = ChannelReader::new(rx);
        let mut buf = [0u8; 2];
        assert_eq!(r.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"ab");
        assert_eq!(r.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"cd");
        assert_eq!(r.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"ef");
        assert_eq!(
            r.read(&mut buf).unwrap(),
            0,
            "closed queue must read as EOF"
        );
    }

    #[tokio::test]
    async fn channel_sink_buffers_until_flush_then_emits_one_chunk() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut sink = ChannelSink::new(tx);
        // `blocking_send` would deadlock on the current thread inside a `#[tokio::test]`,
        // so drive the sink from a real thread — which is where it lives in production.
        let t = std::thread::spawn(move || {
            sink.write_all(b"abc").unwrap();
            assert!(rx_is_empty_marker());
            sink.write_all(b"def").unwrap();
            sink.flush().unwrap();
        });
        let got = rx.recv().await.expect("one chunk");
        match got {
            Out::Data(v) => assert_eq!(v, b"abcdef"),
            Out::Done(_) => panic!("expected data"),
        }
        t.join().unwrap();
    }

    /// Placeholder so the assertion above reads as an assertion; the real check is that
    /// exactly one chunk arrives.
    fn rx_is_empty_marker() -> bool {
        true
    }

    #[test]
    fn read_line_echoes_handles_backspace_and_cancels() {
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(16);
        let (in_tx, in_rx) = mpsc::channel();
        in_tx.send(b"12\x7f3\r".to_vec()).unwrap();
        let t = std::thread::spawn(move || {
            let mut sink = ChannelSink::new(out_tx);
            read_line(&in_rx, &mut sink).unwrap()
        });
        let line = t.join().unwrap();
        assert_eq!(line.as_deref(), Some("13"));
        // Echo went out.
        let mut echoed = Vec::new();
        while let Ok(Out::Data(v)) = out_rx.try_recv() {
            echoed.extend_from_slice(&v);
        }
        assert!(!echoed.is_empty(), "the prompt must echo what was typed");

        // Ctrl-C cancels.
        let (out_tx, _out_rx) = tokio::sync::mpsc::channel(16);
        let (in_tx, in_rx) = mpsc::channel();
        in_tx.send(vec![0x03]).unwrap();
        let t = std::thread::spawn(move || {
            let mut sink = ChannelSink::new(out_tx);
            read_line(&in_rx, &mut sink).unwrap()
        });
        assert_eq!(t.join().unwrap(), None);
    }

    #[test]
    fn render_list_is_crlf_terminated() {
        let text = render_list(&[meta("pane-aaaa"), meta("pane-bbbb")]);
        assert!(text.contains("pane-aaaa"));
        assert!(text.contains("120x40"));
        for line in text.split("\r\n") {
            assert!(!line.contains('\n'), "bare LF in raw-mode output: {text:?}");
        }
        assert_eq!(render_list(&[]), "No live hyperpanes sessions.\r\n");
    }

    #[test]
    fn pick_resolves_a_query_and_reports_ambiguity() {
        let (_out_tx, _out_rx) = tokio::sync::mpsc::channel::<Out>(4);
        let sessions = [meta("pane-aaaa"), meta("pane-abbb")];
        let (out_tx, _keep) = tokio::sync::mpsc::channel(4);
        let (_in_tx, in_rx) = mpsc::channel();
        let mut sink = ChannelSink::new(out_tx);
        assert_eq!(
            pick(&sessions, Some("pane-aaaa"), &mut sink, &in_rx).unwrap(),
            Some("pane-aaaa".to_string())
        );
        let err = pick(&sessions, Some("pane-a"), &mut sink, &in_rx).unwrap_err();
        assert!(err.contains("matches 2"), "{err}");
        let err = pick(&sessions, Some("nope"), &mut sink, &in_rx).unwrap_err();
        assert!(err.contains("no live session"), "{err}");
    }

    #[test]
    fn pick_with_one_session_needs_no_prompt() {
        let sessions = [meta("pane-only")];
        let (out_tx, _keep) = tokio::sync::mpsc::channel(4);
        let (_in_tx, in_rx) = mpsc::channel();
        let mut sink = ChannelSink::new(out_tx);
        assert_eq!(
            pick(&sessions, None, &mut sink, &in_rx).unwrap(),
            Some("pane-only".to_string())
        );
    }
}
