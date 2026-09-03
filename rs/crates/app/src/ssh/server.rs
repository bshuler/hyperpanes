//! The russh listener: auth, channels, and the policy around them (mux backend M3).
//!
//! Everything network-facing lives here. The rules it enforces, in one place so a reviewer
//! can check them against the code below without reading the whole file:
//!
//! 1. **Public key only.** [`build_config`] advertises exactly one method, and every other
//!    `auth_*` callback is explicitly overridden to reject — including `auth_publickey_offered`,
//!    whose russh default is `Accept`.
//! 2. **The authorized-keys file is the sole authority**, re-read on every connection so
//!    `hyperpanes ssh revoke` takes effect without a restart. No file, or an empty one, means
//!    nobody gets in.
//! 3. **The username is not a credential.** It is only ever read as a *pane hint*; two
//!    different usernames with the same key get identical access. There is no per-user
//!    anything to confuse with authorization.
//! 4. **No shell, no exec, no subsystem, no forwarding.** A channel is wired to
//!    [`crate::ssh::bridge`] and nothing else; `exec` is parsed only as a pane query. sftp,
//!    port forwarding, X11 and agent forwarding are all refused (russh's defaults refuse the
//!    forwarding ones; `subsystem_request` is overridden because its default is a no-op that
//!    leaves the client hanging).
//! 5. **Nothing secret is logged.** Only fingerprints, peer addresses, and pane uids.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use hyperpanes_core::session::attach::{self, ResizePolicy};
use russh::keys::ssh_key::PublicKey;
use russh::server::{self, Auth, Handle, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet, Pty};
use tokio::net::TcpListener;
use crate::ssh::bridge::{self, Bridge, BridgeParams};
use crate::ssh::config::{SshPaths, SshSettings};
use crate::ssh::keys;

/// Everything a connection needs; shared immutably by every handler.
#[derive(Debug, Clone)]
pub struct ServeOpts {
    /// Daemon salt — which hyperpanes workspace this port fronts.
    pub salt: String,
    /// Where the key sources live. Both the `ssh-authorized-keys` file and the `sshKey` column
    /// of `device-tokens.json` are re-read per authentication attempt, so a revoke through
    /// either door takes effect on the next connection without a restart.
    pub paths: SshPaths,
    /// Whether an SSH client may reflow panes for everyone.
    pub policy: ResizePolicy,
    /// Detach prefix byte.
    pub detach: u8,
}

/// Build the SSH server configuration.
///
/// Split out and public so the security-relevant choices are directly assertable in tests
/// rather than buried in a `serve` function that needs a socket to run.
pub fn build_config(host_key: russh::keys::PrivateKey) -> server::Config {
    server::Config {
        // Identify honestly. A fake OpenSSH banner would only mislead the operator reading
        // their own logs.
        server_id: russh::SshId::Standard(std::borrow::Cow::Borrowed(concat!(
            "SSH-2.0-hyperpanes_",
            env!("CARGO_PKG_VERSION")
        ))),
        // The whole auth policy, in one line: publickey and nothing else. A client that only
        // knows how to send a password is told there is no such method rather than being
        // given a prompt that can never succeed.
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        keys: vec![host_key],
        // An attached pane is idle whenever its human is thinking, so the default 10-minute
        // inactivity reap would drop people mid-session. Liveness is enforced by keepalives
        // instead: ~90 s to notice a phone that fell off the network, without ever
        // disconnecting an idle-but-present client.
        inactivity_timeout: None,
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        keepalive_max: 3,
        // Interactive typing: don't let Nagle add latency to single keystrokes.
        nodelay: true,
        // Fewer tries than the default 10 — there is exactly one thing to try here.
        max_auth_attempts: 6,
        ..Default::default()
    }
}

