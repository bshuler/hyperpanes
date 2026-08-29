//! The embedded SSH server — mux backend **M3** (`docs/mux-backend-plan.md`).
//!
//! The point of the milestone: a phone with *no hyperpanes software on it* — Termius, Blink,
//! or plain `ssh` — points at this port and lands in a live pane. There is no shell behind the
//! channel: an opened session runs the M2 attach client
//! ([`hyperpanes_core::session::attach`]) directly, so an SSH viewer sees exactly what the
//! desktop sees and detaching leaves the pane running.
//!
//! ```text
//!   phone ──ssh──▶ server.rs (russh)  ──▶ bridge.rs ──▶ attach.rs ──uds──▶ hyperpanesd
//!                  auth: keys.rs           channel↔pty     (M2, reused verbatim)
//!                  policy: config.rs
//! ```
//!
//! # Trust boundary
//!
//! This is a network listener that hands out live terminals, so the defaults are the
//! conservative ones and each relaxation is a separate, explicit act:
//!
//! | control | default | where |
//! |---|---|---|
//! | server runs at all | **off** (`enabled: false`) | [`config`] |
//! | bind address | **127.0.0.1** | [`config`] |
//! | non-loopback bind | refused unless `allowRemote` *as well* | [`config::SshSettings::resolve_bind`] |
//! | auth methods | **public key only** — no password, no keyboard-interactive, no `none` | [`server`] |
//! | unknown key | rejected (russh's default `auth_publickey_offered` is *Accept*, so it is overridden) | [`server`] |
//! | host key | generated once, `0600`, refused if group/other-readable | [`keys`] |
//! | pane reflow | **off** (`ResizePolicy::Observe`) — a phone must not resize the desktop | [`config`] |
//!
//! No secret is ever logged: the private host key is read and passed to russh as bytes and
//! never rendered, and only *fingerprints* appear in output.
//!
//! # Divergence from the plan, stated plainly
//!
//! `mux-backend-plan.md` proposes reusing `device-tokens.json` and the `hyperpanes pair` QR
//! flow for SSH auth. That is not done here and should be reviewed as a deliberate change:
//! those are *bearer tokens*, and an SSH client authenticates by proving possession of a
//! private key it will not hand over — there is nothing to reuse but the UI. Client keys live
//! in their own OpenSSH `authorized_keys`-format file instead
//! ([`config::SshPaths::authorized_keys`]), managed by `hyperpanes ssh authorize|keys|revoke`.
//! It also keeps this milestone out of `persistence/`, which another agent owns.
//!
//! # Windows
//!
//! The server is `#[cfg(unix)]`. This is honest, not lazy: `AttachWriter::disconnect()` is a
//! documented no-op on Windows (a named-pipe handle has no half-close), so when an SSH client
//! hangs up there is no way to unblock the thread sitting in `pump_output` — every connection
//! would leak a thread and a daemon client. Fixing it means changing core's named-pipe
//! transport, which is outside this milestone. [`config`] is portable and is compiled and
//! tested on the Windows leg; everything else refuses with a message that says the above.

/// Settings + on-disk layout. Portable: no crypto, no unix APIs, tested on every CI leg.
pub mod config;

#[cfg(unix)]
mod bridge;
#[cfg(unix)]
pub mod keys;
#[cfg(unix)]
pub mod server;

/// True for `hyperpanes ssh …`. Only `argv[1]`, so `hyperpanes -c "ssh box"` still launches
/// the GUI and runs ssh in a pane.
pub fn wants_ssh(argv: &[String]) -> bool {
    argv.get(1).map(|a| a == "ssh").unwrap_or(false)
}

const SSH_USAGE: &str = "\
hyperpanes ssh — the embedded SSH front door (attach to a pane from a phone)

