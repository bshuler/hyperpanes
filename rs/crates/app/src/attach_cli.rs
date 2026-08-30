//! `hyperpanes attach [<pane>]` — the **M2 terminal client** (`docs/mux-backend-plan.md`).
//!
//! Renders a live hyperpanes pane into whatever terminal this process is running in: seed
//! from the daemon's replay buffer, stream output to stdout, forward stdin, handle
//! `SIGWINCH`, and leave on a detach key with the session still running. This is the
//! tmux-client equivalent, and it works over a stock system sshd today — M3's embedded
//! russh server will run this same code on an SSH channel instead of a shell.
//!
//! ## Split with `core`
//! Everything protocol-shaped — connect, `Attach`, the replay seed, the event pump, uid
//! resolution, the detach-key state machine, the resize policy — lives in
//! [`hyperpanes_core::session::attach`], with no tty in it, so M3 can reuse it verbatim.
//! What is left here is the part that is genuinely about *this* process's terminal:
//! `termios` raw mode, `TIOCGWINSZ`, signal handling, the chooser, and argv. That mirrors
//! how [`control_cli`](crate::control_cli) keeps its shared plumbing separate from the
//! `pair`/`devices` front ends.
//!
//! ## Resize policy — attach at the desktop's grid and letterbox (DEFAULT)
//! The plan's open question is decided in [`ResizePolicy`]: by default this client **never**
//! resizes the pane. See that type's docs; the short version is that a small client silently
//! reflowing a desktop pane is a destructive, invisible side effect, and letterboxing is not.
//! `--resize` is the explicit opt-in for the other policy.
//!
//! Letterboxing is a *viewing* compromise, not a rendering one, and this client forwards
//! bytes rather than running its own VTE: the pane emits for its own grid, and a local
//! terminal of a different width puts autowrap in a different place. Narrower is the case
//! that actually looks wrong, so that one is warned about up front; wider is harmless in
//! practice (the pane simply does not use the extra columns) and neither is fixable without
//! a local screen model, which is what the GUI is for.
//!
//! ## Restoring the terminal
//! Raw mode must never survive this process, however it dies. Four covers:
//! 1. normal return → [`RawGuard`]'s `Drop`;
//! 2. an unwinding panic → the same `Drop`;
//! 3. a panic hook installed ahead of the default one, so the message prints to a sane tty;
//! 4. `SIGINT`/`SIGTERM`/`SIGHUP`/`SIGQUIT` → a handler that calls `tcsetattr` (POSIX
//!    async-signal-safe) and `_exit`s, because those never run destructors.

// Everything below the tty layer is `core`'s: see the module docs for the split. The
// unix-only half of the surface is imported inside `run`, so the Windows build (whose
// `run` is a one-line "not yet") stays warning-clean under clippy's `-D warnings`.
use hyperpanes_core::session::attach::{parse_detach_key, ResizePolicy, DEFAULT_DETACH_PREFIX};
use hyperpanes_core::session::proto::SessionMeta;

/// Whether `argv` is `hyperpanes attach …`. Checked in `main` alongside the other
/// subcommands, before the GUI/single-instance path.
pub fn wants_attach(argv: &[String]) -> bool {
    argv.get(1).map(|a| a == "attach").unwrap_or(false)
}

/// Parsed `attach` flags.
#[derive(Debug, PartialEq, Eq)]
pub struct AttachOpts {
    /// The pane the user named — a uid, a unique prefix, or a unique substring. `None` asks
    /// for the chooser.
    pub query: Option<String>,
    /// `--list` / `-l`: print the live sessions and exit without attaching.
    pub list: bool,
    /// Whether this client may reflow the pane. See the module docs.
    pub policy: ResizePolicy,
    /// The detach prefix byte.
    pub detach: u8,
}

impl Default for AttachOpts {
    fn default() -> Self {
        Self {
            query: None,
            list: false,
            policy: ResizePolicy::default(),
            detach: DEFAULT_DETACH_PREFIX,
        }
    }
}