/// Load settings + keys and run the listener until it fails. Blocking; owns its own runtime.
///
/// `verbose` prints to stdout (the `hyperpanes ssh serve` foreground path); the daemon path
/// passes `false` and everything goes to the debug log instead.
pub fn serve_blocking(paths: &SshPaths, salt: &str, verbose: bool) -> Result<(), String> {
    let settings = SshSettings::load(&paths.settings)?;
    let addr = settings.resolve_bind()?;
    let detach = attach::parse_detach_key(&settings.detach_key)?;
    let policy = if settings.allow_resize {
        ResizePolicy::Request
    } else {
        ResizePolicy::Observe
    };

    let (host_key, fresh) = keys::load_or_create_host_key(&paths.host_key)?;
    let fp = keys::host_fingerprint(&host_key);
    let allowed = keys::Authorizer::load(paths)?;
    let live = allowed.live_len(keys::now_ms());
    for w in &allowed.warnings {
        let msg = format!("ssh: authorized keys: {w}");
        if verbose {
            eprintln!("{msg}");
        }
        tracing::debug!("{}", &msg);
    }
    if live == 0 {
        // Not fatal: the operator may be about to authorize a key, and the running server
        // will pick it up on the next connection. But say so loudly.
        let msg = "ssh: no client keys are authorized — every connection will be rejected. \
                   Add one with `hyperpanes ssh authorize <key>`, or pair a device with \
                   `hyperpanes pair --ssh-key <key>`.";
        if verbose {
            eprintln!("{msg}");
        }
        tracing::debug!("{}", msg);
    }

    let banner = format!(
        "ssh: listening on {addr} (host key {fp}{}, {} authorized client key(s), {})",
        if fresh { ", newly generated" } else { "" },
        live,
        match policy {
            ResizePolicy::Request => "clients may resize panes",
            ResizePolicy::Observe => "clients letterbox",
        }
    );
    tracing::debug!("{}", &banner);
    if verbose {
        println!("{banner}");
        if settings.is_remote_exposed() {
            println!(
                "WARNING: this is not a loopback address. Anyone who can reach {addr} and \
                 holds an authorized key gets a live terminal."
            );
        }
        println!("Press Ctrl-C to stop.");
    }

    let opts = ServeOpts {
        salt: salt.to_string(),
        paths: paths.clone(),
        policy,
        detach,
    };
    let config = Arc::new(build_config(host_key));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("hp-ssh-rt")
        .build()
        .map_err(|e| format!("could not start a tokio runtime for the SSH server: {e}"))?;
    rt.block_on(async move {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("could not bind {addr}: {e}"))?;
        serve_on(listener, config, opts)
            .await
            .map_err(|e| format!("ssh listener stopped: {e}"))
    })
}

/// Serve on an already-bound listener. Runs until the accept loop fails.
///
/// Taking the listener rather than an address is what lets the tests bind port 0 and talk to
/// a real server over a real socket.
pub async fn serve_on(
    listener: TcpListener,
    config: Arc<server::Config>,
    opts: ServeOpts,
) -> std::io::Result<()> {
    use russh::server::Server as _;
    let mut sv = SshServer {
        opts: Arc::new(opts),
    };
    sv.run_on_socket(config, &listener).await
}

struct SshServer {
    opts: Arc<ServeOpts>,
}

impl server::Server for SshServer {
    type Handler = SshHandler;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> SshHandler {
        SshHandler {
            opts: self.opts.clone(),
            peer: peer.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
            user: String::new(),
            key_label: String::new(),
            channels: HashMap::new(),
        }
    }

    fn handle_session_error(&mut self, error: russh::Error) {
        // Includes ordinary disconnects; only ever a debug-log line.
        tracing::debug!("ssh: session ended: {error}");
    }
}

/// Per-channel state between `pty_request` and `shell_request`/`exec_request`.
struct ChanState {
    /// The client's grid, from `pty_request`. `None` until it asks for one.
    pty: Option<(u16, u16)>,
    /// Set once the channel is attached; `None` before that.
    bridge: Option<Bridge>,
}

/// One SSH connection.
pub struct SshHandler {
    opts: Arc<ServeOpts>,
    peer: String,
    user: String,
    key_label: String,
    channels: HashMap<ChannelId, ChanState>,
}

impl SshHandler {
    /// Start the attach bridge for `channel`, or explain on the channel why it cannot.
    async fn start(
        &mut self,
        channel: ChannelId,
        query: Option<String>,
        list: bool,
        session: &mut Session,
    ) {
        let Some(state) = self.channels.get_mut(&channel) else {
            let _ = session.channel_failure(channel);
            return;
        };
        if state.bridge.is_some() {
            // A second shell/exec on one channel is a protocol error, not something to
            // silently double-attach.
            let _ = session.channel_failure(channel);
            return;
        }
        // Rendering a live pane means emitting ANSI into a raw terminal. Without a pty the
        // client's terminal is still line-disciplined and the result is garbage, so refuse
        // and say why rather than producing it. `list` is plain text and needs no pty.
        if state.pty.is_none() && !list {
            let _ = session.channel_success(channel);
            let _ = session.data(
                channel,
                &b"hyperpanes: attaching needs a pty. Use `ssh -t`, or run `ssh \
                   <host> list` to see the panes.\r\n"[..],
            );
            let _ = session.exit_status_request(channel, 1);
            let _ = session.eof(channel);
            let _ = session.close(channel);
            return;
        }

        let handle: Handle = session.handle();
        let params = BridgeParams {
            salt: self.opts.salt.clone(),
            query: query.clone(),
            list_only: list,
            term: state.pty.unwrap_or((80, 24)),
            policy: self.opts.policy,
            detach: self.opts.detach,
            peer: self.peer.clone(),
        };
        tracing::debug!("ssh: {} ({}) opening a channel (query {:?}, list {list})",
            self.peer, self.key_label, query);
        state.bridge = Some(bridge::spawn(params, handle, channel));
        let _ = session.channel_success(channel);
    }
}