USAGE:
    hyperpanes ssh status                 Show settings, bind address, host key, allowed keys
    hyperpanes ssh enable [--port N] [--bind ADDR] [--allow-remote] [--allow-resize]
                                          Turn the server on (and adjust settings)
    hyperpanes ssh disable                Turn the server off
    hyperpanes ssh authorize <key|path> [--label NAME]
                                          Allow a client public key (an authorized_keys line,
                                          a .pub file's contents, or a path to one)
    hyperpanes ssh keys                   List the allowed client keys
    hyperpanes ssh revoke <label|fingerprint>
                                          Remove an allowed client key
    hyperpanes ssh serve                  Run the listener in the foreground (for testing)

NOTES:
    The server binds 127.0.0.1 and is off until you enable it. Binding anything else needs
    BOTH \"bind\" and \"allowRemote\": true — prefer a Tailscale address over 0.0.0.0.
    Auth is public key only; there is no password auth and no shell behind the channel.
";

/// `hyperpanes ssh <subcommand>`.
#[cfg(unix)]
pub fn run(argv: &[String]) -> std::io::Result<()> {
    match run_inner(argv) {
        Ok(()) => Ok(()),
        Err(msg) => {
            eprintln!("hyperpanes ssh: {msg}");
            std::process::exit(1);
        }
    }
}

/// Windows: refuse with the real reason rather than pretending the feature is there.
#[cfg(not(unix))]
pub fn run(_argv: &[String]) -> std::io::Result<()> {
    eprintln!(
        "hyperpanes ssh is not available on Windows.\n\
         The attach client cannot half-close a named pipe, so an SSH client hanging up would \
         leak a thread and a daemon connection per session. See rs/crates/app/src/ssh/mod.rs. \
         Attaching locally (`hyperpanes attach`) works on every platform."
    );
    std::process::exit(2);
}

#[cfg(unix)]
fn run_inner(argv: &[String]) -> Result<(), String> {
    use config::{SshPaths, SshSettings};

    let paths = SshPaths::from_env();
    let sub = argv.get(2).map(String::as_str).unwrap_or("status");
    let rest = argv.get(3..).unwrap_or(&[]);

    match sub {
        "-h" | "--help" | "help" => {
            print!("{SSH_USAGE}");
            Ok(())
        }
        "status" => status(&paths),
        "enable" | "disable" => {
            let mut s = SshSettings::load(&paths.settings)?;
            s.enabled = sub == "enable";
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--port" => {
                        let v = rest.get(i + 1).ok_or("--port needs a value")?;
                        s.port = v.parse().map_err(|_| format!("bad port {v:?}"))?;
                        i += 1;
                    }
                    "--bind" => {
                        s.bind = rest.get(i + 1).ok_or("--bind needs a value")?.clone();
                        i += 1;
                    }
                    "--allow-remote" => s.allow_remote = true,
                    "--no-allow-remote" => s.allow_remote = false,
                    "--allow-resize" => s.allow_resize = true,
                    "--no-allow-resize" => s.allow_resize = false,
                    other => return Err(format!("unknown flag {other:?} (see `ssh --help`)")),
                }
                i += 1;
            }
            // Refuse to *save* a configuration that would be refused at bind time, so nobody
            // discovers the policy only when the daemon quietly fails to listen.
            if s.enabled {
                s.resolve_bind()?;
            }
            s.save(&paths.settings)?;
            if s.enabled {
                // Materialize the host key now so `status` can show a fingerprint the user can
                // compare against what their client prints on first connect.
                let (key, fresh) = keys::load_or_create_host_key(&paths.host_key)?;
                if fresh {
                    println!("Generated a host key: {}", keys::host_fingerprint(&key));
                }
                println!(
                    "SSH server enabled on {}. It starts with the session daemon — restart \
                     hyperpanes (or run `hyperpanes ssh serve`) to listen now.",
                    s.resolve_bind()?
                );
                if s.is_remote_exposed() {
                    println!(
                        "WARNING: {} is not loopback. Every terminal in this workspace is \
                         reachable from the network for anyone holding an authorized key.",
                        s.bind
                    );
                }
                let allowed = keys::load_authorized(&paths.authorized_keys)?;
                if allowed.is_empty() {
                    println!(
                        "No client keys are authorized yet, so nobody can connect. Add one \
                         with `hyperpanes ssh authorize ~/.ssh/id_ed25519.pub`."
                    );
                }
            } else {
                println!("SSH server disabled.");
            }
            Ok(())
        }
        "authorize" => {
            let arg = rest.first().ok_or(
                "usage: hyperpanes ssh authorize <key|path/to/key.pub> [--label NAME]",
            )?;
            let mut label = None;
            let mut i = 1;
            while i < rest.len() {
                if rest[i] == "--label" {
                    label = Some(rest.get(i + 1).ok_or("--label needs a value")?.clone());
                    i += 1;
                }
                i += 1;
            }
            let added = keys::authorize_key(&paths.authorized_keys, arg, label.as_deref())?;
            let key = keys::parse_public_key(arg)?;
            if added {
                println!("Authorized {}", keys::fingerprint(&key));
            } else {
                println!("Already authorized: {}", keys::fingerprint(&key));
            }
            Ok(())
        }
        "keys" => {
            let set = keys::load_authorized(&paths.authorized_keys)?;
            if set.is_empty() {
                println!("No client keys are authorized. Nobody can connect over SSH.");
            } else {
                println!("Authorized client keys ({}):", set.len());
                for k in &set.keys {
                    println!("  {}  {}", keys::fingerprint(k), keys::label_for(k));
                }
            }
            for w in &set.warnings {
                eprintln!("  warning: {w}");
            }
            Ok(())
        }
        "revoke" => {
            let needle = rest
                .first()
                .ok_or("usage: hyperpanes ssh revoke <label|fingerprint>")?;
            let n = keys::revoke_key(&paths.authorized_keys, needle)?;
            match n {
                0 => println!("No authorized key matched {needle:?}."),
                1 => println!("Revoked 1 key."),
                n => println!("Revoked {n} keys."),
            }
            Ok(())
        }
        "serve" => {
            let salt = hyperpanes_core::persistence::paths::user_data_dir()
                .to_string_lossy()
                .into_owned();
            server::serve_blocking(&paths, &salt, true)
        }
        other => Err(format!(
            "unknown subcommand {other:?}\n\n{SSH_USAGE}"
        )),
    }
}