impl AttachOpts {
    /// `hyperpanes attach [<pane>] [--list] [--resize] [--detach-key <spec>]`.
    ///
    /// A single positional argument names the pane; a second is an error rather than a
    /// silent win for one of them. Unknown flags are rejected outright — a typo'd
    /// `--resize` that quietly did nothing would be a very confusing bug given what the
    /// flag does.
    pub fn parse(argv: &[String]) -> Result<Self, String> {
        let mut out = Self::default();
        let mut it = argv.iter().skip(2); // argv[0]=exe, argv[1]="attach"
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--list" | "-l" => out.list = true,
                "--resize" => out.policy = ResizePolicy::Request,
                "--detach-key" => {
                    let spec = it.next().ok_or("--detach-key needs a value")?;
                    out.detach = parse_detach_key(spec)?;
                }
                "--help" | "-h" => return Err(HELP.to_string()),
                other if other.starts_with('-') => {
                    return Err(format!("unknown flag '{other}'\n\n{HELP}"))
                }
                positional => {
                    if out.query.is_some() {
                        return Err(format!(
                            "attach takes at most one pane ({}, {positional})",
                            out.query.as_deref().unwrap_or_default()
                        ));
                    }
                    out.query = Some(positional.to_string());
                }
            }
        }
        Ok(out)
    }
}

pub const HELP: &str = "\
hyperpanes attach — render a live pane into this terminal

USAGE:
    hyperpanes attach [<pane>] [--resize] [--detach-key <key>]
    hyperpanes attach --list

    <pane>            A pane uid, or any unique prefix/substring of one. Omit it to
                      pick from a list of the live sessions.
    -l, --list        List the live sessions and exit.
    --resize          Resize the pane to THIS terminal, now and on every window change.
                      This reflows the pane for every viewer, the desktop included.
                      Without it (the default) the pane keeps the desktop's grid and is
                      letterboxed into this terminal.
    --detach-key <k>  The detach prefix, e.g. C-\\ (default), C-], ctrl-o.

Press the detach prefix then `d` to detach, leaving the session running. Press it twice
to send the literal control byte through to the pane.
";