impl server::Handler for SshHandler {
    type Error = russh::Error;

    // ---- authentication ----------------------------------------------------------------

    /// Reject, always. `none` must never authenticate; russh's default already rejects, and
    /// this override exists so that stays true if the default ever changes.
    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(reject())
    }

    /// Reject, always. There is no password to be right: nothing in hyperpanes stores or
    /// checks one, and adding one would put a guessable credential in front of every
    /// terminal on the machine.
    async fn auth_password(&mut self, user: &str, _password: &str) -> Result<Auth, Self::Error> {
        tracing::debug!("ssh: {} tried password auth as {user:?} — refused (publickey only)",
            self.peer);
        Ok(reject())
    }

    /// Reject, always — same reason as `auth_password`.
    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _user: &str,
        _submethods: &str,
        _response: Option<server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        Ok(reject())
    }

    /// Reject, always. An OpenSSH *certificate* authenticates against a CA, and hyperpanes
    /// has no CA: the authorized-keys file lists key material, and only key material on that
    /// list gets in. russh's default already rejects; this override keeps that true if the
    /// default ever changes, the same reason `auth_none` is spelled out above.
    async fn auth_openssh_certificate(
        &mut self,
        _user: &str,
        _certificate: &russh::keys::Certificate,
    ) -> Result<Auth, Self::Error> {
        Ok(reject())
    }

    /// The probe a client makes before signing. **russh's default here is `Accept`**, which
    /// would let an unknown key proceed; overriding it is what makes the probe answer the
    /// truth. Ownership is not proven at this point, so nothing but the answer depends on it.
    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(if self.lookup(public_key).is_some() {
            Auth::Accept
        } else {
            reject()
        })
    }

    /// The real check: the signature is verified by russh before this runs, so the client has
    /// proven it holds the private half. All that is left is whether that public half is on
    /// the list — re-read from disk here so a revoke takes effect on the next connection.
    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        match self.lookup(public_key) {
            Some(label) => {
                self.user = user.to_string();
                self.key_label = label.clone();
                tracing::debug!("ssh: {} authenticated as {user:?} with {} ({label})",
                    self.peer,
                    keys::fingerprint(public_key));
                Ok(Auth::Accept)
            }
            None => {
                tracing::debug!("ssh: {} REJECTED — {} is not authorized",
                    self.peer,
                    keys::fingerprint(public_key));
                Ok(reject())
            }
        }
    }

    // ---- channels ----------------------------------------------------------------------

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(
            channel.id(),
            ChanState {
                pty: None,
                bridge: None,
            },
        );
        reply.accept().await;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let size = clamp_grid(col_width, row_height);
        match self.channels.get_mut(&channel) {
            Some(state) => {
                state.pty = Some(size);
                session.channel_success(channel)?;
            }
            None => session.channel_failure(channel)?,
        }
        Ok(())
    }

    /// A plain `ssh host`: attach, choosing a pane from the username hint or interactively.
    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let query = query_from_user(&self.user);
        let list = query.as_deref() == Some("list");
        let query = if list { None } else { query };
        self.start(channel, query, list, session).await;
        Ok(())
    }

    /// `ssh host <pane>` / `ssh host list`. The command is NOT executed — there is no shell
    /// behind this server. It is read only as a pane selector.
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data).to_string();
        let (query, list) = parse_command(&cmd);
        self.start(channel, query, list, session).await;
        Ok(())
    }

    /// No sftp, no scp, no anything. russh's default is a silent no-op that leaves the client
    /// waiting forever, so answer with an explicit failure.
    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        tracing::debug!("ssh: {} asked for subsystem {name:?} — refused",
            self.peer);
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(state) = self.channels.get(&channel) {
            if let Some(bridge) = &state.bridge {
                bridge.input(data);
            }
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let (cols, rows) = clamp_grid(col_width, row_height);
        if let Some(state) = self.channels.get_mut(&channel) {
            state.pty = Some((cols, rows));
            if let Some(bridge) = &state.bridge {
                bridge.resize(cols, rows);
            }
            session.channel_success(channel)?;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }

    /// The client hung up. Dropping the [`Bridge`] EOFs the input reader, which detaches and
    /// unblocks the attach thread — the session itself keeps running.
    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.remove(&channel);
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.remove(&channel);
        Ok(())
    }
}

