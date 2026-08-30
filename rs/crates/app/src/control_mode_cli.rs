//! `hyperpanes control-mode` — the **M4 tmux control-mode (`-CC`) server**
//! (`docs/mux-backend-plan.md`).
//!
//! iTerm2 and the mobile tmux clients (Blink, Prompt, Termius…) do not scrape a terminal:
//! they run `tmux -CC` and speak a line-oriented control protocol over the SSH channel,
//! rendering each tmux pane as a native tab. This subcommand makes hyperpanes answer that
//! protocol, so a stock `ssh host hyperpanes control-mode` presents the desktop's live panes
//! to those clients as tmux windows.
//!
//! ## Split with `core`
//! Everything protocol-shaped — the `%begin`/`%end` guard blocks, `%output` escaping, the
//! layout strings and their checksums, the uid → tmux-id mapping, the command dispatcher —
//! lives in [`hyperpanes_core::session::control_mode`] as a pure, I/O-free state machine, so
//! M3's embedded SSH server can drive the identical code on a channel instead of on stdio.
//! What is left in this file is only the transport: connect to the daemon, attach to every
//! pane, turn daemon events into calls on the state machine, and write its lines out.
//!
//! ## Wire framing
//! Every emitted line is written verbatim plus a single `\n` — never `\r\n`; that is exactly
//! what tmux's own `control.c:control_write_line` does, and the `\r` in a captured transcript
//! is the tty's `ONLCR`, not part of the protocol. The two DCS wrapper strings are the sole
//! exception (no newline at all); [`needs_newline`] is the shared rule.
//!
//! ## What the daemon can and cannot give us
//! * The daemon's event feed carries pane output as a **`String`**
//!   ([`SessionEvent::Data`]), already UTF-8-sanitized by the pty reader, so `%output`
//!   payloads are the UTF-8 bytes of that string rather than the pty's raw bytes. Escaping
//!   is byte-exact from there down.
//! * Pane **membership** does arrive as a push: the daemon sends `SessionsChanged` — a full
//!   `SessionMeta` snapshot — whenever a session is created, killed or adopted (M7), so an
//!   appearing or vanishing pane reaches the client immediately.
//! * A **resize** still has no push event, so this loop also polls `ListSessions` every
//!   [`POLL`]. Both paths feed the same differ: new uids become `%window-add`, vanished uids
//!   `%window-close`, changed grids `%layout-change`. Running both is safe because the diff
//!   is against `seen` — a snapshot that changes nothing emits nothing.
//!
//! ## Resize policy
//! Same decision as `attach` (and for the same reason): by default a control-mode client
//! **observes** the desktop's grid and is told the true layout, rather than silently
//! reflowing everyone else's panes to a phone screen. `refresh-client -C` is acknowledged
//! either way — iTerm2 sends it unconditionally during attach and treats an error as a fatal
//! protocol failure — but only `--resize` makes it act.

use hyperpanes_core::session::attach::ResizePolicy;
use hyperpanes_core::session::control_mode::ControlMode;

/// Whether `argv` is `hyperpanes control-mode …`.
///
/// `-CC` is accepted as an alias because that is what a user copying a tmux invocation into
/// a mobile client's "custom command" box will type.
pub fn wants_control_mode(argv: &[String]) -> bool {
    argv.get(1)
        .map(|a| a == "control-mode" || a == "-CC")
        .unwrap_or(false)
}

/// Parsed `control-mode` flags.
#[derive(Debug, PartialEq, Eq)]
pub struct ControlOpts {
    /// The name this workspace is published under (`%session-changed $0 <name>`).
    pub session_name: String,
    /// Whether the client may reflow panes with `refresh-client -C`.
    pub policy: ResizePolicy,
    /// `--no-dcs`: bare `-C` framing, without the `ESC P 1000 p` … `ESC \` envelope.
    pub mode: ControlMode,
}

impl Default for ControlOpts {
    fn default() -> Self {
        Self {
            session_name: "hyperpanes".to_string(),
            policy: ResizePolicy::default(),
            mode: ControlMode::default(),
        }
    }
}