#[cfg_attr(not(unix), allow(dead_code))]
/// Human-readable \"how long ago\" for the chooser, from an epoch-ms stamp and \"now\".
/// Split out (and pure) so the formatting is testable without a clock.
fn ago(last_ms: Option<u64>, now_ms: u64) -> String {
    let Some(last) = last_ms else {
        return "never".to_string();
    };
    let secs = now_ms.saturating_sub(last) / 1000;
    match secs {
        0..=1 => "now".to_string(),
        2..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg_attr(not(unix), allow(dead_code))]
/// One chooser/`--list` row: `1) pane-1a2b3c4d  120x40  ~/code/hyperpanes   2m ago`.
fn row(index: usize, s: &SessionMeta, now: u64) -> String {
    let grid = match (s.cols, s.rows) {
        (Some(c), Some(r)) => format!("{c}x{r}"),
        _ => "?x?".to_string(),
    };
    let cwd = s.cwd.as_deref().unwrap_or("-");
    format!(
        "  {index}) {uid}  {grid:>7}  {cwd}  ({when})",
        uid = s.uid,
        when = ago(s.last_output_at, now)
    )
}

#[cfg_attr(not(unix), allow(dead_code))]
fn print_sessions(sessions: &[SessionMeta]) {
    let now = now_ms();
    for (i, s) in sessions.iter().enumerate() {
        println!("{}", row(i + 1, s, now));
    }
}

// ---------------------------------------------------------------------------
// unix implementation
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod tty {
    //! `termios` / `TIOCGWINSZ` / signal glue — the only part of the attach client that is
    //! genuinely about *this* process's controlling terminal.

    use std::io::{self, Read, Write};
    use std::os::unix::io::RawFd;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, Ordering};

    pub const STDIN: RawFd = 0;
    pub const STDOUT: RawFd = 1;

    /// Set by the `SIGWINCH` handler; consumed by the resize watcher thread. An
    /// `AtomicBool` is the only thing a signal handler may safely touch.
    pub static WINCH: AtomicBool = AtomicBool::new(false);

    /// The termios to restore, published before raw mode is entered so the signal handler
    /// (which cannot lock anything) can reach it. Leaked deliberately: it must outlive every
    /// path that could still need to restore, including `_exit` from a handler.
    static SAVED: AtomicPtr<libc::termios> = AtomicPtr::new(std::ptr::null_mut());
    static SAVED_FD: AtomicI32 = AtomicI32::new(-1);

    pub fn is_tty(fd: RawFd) -> bool {
        // SAFETY: `isatty` only inspects the descriptor.
        unsafe { libc::isatty(fd) == 1 }
    }

    /// This terminal's `(cols, rows)`, or `None` if the descriptor is not a terminal.
    pub fn size(fd: RawFd) -> Option<(u16, u16)> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: `TIOCGWINSZ` writes exactly one `winsize` through the pointer we pass.
        let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) };
        if rc != 0 || ws.ws_col == 0 || ws.ws_row == 0 {
            return None;
        }
        Some((ws.ws_col, ws.ws_row))
    }

    /// Restore the termios published by the first [`RawGuard`]. Async-signal-safe
    /// (`tcsetattr` is on POSIX's list), which is the whole reason it reads from statics:
    /// it is called from the fatal-signal handler and from the panic hook, neither of which
    /// may allocate, lock, or reach the guard on somebody's stack. Ordinary scope exit does
    /// **not** come through here — [`RawGuard`]'s `Drop` restores from its own copy.
    pub fn restore() {
        let saved = SAVED.load(Ordering::SeqCst);
        let fd = SAVED_FD.load(Ordering::SeqCst);
        if saved.is_null() || fd < 0 {
            return;
        }
        // SAFETY: `saved` is a leaked, never-freed `termios` published once by `RawGuard`.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, saved) };
    }

    /// Puts the terminal in raw mode for its lifetime and restores it on drop — including
    /// while a panic unwinds.
    ///
    /// The guard carries its **own** copy of the original `termios` and its own fd, and
    /// restores from those rather than from the globals. The globals exist only so the
    /// fatal-signal handler — which cannot allocate, lock, or reach a stack local — has
    /// something to restore from; they are published once and never cleared, so a `Drop`
    /// that trusted them would restore the wrong descriptor the moment there were two
    /// guards (as there are in the tests, and as M3 will have with one client per channel).
    pub struct RawGuard {
        fd: RawFd,
        original: libc::termios,
    }

    impl RawGuard {
        pub fn enter(fd: RawFd) -> io::Result<Self> {
            let mut term: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: `tcgetattr` fills the struct we own.
            if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // Publish the ORIGINAL settings before touching anything, so a signal that
            // arrives mid-`tcsetattr` can still restore.
            if SAVED.load(Ordering::SeqCst).is_null() {
                SAVED.store(Box::into_raw(Box::new(term)), Ordering::SeqCst);
                SAVED_FD.store(fd, Ordering::SeqCst);
            }
            let mut raw = term;
            // SAFETY: plain field manipulation on a struct we own.
            unsafe { libc::cfmakeraw(&mut raw) };
            // SAFETY: `raw` is a fully initialised termios for `fd`.
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { fd, original: term })
        }
    }

    impl Drop for RawGuard {
        fn drop(&mut self) {
            // SAFETY: `original` is this guard's own copy of the settings read from `fd`.
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
        }
    }

    extern "C" fn on_winch(_sig: libc::c_int) {
        WINCH.store(true, Ordering::SeqCst);
    }

    /// A signal that would otherwise kill us with the terminal still raw. Restore, then
    /// re-raise the conventional exit status. Only async-signal-safe calls here.
    extern "C" fn on_fatal(sig: libc::c_int) {
        restore();
        // SAFETY: `_exit` is async-signal-safe and never returns.
        unsafe { libc::_exit(128 + sig) };
    }

    /// Install the `SIGWINCH` watcher and the restore-then-die handlers. Also chains a panic
    /// hook that restores before the default hook prints, so a panic message lands on a
    /// cooked terminal instead of a staircase of raw-mode lines.
    pub fn install_handlers() {
        // SAFETY: both handlers are `extern "C"` and touch only async-signal-safe calls.
        unsafe {
            libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t);
            for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
                libc::signal(sig, on_fatal as *const () as libc::sighandler_t);
            }
        }
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    }

    /// A `Read` straight off a descriptor — no `std::io::Stdin` lock and no `BufReader`
    /// between a keystroke and the wire.
    pub struct FdReader(pub RawFd);

    impl Read for FdReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            loop {
                // SAFETY: `read` writes at most `buf.len()` bytes into `buf`.
                let n =
                    unsafe { libc::read(self.0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n >= 0 {
                    return Ok(n as usize);
                }
                let e = io::Error::last_os_error();
                // A `SIGWINCH` during a blocked read must not look like the user hung up.
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
        }
    }

    /// A `Write` straight to a descriptor — the pane's bytes reach the terminal unbuffered
    /// and unmangled (no `LineWriter` splitting escape sequences on newlines).
    pub struct FdWriter(pub RawFd);

    impl Write for FdWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            loop {
                // SAFETY: `write` reads at most `buf.len()` bytes from `buf`.
                let n =
                    unsafe { libc::write(self.0, buf.as_ptr() as *const libc::c_void, buf.len()) };
                if n >= 0 {
                    return Ok(n as usize);
                }
                let e = io::Error::last_os_error();
                // Retry rather than returning `Ok(0)`: `write_all` reads a zero-length write
                // as `WriteZero` and would abort the pump on a harmless `SIGWINCH`.
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(()) // unbuffered: `write` already reached the descriptor
        }
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
/// The salt every hyperpanes client keys its daemon by: this install's user-data dir.
/// Identical to `main`'s `--kill-daemon` path and to `SessionManager::new_daemon`, so
/// `attach` always finds the daemon THIS build would attach to.
fn salt() -> String {
    hyperpanes_core::persistence::paths::user_data_dir()
        .to_string_lossy()
        .into_owned()
}

/// `hyperpanes attach` on Windows. The GUI binary is built with
/// `windows_subsystem = "windows"` (no console at all), and `core`'s Windows dependency set
/// carries no `Win32_System_Console` bindings for raw mode — so rather than ship something
/// that silently does nothing, say so. The plan scopes M2 to "usable over a stock system
/// sshd immediately", which is the unix leg; a Windows console client is follow-up work.
#[cfg(not(unix))]
pub fn run(_argv: &[String]) -> Result<(), String> {
    Err("hyperpanes attach is not available on Windows yet \
         (the GUI binary has no console subsystem, and the console raw-mode bindings are \
         not in this build). Use the desktop app, or attach from a unix host."
        .to_string())
}

#[cfg(unix)]
pub fn run(argv: &[String]) -> Result<(), String> {
    use hyperpanes_core::session::attach::{self, detach_key_label, Attachment, PumpEnd};
    use std::io::Write;
    use std::sync::atomic::Ordering;

    let opts = AttachOpts::parse(argv)?;
    let salt = salt();

    let conn = attach::connect(&salt).map_err(|e| e.to_string())?;
    match attach::handshake(&conn) {
        Ok(ver) if ver != hyperpanes_core::session::proto::PROTO_VER => {
            // Deliberately NOT the lock-step tear-down `daemon_client` does: that daemon is
            // full of somebody's live shells and this CLI does not own its lifetime.
            eprintln!(
                "hyperpanes attach: warning — the running daemon speaks protocol {ver}, this \
                 binary speaks {}. Attaching anyway.",
                hyperpanes_core::session::proto::PROTO_VER
            );
        }
        Ok(_) => {}
        Err(e) => return Err(format!("handshake failed: {e}")),
    }

    let sessions = attach::list_sessions(&conn).map_err(|e| e.to_string())?;
    if sessions.is_empty() {
        return Err("no live hyperpanes sessions on this install.".to_string());
    }
    if opts.list {
        println!("Live hyperpanes sessions:");
        print_sessions(&sessions);
        return Ok(());
    }

    let uid = choose(&sessions, opts.query.as_deref())?;
    let meta = sessions
        .iter()
        .find(|s| s.uid == uid)
        .expect("chosen uid came from this list");

    // ---- banner + the letterbox verdict, printed while the terminal is still cooked ----
    let term_size = tty::size(tty::STDOUT).unwrap_or((80, 24));
    println!(
        "[hyperpanes] attaching to {uid} — press {} d to detach",
        detach_key_label(opts.detach)
    );
    match opts.policy {
        ResizePolicy::Request => {
            println!(
                "[hyperpanes] --resize: reflowing the pane to {}x{} (this changes it on the \
                 desktop too)",
                term_size.0, term_size.1
            );
        }
        ResizePolicy::Observe => {
            if attach::fits(term_size, (meta.cols, meta.rows)) == Some(false) {
                let (c, r) = (meta.cols.unwrap_or(0), meta.rows.unwrap_or(0));
                println!(
                    "[hyperpanes] this terminal is {}x{} but the pane is {c}x{r} — output will \
                     be clipped. Resize this window, or re-run with --resize to reflow the \
                     pane (which also reflows it on the desktop).",
                    term_size.0, term_size.1
                );
            }
        }
    }

    let (mut attachment, seed) = Attachment::open(conn, &uid).map_err(|e| e.to_string())?;
    attachment.set_repaint_prefix(CLEAR_SCREEN);
    let writer = attachment.writer();

    if opts.policy == ResizePolicy::Request {
        writer
            .request_resize(term_size.0, term_size.1)
            .map_err(|e| e.to_string())?;
    }

    // ---- raw mode from here down; every exit path below restores it ----
    tty::install_handlers();
    let interactive = tty::is_tty(tty::STDIN);
    let _raw = if interactive {
        Some(tty::RawGuard::enter(tty::STDIN).map_err(|e| format!("raw mode: {e}"))?)
    } else {
        None
    };

    let mut out = tty::FdWriter(tty::STDOUT);
    out.write_all(CLEAR_SCREEN).map_err(|e| e.to_string())?;
    out.write_all(seed.as_bytes()).map_err(|e| e.to_string())?;

    // Input: keystrokes → the pane, until the detach key. On detach it shuts the socket
    // down, which EOFs the output pump below and unwinds the whole client.
    {
        let writer = writer.clone();
        let detach = opts.detach;
        std::thread::Builder::new()
            .name("hp-attach-input".into())
            .spawn(move || {
                // Both endings — the detach key and a closed stdin — mean this client is
                // done. The SESSION is untouched: no Kill, no Shutdown, ever.
                //
                // The disconnect runs from a `Drop`, not from a statement after the pump,
                // because it is what unblocks `pump_output` on the main thread. A panic in
                // here that skipped it would leave the process parked in a blocking read
                // with the terminal still raw, and the `RawGuard` — owned by the main
                // thread — never reached.
                struct Disconnect(attach::AttachWriter);
                impl Drop for Disconnect {
                    fn drop(&mut self) {
                        self.0.disconnect();
                    }
                }
                let on_end = Disconnect(writer);
                let mut filter = attach::DetachFilter::new(detach);
                let _ = attach::pump_input(tty::FdReader(tty::STDIN), &on_end.0, &mut filter);
            })
            .map_err(|e| format!("input thread: {e}"))?;
    }

    // SIGWINCH: the handler can only set a flag, so a small watcher turns it into work.
    // Under `Observe` that work is a REPAINT, not a resize — the local window changed, the
    // pane did not.
    {
        let writer = writer.clone();
        let policy = opts.policy;
        std::thread::Builder::new()
            .name("hp-attach-winch".into())
            .spawn(move || {
                let mut last = term_size;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(60));
                    if !tty::WINCH.swap(false, Ordering::SeqCst) {
                        continue;
                    }
                    let Some(size) = tty::size(tty::STDOUT) else {
                        continue;
                    };
                    if size == last {
                        continue;
                    }
                    last = size;
                    let sent = match policy {
                        ResizePolicy::Request => writer.request_resize(size.0, size.1),
                        ResizePolicy::Observe => writer.request_repaint(),
                    };
                    if sent.is_err() {
                        return; // the connection is gone; the main pump is unwinding
                    }
                }
            })
            .map_err(|e| format!("winch thread: {e}"))?;
    }

    let end = attachment
        .pump_output(&mut out)
        .map_err(|e| e.to_string())?;

    // Cooked again before anything is printed, so these lines are not a raw-mode staircase.
    drop(_raw);
    match end {
        PumpEnd::Exited(code) => {
            println!("\r\n[hyperpanes] {uid} exited (code {code})");
        }
        PumpEnd::Disconnected => {
            println!("\r\n[hyperpanes] detached from {uid} — the session is still running");
        }
    }
    Ok(())
}