impl SshHandler {
    /// Is this key authorized? Returns its label and source. Re-reads both key sources every
    /// call: the lists are tiny and a revoke that needs a restart is a revoke that does not work.
    /// An expired device pairing is not authorized — the SSH door shuts on the same millisecond
    /// as the control-API door.
    fn lookup(&self, key: &PublicKey) -> Option<String> {
        match keys::Authorizer::load(&self.opts.paths) {
            Ok(set) => set.authorize(key, keys::now_ms()).map(|e| e.describe()),
            Err(e) => {
                // Fail CLOSED. An unreadable or wrong-mode key file rejects everyone rather
                // than admitting them.
                tracing::debug!("ssh: refusing everyone — {e}");
                None
            }
        }
    }
}

/// A rejection that offers publickey again (and nothing else) as the way forward.
fn reject() -> Auth {
    Auth::Reject {
        proceed_with_methods: Some(MethodSet::from(&[MethodKind::PublicKey][..])),
        partial_success: false,
    }
}

/// SSH sends the grid as `u32`. Clamp into the `u16` the pty layer speaks, and never accept a
/// zero dimension — a 0-column grid would divide by zero downstream.
fn clamp_grid(cols: u32, rows: u32) -> (u16, u16) {
    (
        cols.clamp(1, u16::MAX as u32) as u16,
        rows.clamp(1, u16::MAX as u32) as u16,
    )
}

/// Read an `exec` command as a pane selector. Returns `(query, list_only)`.
///
/// Deliberately tiny: this is not a shell and must never look like one.
pub fn parse_command(cmd: &str) -> (Option<String>, bool) {
    let cmd = cmd.trim();
    let mut words = cmd.split_whitespace();
    match words.next() {
        None => (None, false),
        Some("list" | "ls" | "sessions") => (None, true),
        // `ssh host attach pane-x` — accept the verb people will type out of habit.
        Some("attach") => (words.next().map(str::to_string), false),
        Some(first) => (Some(first.to_string()), false),
    }
}