impl ControlOpts {
    /// `hyperpanes control-mode [--session-name <n>] [--resize] [--no-dcs]`.
    ///
    /// Unknown flags are rejected rather than ignored: this process's stdout is a protocol
    /// stream, so a typo that silently changed behaviour would surface as an unexplainable
    /// client bug much later.
    pub fn parse(argv: &[String]) -> Result<Self, String> {
        let mut out = Self::default();
        let mut it = argv.iter().skip(2); // argv[0]=exe, argv[1]="control-mode"
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--resize" => out.policy = ResizePolicy::Request,
                "--no-dcs" | "-C" => out.mode = ControlMode::Plain,
                "--session-name" => {
                    let n = it.next().ok_or("--session-name needs a value")?;
                    if n.is_empty() || n.contains(char::is_whitespace) {
                        return Err(
                            "--session-name must be one non-empty word (it is a protocol \
                             field, and a space in it would desync %session-changed)"
                                .to_string(),
                        );
                    }
                    out.session_name = n.clone();
                }
                "--help" | "-h" => return Err(HELP.to_string()),
                other => return Err(format!("unknown flag '{other}'\n\n{HELP}")),
            }
        }
        Ok(out)
    }
}

pub const HELP: &str = "\
hyperpanes control-mode — serve this workspace's panes over the tmux control protocol

USAGE:
    hyperpanes control-mode [--session-name <name>] [--resize] [--no-dcs]

    Speaks the SERVER side of tmux's control mode (`tmux -CC`) on stdin/stdout, so
    iTerm2 and the mobile tmux clients render live hyperpanes panes as native tabs.
    Each hyperpanes pane is published as one tmux window holding one pane.

    --session-name <n>  Name to publish the workspace under (default: hyperpanes).
    --resize            Let the client reflow panes with `refresh-client -C`. This
                        changes the pane for every viewer, the desktop included.
                        Without it (the default) panes keep the desktop's grid.
    --no-dcs, -C        Bare `-C` framing: omit the DCS envelope around the stream.

Point a client at it over SSH, e.g.:
    ssh <host> hyperpanes control-mode
";

/// The salt every hyperpanes client keys its daemon by: this install's user-data dir.
/// Identical to `attach`'s, so both find the same daemon.
#[cfg_attr(not(unix), allow(dead_code))]
fn salt() -> String {
    hyperpanes_core::persistence::paths::user_data_dir()
        .to_string_lossy()
        .into_owned()
}

/// How often the loop re-polls `ListSessions` to notice panes being **resized** on the
/// desktop — the one change the daemon still pushes no event for. Appearing and vanishing
/// panes arrive on `SessionsChanged` and do not wait for this; the poll remains the backstop
/// for them too, since a client that learns about a new window a second late is fine and one
/// that never learns is not.
#[cfg_attr(not(unix), allow(dead_code))]
const POLL: std::time::Duration = std::time::Duration::from_millis(1200);

/// How long `capture-pane` waits for the daemon's screen mirror before answering with
/// whatever it already had. Short on purpose: the client is blocked on a guard block.
#[cfg_attr(not(unix), allow(dead_code))]
const SCREEN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// `hyperpanes control-mode` on Windows. Same reason as `attach`: the GUI binary is built
/// with `windows_subsystem = "windows"` and has no console to be a protocol stream. The
/// protocol core itself is platform-independent and compiles here — only this transport is
/// unix-only.
#[cfg(not(unix))]
pub fn run(_argv: &[String]) -> Result<(), String> {
    Err(
        "hyperpanes control-mode is not available on Windows yet (the GUI binary has no \
         console subsystem). Run it from a unix host."
            .to_string(),
    )
}

/// One thing the loop can be woken by.
#[cfg(unix)]
enum Msg {
    /// A frame from the session daemon.
    Daemon(Box<hyperpanes_core::session::proto::DaemonMsg>),
    /// The daemon connection ended — the workspace is gone, so this client is done.
    DaemonEof,
    /// One command line from the control client.
    Line(String),
    /// The client closed its side.
    StdinEof,
}