#[cfg(unix)]
fn status(paths: &config::SshPaths) -> Result<(), String> {
    let s = config::SshSettings::load(&paths.settings)?;
    println!("enabled:    {}", s.enabled);
    match s.resolve_bind() {
        Ok(addr) => println!("listen:     {addr}"),
        Err(e) => println!("listen:     REFUSED — {e}"),
    }
    println!("allowRemote: {}", s.allow_remote);
    println!(
        "allowResize: {}  ({})",
        s.allow_resize,
        if s.allow_resize {
            "SSH clients reflow the pane for everyone"
        } else {
            "SSH clients letterbox; the desktop keeps its grid"
        }
    );
    println!("detachKey:  {}", s.detach_key);
    println!("settings:   {}", paths.settings.display());
    if paths.host_key.exists() {
        match keys::load_or_create_host_key(&paths.host_key) {
            Ok((k, _)) => println!("host key:   {}", keys::host_fingerprint(&k)),
            Err(e) => println!("host key:   UNUSABLE — {e}"),
        }
    } else {
        println!("host key:   not generated yet ({})", paths.host_key.display());
    }
    let set = keys::load_authorized(&paths.authorized_keys)?;
    if set.is_empty() {
        println!("client keys: none — nobody can connect");
    } else {
        println!("client keys ({}):", set.len());
        for k in &set.keys {
            println!("  {}  {}", keys::fingerprint(k), keys::label_for(k));
        }
    }
    for w in &set.warnings {
        println!("  warning: {w}");
    }
    Ok(())
}

/// Start the listener alongside the session daemon, if the user turned it on.
///
/// Called from the `--session-daemon` branch: the daemon process is the one that outlives
/// every GUI window, and the server is just another attach client of its socket. Returns
/// immediately; every failure is logged and non-fatal, because an SSH misconfiguration must
/// never stop the daemon that owns the user's live terminals from starting.
#[cfg(unix)]
pub fn spawn_with_daemon(salt: &str) {
    let paths = config::SshPaths::from_env();
    let settings = match config::SshSettings::load(&paths.settings) {
        Ok(s) => s,
        Err(e) => {
            crate::dbg_log(&format!("ssh: {e} — not starting the SSH server"));
            return;
        }
    };
    if !settings.enabled {
        return;
    }
    let salt = salt.to_string();
    let spawned = std::thread::Builder::new()
        .name("hp-ssh".into())
        .spawn(move || {
            if let Err(e) = server::serve_blocking(&paths, &salt, false) {
                crate::dbg_log(&format!("ssh: server stopped: {e}"));
            }
        });
    if let Err(e) = spawned {
        crate::dbg_log(&format!("ssh: could not start the server thread: {e}"));
    }
}

/// No-op on Windows — see the module docs.
#[cfg(not(unix))]
pub fn spawn_with_daemon(_salt: &str) {}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique temp directory that deletes itself on drop.
    ///
    /// The SSH tests write host keys and authorized-key files and assert on their *modes*, so
    /// they must never run against a developer's real `~/.config` — every path in the suite
    /// comes from here.
    pub struct TmpDir(PathBuf);

    impl TmpDir {
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Create `$TMPDIR/hyperpanes-ssh-test-<tag>-<pid>-<n>/`.
    pub fn tmpdir(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "hyperpanes-ssh-test-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create temp dir");
        TmpDir(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wants_ssh_only_fires_on_the_subcommand() {
        assert!(wants_ssh(&argv(&["hyperpanes", "ssh"])));
        assert!(wants_ssh(&argv(&["hyperpanes", "ssh", "status"])));
        assert!(!wants_ssh(&argv(&["hyperpanes"])));
        assert!(!wants_ssh(&argv(&["hyperpanes", "attach"])));
        // A pane running ssh must not be hijacked into our subcommand.
        assert!(!wants_ssh(&argv(&["hyperpanes", "-c", "ssh box"])));
    }

    #[test]
    fn tmpdir_is_unique_and_cleans_up() {
        let a = testutil::tmpdir("x");
        let b = testutil::tmpdir("x");
        assert_ne!(a.path(), b.path());
        let path = a.path().to_path_buf();
        assert!(path.is_dir());
        drop(a);
        assert!(!path.exists(), "the temp dir must not outlive the test");
    }
}