/// Read the SSH *username* as a pane hint — and only when it is unmistakably one.
///
/// `ssh pane-3f2a@host` is a genuinely nice way to reach a pane from a phone, but a username
/// is also whatever the client's OS put there by default (`bert`, `mobile`, `root`). Treating
/// that as a pane query would make every ordinary `ssh host` fail with "no session matches
/// 'bert'" instead of showing the chooser. So only the two unambiguous spellings count.
pub fn query_from_user(user: &str) -> Option<String> {
    let user = user.trim();
    if user == "list" {
        return Some("list".to_string());
    }
    if user.starts_with("pane-") {
        return Some(user.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::testutil::tmpdir;
    use russh::client::AuthResult;

    #[test]
    fn the_config_advertises_public_key_and_nothing_else() {
        let dir = tmpdir("cfg-auth");
        let (key, _) = keys::load_or_create_host_key(&dir.path().join("hk")).unwrap();
        let cfg = build_config(key);
        let methods: Vec<_> = cfg.methods.iter().collect();
        assert_eq!(
            methods.len(),
            1,
            "exactly one auth method may be advertised, got {methods:?}"
        );
        assert_eq!(methods[0], &MethodKind::PublicKey);
        assert!(
            !cfg.keys.is_empty(),
            "a server with no host key would negotiate nothing"
        );
        assert!(
            cfg.inactivity_timeout.is_none(),
            "an idle attached pane must not be reaped"
        );
        assert!(cfg.keepalive_interval.is_some(), "dead TCP must be noticed");
    }

    #[test]
    fn rejection_never_offers_password_or_keyboard_interactive() {
        let Auth::Reject {
            proceed_with_methods: Some(m),
            partial_success,
        } = reject()
        else {
            panic!("reject() must be a rejection carrying the remaining methods");
        };
        assert!(!partial_success);
        let methods: Vec<_> = m.iter().collect();
        assert_eq!(methods, vec![&MethodKind::PublicKey]);
    }

    #[test]
    fn a_command_is_read_as_a_pane_selector_never_run() {
        assert_eq!(parse_command("list"), (None, true));
        assert_eq!(parse_command("  ls  "), (None, true));
        assert_eq!(
            parse_command("pane-3f2a"),
            (Some("pane-3f2a".to_string()), false)
        );
        assert_eq!(
            parse_command("attach pane-3f2a"),
            (Some("pane-3f2a".to_string()), false)
        );
        assert_eq!(parse_command(""), (None, false));
        // A shell command is a pane query that will simply not match — it is never executed.
        assert_eq!(
            parse_command("rm -rf /"),
            (Some("rm".to_string()), false),
            "the first word is a query, not a program"
        );
    }

    #[test]
    fn only_an_unmistakable_username_is_a_pane_query() {
        assert_eq!(query_from_user("pane-3f2a"), Some("pane-3f2a".to_string()));
        assert_eq!(query_from_user("list"), Some("list".to_string()));
        // Ordinary usernames must fall through to the chooser, not fail to match.
        for u in ["bert", "root", "mobile", "ubuntu", ""] {
            assert_eq!(query_from_user(u), None, "{u:?} is not a pane query");
        }
    }

    // ---- end-to-end authentication ------------------------------------------------------
    //
    // These stand up the REAL server (`serve_on` + `build_config`) on 127.0.0.1:0 and drive it
    // with a real russh client. Nothing about authentication is asserted by inspection here:
    // every claim below is a full SSH handshake against the same code path a phone hits.
    //
    // The salt points at an empty temp directory, so no channel can reach a live daemon even
    // if one of these tests grew one — but none of them opens a channel: this is the door.

    /// A client that trusts whatever host key it is shown. Fine here — the host key is not what
    /// is under test, and the server is one we just started on loopback.
    struct BlindClient;

    impl russh::client::Handler for BlindClient {
        type Error = russh::Error;

        async fn check_server_key(
            &mut self,
            _server_public_key: &russh::keys::PublicKeyOrCertificate,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }

    fn a_client_key() -> russh::keys::PrivateKey {
        russh::keys::PrivateKey::random(&mut rand::rng(), ssh_key::Algorithm::Ed25519).unwrap()
    }

    /// Start the real server on an ephemeral loopback port; returns its address.
    async fn start(paths: &SshPaths) -> SocketAddr {
        let (host_key, _) = keys::load_or_create_host_key(&paths.host_key).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let opts = ServeOpts {
            // An empty directory: there is no daemon here, and no test below opens a channel.
            salt: paths.settings.parent().unwrap().display().to_string(),
            paths: paths.clone(),
            policy: ResizePolicy::Observe,
            detach: 0x1c,
        };
        let config = Arc::new(build_config(host_key));
        tokio::spawn(async move {
            let _ = serve_on(listener, config, opts).await;
        });
        addr
    }

    async fn connect(addr: SocketAddr) -> russh::client::Handle<BlindClient> {
        russh::client::connect(
            Arc::new(russh::client::Config::default()),
            addr,
            BlindClient,
        )
        .await
        .unwrap()
    }

    /// Does this key open the door? One fresh connection per attempt, as in real life.
    async fn try_key(addr: SocketAddr, key: &russh::keys::PrivateKey) -> bool {
        let mut session = connect(addr).await;
        session
            .authenticate_publickey(
                "hyperpanes",
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key.clone()), None),
            )
            .await
            .unwrap()
            .success()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_authorized_key_gets_in_and_an_unlisted_one_does_not() {
        let dir = tmpdir("e2e-file-keys");
        let paths = SshPaths::under(dir.path());
        let allowed = a_client_key();
        let stranger = a_client_key();
        keys::authorize_key(
            &paths.authorized_keys,
            &allowed.public_key().to_openssh().unwrap(),
            Some("laptop"),
        )
        .unwrap();
        let addr = start(&paths).await;

        assert!(
            try_key(addr, &allowed).await,
            "a key in ssh-authorized-keys must authenticate"
        );
        assert!(
            !try_key(addr, &stranger).await,
            "a key nobody authorized must NOT authenticate"
        );

        // Revoking takes effect on the very next connection — no restart, no reload command.
        assert_eq!(
            keys::revoke_key(&paths.authorized_keys, "laptop").unwrap(),
            1
        );
        assert!(
            !try_key(addr, &allowed).await,
            "a revoked key must stop working immediately"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_paired_device_key_gets_in_until_its_pairing_expires() {
        use hyperpanes_core::persistence::device_tokens::{save_to, DeviceRecord};

        let dir = tmpdir("e2e-device-keys");
        let paths = SshPaths::under(dir.path());
        let phone = a_client_key();
        let old_phone = a_client_key();
        let no_key_device = a_client_key();
        let now = keys::now_ms();

        save_to(
            &paths.device_tokens,
            &[
                DeviceRecord {
                    label: "phone".into(),
                    token: "t1".into(),
                    expires_at: Some(now + 3_600_000),
                    ssh_key: Some(phone.public_key().to_openssh().unwrap()),
                },
                DeviceRecord {
                    label: "old-phone".into(),
                    token: "t2".into(),
                    expires_at: Some(now - 1),
                    ssh_key: Some(old_phone.public_key().to_openssh().unwrap()),
                },
                DeviceRecord {
                    label: "api-only".into(),
                    token: "t3".into(),
                    expires_at: None,
                    ssh_key: None,
                },
            ],
        )
        .unwrap();
        let addr = start(&paths).await;

        // No ssh-authorized-keys file exists at all: the device table is the only source.
        assert!(!paths.authorized_keys.exists());
        assert!(
            try_key(addr, &phone).await,
            "a device paired with --ssh-key must authenticate"
        );
        assert!(
            !try_key(addr, &old_phone).await,
            "an expired pairing must not authenticate over SSH either"
        );
        assert!(
            !try_key(addr, &no_key_device).await,
            "a key that is on no list must not authenticate"
        );

        // `hyperpanes revoke <label>` rewrites this file without the record; the SSH door shuts
        // with the control-API door, on the next connection.
        save_to(&paths.device_tokens, &[]).unwrap();
        assert!(
            !try_key(addr, &phone).await,
            "revoking the device must revoke its SSH key"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn there_is_no_password_and_no_none_auth_even_with_no_keys_installed() {
        let dir = tmpdir("e2e-no-password");
        let paths = SshPaths::under(dir.path());
        let addr = start(&paths).await;

        let mut session = connect(addr).await;
        let none = session.authenticate_none("hyperpanes").await.unwrap();
        assert!(!none.success(), "`none` auth must never succeed");
        let AuthResult::Failure {
            remaining_methods, ..
        } = none
        else {
            unreachable!()
        };
        // What the server tells a client to try next is publickey, and only publickey — a
        // password prompt that can never succeed is worse than no prompt at all.
        assert_eq!(
            remaining_methods.iter().collect::<Vec<_>>(),
            vec![&MethodKind::PublicKey]
        );

        let mut session = connect(addr).await;
        assert!(
            !session
                .authenticate_password("hyperpanes", "hunter2")
                .await
                .unwrap()
                .success(),
            "password auth must never succeed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_unreadable_key_source_locks_everybody_out_rather_than_letting_them_in() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmpdir("e2e-fail-closed");
        let paths = SshPaths::under(dir.path());
        let allowed = a_client_key();
        keys::authorize_key(
            &paths.authorized_keys,
            &allowed.public_key().to_openssh().unwrap(),
            Some("laptop"),
        )
        .unwrap();
        let addr = start(&paths).await;
        assert!(try_key(addr, &allowed).await);

        // World-writable = anyone on the box can append their own key. Refusing to read the
        // file must lock the door, not prop it open.
        std::fs::set_permissions(
            &paths.authorized_keys,
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        assert!(
            !try_key(addr, &allowed).await,
            "a badly-permissioned key file must reject everyone"
        );
    }

    #[test]
    fn the_listener_address_comes_from_settings_and_defaults_to_loopback() {
        // The bind policy lives in `config`, but this is the milestone's headline security
        // claim, so it is asserted here too, on the value `serve_blocking` actually binds.
        let dir = tmpdir("bind-default");
        let paths = SshPaths::under(dir.path());
        let settings = SshSettings::load(&paths.settings).unwrap();
        let addr = settings.resolve_bind().unwrap();
        assert!(
            addr.ip().is_loopback(),
            "the default bind must be loopback, got {addr}"
        );
        assert_eq!(addr.port(), crate::ssh::config::DEFAULT_PORT);
    }

    #[test]
    fn a_zero_or_huge_grid_is_clamped() {
        assert_eq!(clamp_grid(0, 0), (1, 1));
        assert_eq!(clamp_grid(80, 24), (80, 24));
        assert_eq!(clamp_grid(u32::MAX, u32::MAX), (u16::MAX, u16::MAX));
    }
}