/// Home the cursor and clear the screen AND the scrollback, so a repaint of the pane's
/// replay buffer does not stack on top of the previous copy.
#[cfg_attr(not(unix), allow(dead_code))]
const CLEAR_SCREEN: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";

/// Turn the user's pane argument (or its absence) into one uid.
///
/// With a query: resolve it, and report ambiguity rather than guessing. Without one: a
/// single live session is unambiguous, so attach to it; otherwise show the chooser — but
/// only if stdin is a terminal, because a non-interactive caller (a script, an SSH
/// `ProxyCommand`) has nobody to answer the prompt.
#[cfg(unix)]
fn choose(sessions: &[SessionMeta], query: Option<&str>) -> Result<String, String> {
    use hyperpanes_core::session::attach::{self, UidMatch};
    use std::io::Write;

    if let Some(q) = query {
        return match attach::resolve_uid(sessions, q) {
            UidMatch::One(uid) => Ok(uid),
            UidMatch::None => Err(format!(
                "no live session matches '{q}'. `hyperpanes attach --list` shows them."
            )),
            UidMatch::Ambiguous(hits) => Err(format!(
                "'{q}' matches {} sessions:\n{}\nBe more specific.",
                hits.len(),
                hits.iter()
                    .map(|u| format!("  {u}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
        };
    }
    if sessions.len() == 1 {
        return Ok(sessions[0].uid.clone());
    }
    if !tty::is_tty(tty::STDIN) {
        let mut msg = String::from("several live sessions; name one:\n");
        let now = now_ms();
        for (i, s) in sessions.iter().enumerate() {
            msg.push_str(&row(i + 1, s, now));
            msg.push('\n');
        }
        return Err(msg);
    }

    println!("Live hyperpanes sessions:");
    print_sessions(sessions);
    print!("Attach to [1-{}, or q to quit]: ", sessions.len());
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let line = line.trim();
    if line.is_empty() || line.eq_ignore_ascii_case("q") {
        return Err("cancelled.".to_string());
    }
    match line.parse::<usize>() {
        Ok(n) if (1..=sessions.len()).contains(&n) => Ok(sessions[n - 1].uid.clone()),
        // A pasted uid is at least as likely as a number at this prompt.
        _ => choose(sessions, Some(line)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn meta(uid: &str, cols: Option<u16>, rows: Option<u16>) -> SessionMeta {
        SessionMeta {
            uid: uid.to_string(),
            cwd: Some("/tmp".into()),
            output_bytes: 0,
            last_output_at: None,
            alive: true,
            cols,
            rows,
            foreground: None,
        }
    }

    // ---- subcommand detection (mirrors the argv tests in main.rs) ----

    #[test]
    fn wants_attach_only_fires_on_the_subcommand() {
        assert!(wants_attach(&argv(&["hyperpanes", "attach"])));
        assert!(wants_attach(&argv(&["hyperpanes", "attach", "pane-1"])));
        assert!(!wants_attach(&argv(&["hyperpanes"])));
        assert!(!wants_attach(&argv(&["hyperpanes", "pair"])));
        // Not a flag, and never matched past argv[1] — `-c "attach"` must launch the GUI.
        assert!(!wants_attach(&argv(&["hyperpanes", "-c", "attach"])));
    }

    // ---- flag parsing ----

    #[test]
    fn parse_defaults_to_the_chooser_and_the_letterbox_policy() {
        let o = AttachOpts::parse(&argv(&["hyperpanes", "attach"])).expect("parses");
        assert_eq!(o.query, None);
        assert!(!o.list);
        assert_eq!(
            o.policy,
            ResizePolicy::Observe,
            "an attach client must not reflow the desktop unless asked"
        );
        assert_eq!(o.detach, DEFAULT_DETACH_PREFIX);
    }

    #[test]
    fn parse_reads_a_pane_and_every_flag() {
        let o = AttachOpts::parse(&argv(&[
            "hyperpanes",
            "attach",
            "pane-abc",
            "--resize",
            "--detach-key",
            "C-]",
        ]))
        .expect("parses");
        assert_eq!(o.query.as_deref(), Some("pane-abc"));
        assert_eq!(o.policy, ResizePolicy::Request);
        assert_eq!(o.detach, 0x1D);

        let l = AttachOpts::parse(&argv(&["hyperpanes", "attach", "-l"])).expect("parses");
        assert!(l.list);
    }

    #[test]
    fn parse_rejects_typos_rather_than_silently_ignoring_them() {
        // A dropped `--resize` would silently keep the (opposite) default policy.
        assert!(AttachOpts::parse(&argv(&["hyperpanes", "attach", "--resiez"])).is_err());
        assert!(AttachOpts::parse(&argv(&["hyperpanes", "attach", "--detach-key"])).is_err());
        assert!(AttachOpts::parse(&argv(&["hyperpanes", "attach", "--detach-key", "x"])).is_err());
        assert!(AttachOpts::parse(&argv(&["hyperpanes", "attach", "a", "b"])).is_err());
    }

    // ---- chooser formatting ----

    #[test]
    fn ago_reads_like_a_person_wrote_it() {
        let now = 1_000_000_000_000u64;
        assert_eq!(ago(None, now), "never");
        assert_eq!(ago(Some(now), now), "now");
        assert_eq!(ago(Some(now - 5_000), now), "5s ago");
        assert_eq!(ago(Some(now - 120_000), now), "2m ago");
        assert_eq!(ago(Some(now - 7_200_000), now), "2h ago");
        assert_eq!(ago(Some(now - 172_800_000), now), "2d ago");
        // A clock that ran backwards must not underflow into a huge number.
        assert_eq!(ago(Some(now + 5_000), now), "now");
    }

    #[test]
    fn a_row_shows_the_grid_so_the_letterbox_warning_makes_sense() {
        let now = 1_000_000_000_000u64;
        let line = row(1, &meta("pane-abc", Some(120), Some(40)), now);
        assert!(line.contains("pane-abc"), "{line}");
        assert!(line.contains("120x40"), "{line}");
        assert!(line.contains("/tmp"), "{line}");
        // A daemon that predates the grid fields must still render a row.
        let unknown = row(2, &meta("pane-xyz", None, None), now);
        assert!(unknown.contains("?x?"), "{unknown}");
    }

    #[test]
    fn the_clear_sequence_wipes_scrollback_too() {
        // ED 3 — without it a repaint stacks a second copy of the replay in the scrollback.
        assert_eq!(CLEAR_SCREEN, b"\x1b[H\x1b[2J\x1b[3J");
    }
}

// ---------------------------------------------------------------------------
// raw-mode restoration
// ---------------------------------------------------------------------------

/// Restoring the terminal is the one failure in this client a user cannot recover from
/// without `reset(1)`, so it gets a real pty rather than a mock.
///
/// Deliberately **one** test: [`tty::restore`] reads process-global state that the first
/// [`tty::RawGuard`] publishes and nothing ever clears, so a second test constructing a
/// guard in parallel would decide which descriptor the globals name. Everything that
/// enters raw mode in this crate lives in this function, and it runs in order.
#[cfg(all(test, unix))]
mod raw_mode_tests {
    use super::tty;
    use std::os::unix::io::RawFd;

    struct Pty {
        master: RawFd,
        slave: RawFd,
    }

    impl Drop for Pty {
        fn drop(&mut self) {
            // SAFETY: both descriptors were opened by `openpty` and are closed once.
            unsafe {
                libc::close(self.slave);
                libc::close(self.master);
            }
        }
    }

    fn open_pty() -> Pty {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        // SAFETY: `openpty` writes the two descriptors; the three optional pointers are null.
        // They are spelled `null_mut` and typed explicitly because the trailing two are
        // `*mut` on Apple and `*const` on Linux — `*mut T` coerces to either.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut::<libc::c_char>(),
                std::ptr::null_mut::<libc::termios>(),
                std::ptr::null_mut::<libc::winsize>(),
            )
        };
        assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
        Pty { master, slave }
    }

    fn attrs(fd: RawFd) -> libc::termios {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `tcgetattr` fills the struct we own; `fd` is a pty.
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0);
        t
    }

    /// `libc::termios` is a plain C struct with no `PartialEq`.
    ///
    /// `PENDIN`/`FLUSHO` are masked out of `c_lflag`: they are kernel *status* bits, not
    /// settings — leaving canonical mode with input queued sets `PENDIN` behind our back —
    /// and comparing them would fail a restore that is byte-for-byte correct.
    fn same(a: &libc::termios, b: &libc::termios) -> bool {
        let settings = |t: &libc::termios| t.c_lflag & !(libc::PENDIN | libc::FLUSHO);
        a.c_iflag == b.c_iflag
            && a.c_oflag == b.c_oflag
            && a.c_cflag == b.c_cflag
            && settings(a) == settings(b)
            && a.c_cc == b.c_cc
    }

    /// Cooked-mode markers `cfmakeraw` clears. Checking these (rather than "not equal to
    /// the original") proves the guard actually entered raw mode.
    fn is_raw(t: &libc::termios) -> bool {
        t.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG) == 0
    }

    #[test]
    fn the_terminal_comes_back_on_every_exit_path() {
        let pty = open_pty();
        let fd = pty.slave;
        let cooked = attrs(fd);
        assert!(!is_raw(&cooked), "a fresh pty should start cooked");

        // 1. Ordinary scope exit — the detach key, a daemon disconnect, an `?` early return.
        {
            let _raw = tty::RawGuard::enter(fd).expect("enter raw");
            assert!(is_raw(&attrs(fd)), "guard did not enter raw mode");
        }
        assert!(
            same(&attrs(fd), &cooked),
            "drop did not restore the terminal"
        );

        // 2. A panic unwinding through the guard. `run` holds it across the output pump, so
        //    this is the path a bug in the pump takes.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let caught = std::panic::catch_unwind(|| {
            let _raw = tty::RawGuard::enter(fd).expect("enter raw");
            assert!(is_raw(&attrs(fd)));
            panic!("output pump exploded");
        });
        std::panic::set_hook(hook);
        assert!(caught.is_err(), "the panic should have propagated");
        assert!(
            same(&attrs(fd), &cooked),
            "unwinding did not restore the terminal"
        );

        // 3. `SIGINT`/`SIGTERM`/`SIGHUP`/`SIGQUIT` and the panic hook, which cannot reach a
        //    guard on another thread's stack and go through the globals instead. `on_fatal`
        //    itself is `restore()` plus `_exit`, and `_exit` is not testable in-process.
        //    The globals were published by the first `enter` above, for this fd.
        let mut raw = cooked;
        // SAFETY: plain field manipulation on a struct we own, then applied to our pty.
        unsafe {
            libc::cfmakeraw(&mut raw);
            assert_eq!(libc::tcsetattr(fd, libc::TCSANOW, &raw), 0);
        }
        assert!(is_raw(&attrs(fd)));
        tty::restore();
        assert!(
            same(&attrs(fd), &cooked),
            "the signal-handler path did not restore the terminal"
        );
    }
}