#[cfg(unix)]
pub fn run(argv: &[String]) -> Result<(), String> {
    use hyperpanes_core::session::attach;
    use hyperpanes_core::session::control_mode::{
        needs_newline, wants_screen_refresh, Action, ControlServer, PaneInfo,
    };
    use hyperpanes_core::session::proto::{
        read_frame, write_frame, ClientMsg, DaemonMsg, SessionMeta, PROTO_VER,
    };
    use hyperpanes_core::session_manager::SessionEvent;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::mpsc;

    let opts = ControlOpts::parse(argv)?;
    let salt = salt();

    let conn = attach::connect(&salt).map_err(|e| e.to_string())?;
    match attach::handshake(&conn) {
        // Same judgement as `attach`: warn, never tear down. A control client does not own
        // the daemon's lifetime, and its stderr is not in the protocol stream.
        Ok(ver) if ver != PROTO_VER => {
            eprintln!(
                "hyperpanes control-mode: warning — the running daemon speaks protocol \
                 {ver}, this binary speaks {PROTO_VER}. Continuing."
            );
        }
        Ok(_) => {}
        Err(e) => return Err(format!("handshake failed: {e}")),
    }

    let sessions = attach::list_sessions(&conn).map_err(|e| e.to_string())?;
    if sessions.is_empty() {
        // Refused rather than served empty: tmux clients treat a session with no windows as
        // a dead server and show an opaque failure, so a plain message on stderr is kinder.
        return Err(
            "no live hyperpanes sessions on this install — start hyperpanes first.".to_string(),
        );
    }

    let meta_to_pane = |m: &SessionMeta| {
        let mut p = PaneInfo::new(m.uid.clone());
        p.cols = m.cols;
        p.rows = m.rows;
        p.cwd = m.cwd.clone();
        p
    };
    let mut server = ControlServer::new(
        opts.session_name.clone(),
        sessions.iter().map(&meta_to_pane).collect(),
    )
    .with_policy(opts.policy)
    .with_mode(opts.mode);
    // What the last poll saw, so the next one can diff rather than re-announce everything.
    let mut seen: BTreeMap<String, (Option<u16>, Option<u16>)> = sessions
        .iter()
        .map(|m| (m.uid.clone(), (m.cols, m.rows)))
        .collect();
    // Per-uid splice point: the session's output cursor at the instant its replay buffer was
    // snapshotted (`DaemonMsg::Replay::cursor`). The daemon starts streaming a uid the moment
    // we attach, so live `Data` for bytes ALREADY inside the seed can reach us right after
    // the seed does; forwarding both would paint them twice in the client's pane. Same filter
    // `attach` runs, keyed by uid because control mode carries every pane at once.
    //
    // `0` means "not reported" (a daemon predating the field) — no real chunk ends at cursor
    // 0, so the absent entry and the sentinel both leave the filter off rather than eating
    // live output.
    let mut seed_cursors: BTreeMap<String, u64> = BTreeMap::new();

    let (tx, rx) = mpsc::channel::<Msg>();

    // Reader thread. Started BEFORE the first `Attach` so no reply can be missed; from here
    // on nothing else reads the socket (which is why the blocking `read_frame` is safe —
    // `attach::handshake`/`list_sessions` cleared their socket timeouts on the way out).
    {
        let mut r = hyperpanes_core::session::transport::try_clone(&conn)
            .map_err(|e| format!("clone connection: {e}"))?;
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("hp-ccmode-daemon".into())
            .spawn(move || {
                while let Ok(Some(msg)) = read_frame::<_, DaemonMsg>(&mut r) {
                    if tx.send(Msg::Daemon(Box::new(msg))).is_err() {
                        return;
                    }
                }
                let _ = tx.send(Msg::DaemonEof);
            })
            .map_err(|e| format!("daemon reader thread: {e}"))?;
    }

    // Stdin thread. Read as BYTES and lossily decode: a control client should only ever send
    // ASCII command lines, but a mis-wired one sending binary must not kill this process
    // with a UTF-8 error mid-session — the dispatcher will answer it with a `%error`.
    {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("hp-ccmode-stdin".into())
            .spawn(move || {
                use std::io::BufRead;
                let stdin = std::io::stdin();
                let mut r = stdin.lock();
                loop {
                    let mut buf = Vec::new();
                    match r.read_until(b'\n', &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    while matches!(buf.last(), Some(b'\n' | b'\r')) {
                        buf.pop();
                    }
                    let line = String::from_utf8_lossy(&buf).into_owned();
                    if tx.send(Msg::Line(line)).is_err() {
                        return;
                    }
                }
                let _ = tx.send(Msg::StdinEof);
            })
            .map_err(|e| format!("stdin thread: {e}"))?;
    }

    // Every socket write happens on THIS thread (the poll is a `recv_timeout`, not a second
    // writer thread), so the frames can never interleave.
    let mut sock = hyperpanes_core::session::transport::try_clone(&conn)
        .map_err(|e| format!("clone connection: {e}"))?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // The greeting first, then attach to every pane. One connection carries them all: the
    // daemon keys its event fan-out on a per-connection `attached` set (M0).
    let emit = |out: &mut dyn Write, lines: Vec<Vec<u8>>| -> std::io::Result<()> {
        for line in lines {
            out.write_all(&line)?;
            if needs_newline(&line) {
                out.write_all(b"\n")?;
            }
        }
        out.flush()
    };
    emit(&mut out, server.greeting()).map_err(|e| e.to_string())?;
    for uid in server.uids() {
        write_frame(&mut sock, &ClientMsg::Attach { uid }).map_err(|e| e.to_string())?;
    }

    // Screens requested by an in-flight `capture-pane`, and messages that arrived while we
    // waited for them (replayed by the main loop rather than dropped).
    let mut pending: std::collections::VecDeque<Msg> = std::collections::VecDeque::new();

    loop {
        let msg = match pending.pop_front() {
            Some(m) => m,
            None => match rx.recv_timeout(POLL) {
                Ok(m) => m,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Poll for structure changes. The reply comes back through the reader
                    // thread as `DaemonMsg::Sessions` and is diffed below.
                    if write_frame(&mut sock, &ClientMsg::ListSessions).is_err() {
                        break;
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };

        match msg {
            Msg::DaemonEof => {
                emit(
                    &mut out,
                    server.goodbye(Some("the hyperpanes daemon exited")),
                )
                .map_err(|e| e.to_string())?;
                return Ok(());
            }
            Msg::StdinEof => {
                emit(&mut out, server.goodbye(None)).map_err(|e| e.to_string())?;
                return Ok(());
            }
            Msg::Line(line) => {
                // `capture-pane` is the one command that needs state only the daemon has, and
                // the dispatcher is synchronous — so top up the screen mirrors first.
                if wants_screen_refresh(&line) {
                    refresh_screens(&mut server, &mut sock, &rx, &mut pending);
                }
                let reaction = server.command(&line);
                emit(&mut out, reaction.lines).map_err(|e| e.to_string())?;
                for action in reaction.actions {
                    match action {
                        Action::Write { uid, data } => {
                            // The daemon's `Write` carries a `String`. Keys from `send-keys`
                            // are ASCII or UTF-8 in every real client; `-H` can express a
                            // lone invalid byte, which is lossily replaced rather than
                            // silently truncating the write.
                            let data = String::from_utf8_lossy(&data).into_owned();
                            let _ = write_frame(&mut sock, &ClientMsg::Write { uid, data });
                        }
                        Action::Resize { uid, cols, rows } => {
                            let _ = write_frame(&mut sock, &ClientMsg::Resize { uid, cols, rows });
                        }
                        Action::Detach => {
                            emit(&mut out, server.goodbye(None)).map_err(|e| e.to_string())?;
                            return Ok(());
                        }
                    }
                }
            }
            Msg::Daemon(msg) => match *msg {
                // The replay buffer seeds the client's brand-new tab, exactly as `attach`
                // seeds a fresh grid: the same bytes, escaped as one `%output`.
                DaemonMsg::Replay { uid, data, cursor } => {
                    // A re-attach can seed the same uid twice; keep the furthest point, so a
                    // stale second seed never re-opens the window on bytes already painted.
                    let seed = seed_cursors.entry(uid.clone()).or_insert(0);
                    *seed = (*seed).max(cursor);
                    emit(&mut out, server.output(&uid, data.as_bytes()))
                        .map_err(|e| e.to_string())?;
                }
                DaemonMsg::Screen { uid, text } => server.set_screen(&uid, text),
                // The poll's reply, and (M7) the daemon's unsolicited push when a session is
                // created, killed or adopted. Same snapshot shape, so the same differ: the
                // push just gets there first.
                DaemonMsg::Sessions(list) | DaemonMsg::SessionsChanged(list) => {
                    let lines = diff_sessions(&mut server, &mut seen, &list, &mut sock);
                    emit(&mut out, lines).map_err(|e| e.to_string())?;
                }
                DaemonMsg::Event(ev) => {
                    let lines = match ev {
                        SessionEvent::Data { uid, data, cursor } => {
                            // `cursor` is the value AFTER the chunk, bumped under the same
                            // lock the seed was snapshotted under, so a chunk is wholly
                            // inside the seed or wholly outside it — never straddling.
                            if cursor != 0 && cursor <= *seed_cursors.get(&uid).unwrap_or(&0) {
                                Vec::new() // already in the seed we just emitted
                            } else {
                                server.output(&uid, data.as_bytes())
                            }
                        }
                        SessionEvent::Exit { uid, .. } => server.pane_exited(&uid),
                        SessionEvent::Cwd { uid, cwd } => {
                            server.set_cwd(&uid, Some(cwd));
                            Vec::new()
                        }
                        // Prompt/command/agent events are hyperpanes-specific and have no
                        // tmux notification; a client learns nothing from them.
                        _ => Vec::new(),
                    };
                    emit(&mut out, lines).map_err(|e| e.to_string())?;
                }
                // Claims are the GUI's adoption bookkeeping (M7). A control-mode client never
                // adopts an orphan — it observes whatever the desktop is hosting — so it has
                // nothing to do with the claim table.
                DaemonMsg::Hello { .. }
                | DaemonMsg::Created { .. }
                | DaemonMsg::Pong
                | DaemonMsg::ClaimResult { .. }
                | DaemonMsg::Claims(_) => {}
            },
        }
    }

    emit(&mut out, server.goodbye(None)).map_err(|e| e.to_string())?;
    Ok(())
}

/// Ask the daemon for a fresh screen mirror of every pane and wait (briefly) for the
/// replies, so a synchronous `capture-pane` has something current to answer with.
///
/// Anything else that arrives meanwhile is pushed onto `pending` for the main loop rather
/// than dropped — losing a `%output` here would leave a permanent hole in the pane.
#[cfg(unix)]
fn refresh_screens(
    server: &mut hyperpanes_core::session::control_mode::ControlServer,
    sock: &mut hyperpanes_core::session::transport::Conn,
    rx: &std::sync::mpsc::Receiver<Msg>,
    pending: &mut std::collections::VecDeque<Msg>,
) {
    use hyperpanes_core::session::proto::{write_frame, ClientMsg, DaemonMsg};

    let uids = server.uids();
    let mut want = 0usize;
    for uid in &uids {
        if write_frame(sock, &ClientMsg::RenderScreen { uid: uid.clone() }).is_ok() {
            want += 1;
        }
    }
    let deadline = std::time::Instant::now() + SCREEN_TIMEOUT;
    while want > 0 {
        let Some(budget) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return;
        };
        match rx.recv_timeout(budget) {
            Ok(Msg::Daemon(msg)) => match *msg {
                DaemonMsg::Screen { uid, text } => {
                    server.set_screen(&uid, text);
                    want -= 1;
                }
                other => pending.push_back(Msg::Daemon(Box::new(other))),
            },
            Ok(other) => pending.push_back(other),
            Err(_) => return,
        }
    }
}

/// Turn a `ListSessions` reply into the notifications a client needs: `%window-add` for a
/// pane that appeared, `%window-close` for one that went away, `%layout-change` for one
/// whose grid moved. Attaches to newly-discovered panes as a side effect, so their output
/// starts flowing on this same connection.
#[cfg(unix)]
fn diff_sessions(
    server: &mut hyperpanes_core::session::control_mode::ControlServer,
    seen: &mut std::collections::BTreeMap<String, (Option<u16>, Option<u16>)>,
    list: &[hyperpanes_core::session::proto::SessionMeta],
    sock: &mut hyperpanes_core::session::transport::Conn,
) -> Vec<Vec<u8>> {
    use hyperpanes_core::session::control_mode::PaneInfo;
    use hyperpanes_core::session::proto::{write_frame, ClientMsg};

    let mut lines = Vec::new();
    let mut now: std::collections::BTreeMap<String, (Option<u16>, Option<u16>)> =
        std::collections::BTreeMap::new();

    for m in list {
        now.insert(m.uid.clone(), (m.cols, m.rows));
        if !server.has_pane(&m.uid) {
            let mut p = PaneInfo::new(m.uid.clone());
            p.cols = m.cols;
            p.rows = m.rows;
            p.cwd = m.cwd.clone();
            lines.extend(server.pane_added(p));
            let _ = write_frame(sock, &ClientMsg::Attach { uid: m.uid.clone() });
            continue;
        }
        if let (Some(c), Some(r)) = (m.cols, m.rows) {
            if seen.get(&m.uid) != Some(&(m.cols, m.rows)) {
                lines.extend(server.pane_resized(&m.uid, c, r));
            }
        }
        if m.cwd.is_some() {
            server.set_cwd(&m.uid, m.cwd.clone());
        }
    }
    let gone: Vec<String> = seen
        .keys()
        .filter(|u| !now.contains_key(*u))
        .cloned()
        .collect();
    for uid in gone {
        lines.extend(server.pane_exited(&uid));
    }
    *seen = now;
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(rest: &[&str]) -> Vec<String> {
        let mut v = vec!["hyperpanes".to_string(), "control-mode".to_string()];
        v.extend(rest.iter().map(|s| s.to_string()));
        v
    }

    #[test]
    fn recognizes_the_subcommand_and_the_tmux_alias() {
        assert!(wants_control_mode(&argv(&[])));
        assert!(wants_control_mode(&[
            "hyperpanes".to_string(),
            "-CC".to_string()
        ]));
        assert!(!wants_control_mode(&[
            "hyperpanes".to_string(),
            "attach".to_string()
        ]));
        assert!(!wants_control_mode(&["hyperpanes".to_string()]));
    }

    #[test]
    fn defaults_are_observe_and_wrapped() {
        let o = ControlOpts::parse(&argv(&[])).unwrap();
        assert_eq!(o.session_name, "hyperpanes");
        assert_eq!(o.policy, ResizePolicy::Observe);
        assert_eq!(o.mode, ControlMode::Wrapped);
    }

    #[test]
    fn flags_parse() {
        let o =
            ControlOpts::parse(&argv(&["--resize", "--no-dcs", "--session-name", "work"])).unwrap();
        assert_eq!(o.policy, ResizePolicy::Request);
        assert_eq!(o.mode, ControlMode::Plain);
        assert_eq!(o.session_name, "work");
    }

    #[test]
    fn a_session_name_with_a_space_is_rejected() {
        // It is a protocol field: `%session-changed $0 <name>` is whitespace-delimited.
        let err = ControlOpts::parse(&argv(&["--session-name", "my work"])).unwrap_err();
        assert!(err.contains("one non-empty word"), "{err}");
        assert!(ControlOpts::parse(&argv(&["--session-name", ""])).is_err());
        assert!(ControlOpts::parse(&argv(&["--session-name"])).is_err());
    }

    #[test]
    fn unknown_flags_and_positionals_are_rejected_not_ignored() {
        assert!(ControlOpts::parse(&argv(&["--reisze"])).is_err());
        assert!(ControlOpts::parse(&argv(&["pane-abc"])).is_err());
        assert!(ControlOpts::parse(&argv(&["--help"]))
            .unwrap_err()
            .contains("USAGE"));
    }
}
