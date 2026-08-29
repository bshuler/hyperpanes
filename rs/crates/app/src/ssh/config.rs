//! `ssh-settings.json` plus the on-disk layout for the embedded SSH server (mux backend M3).
//!
//! Deliberately portable (no `russh`, no `ssh-key`, no unix APIs) so the policy — *is it on,
//! and what may it bind to?* — is testable on every CI leg, including the windows-latest one
//! where the server itself is compiled out (see [`super`]).
//!
//! # Security posture encoded here
//!
//! * `enabled` defaults to **false**. An SSH server that turns itself on because a crate was
//!   added is a backdoor, not a feature.
//! * `bind` defaults to **127.0.0.1**. Any non-loopback address is refused unless
//!   `allowRemote` is *also* set — two independent edits, so nobody exposes every terminal on
//!   the box to their coffee-shop LAN by fat-fingering one field. The plan's remote story is
//!   Tailscale (`docs/mobile-client-plan.md`), which is loopback-adjacent by design.
//! * `allowResize` defaults to **false** → `ResizePolicy::Observe`: a phone attaching must
//!   not reflow the desktop's panes.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default listen port. 2222 rather than 22: hyperpanes runs as an unprivileged desktop app
/// and must never look like it is trying to replace the system sshd.
pub const DEFAULT_PORT: u16 = 2222;
/// Default listen address — loopback only.
pub const DEFAULT_BIND: &str = "127.0.0.1";
/// Default detach key, matching `hyperpanes attach` (`Ctrl-\` then `d`).
pub const DEFAULT_DETACH_KEY: &str = "C-\\";

/// Contents of `ssh-settings.json`.
///
/// camelCase on the wire to match `control-settings.json`, the file this one sits beside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SshSettings {
    /// Master switch. The daemon only starts a listener when this is true.
    pub enabled: bool,
    /// Address to bind. Non-loopback requires `allow_remote` as well.
    pub bind: String,
    /// TCP port.
    pub port: u16,
    /// Second, independent consent for a non-loopback `bind`. See the module docs.
    pub allow_remote: bool,
    /// `true` → `ResizePolicy::Request`: an SSH client's window size reflows the pane for
    /// *every* viewer, desktop included.
    pub allow_resize: bool,
    /// Detach key spec, parsed by `attach::parse_detach_key`.
    pub detach_key: String,
}

impl Default for SshSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: DEFAULT_BIND.to_string(),
            port: DEFAULT_PORT,
            allow_remote: false,
            allow_resize: false,
            detach_key: DEFAULT_DETACH_KEY.to_string(),
        }
    }
}

impl SshSettings {
    /// Read `path`, falling back to [`Default`] when it does not exist. A *malformed* file is
    /// an error, not a silent default: silently falling back would quietly change the bind
    /// address, and an operator who typo'd their config deserves to be told.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("{}: malformed ssh settings: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Write `path` atomically. Contains no secrets — the host key lives in its own file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut json = serde_json::to_vec_pretty(self)
            .map_err(|e| format!("could not serialize ssh settings: {e}"))?;
        json.push(b'\n');
        hyperpanes_core::persistence::paths::write_atomic(path, &json)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The socket address to listen on, or an explanation of why this configuration is
    /// refused. This is the *only* place a bind address is derived; there is no code path
    /// that reaches a listener without passing through it.
    pub fn resolve_bind(&self) -> Result<SocketAddr, String> {
        let ip: IpAddr = self.bind.trim().parse().map_err(|_| {
            format!(
                "ssh bind {:?} is not an IP address (hyperpanes never resolves a hostname for a \
                 listen address — write 127.0.0.1, ::1, 0.0.0.0 or a literal interface address)",
                self.bind
            )
        })?;
        if !ip.is_loopback() && !self.allow_remote {
            return Err(format!(
                "refusing to bind the SSH server to {ip}: that is not loopback, and \
                 \"allowRemote\" is false.\n\
                 Every terminal on this machine is reachable through this port. If you really \
                 want it off-box, set BOTH \"bind\" and \"allowRemote\": true in \
                 ssh-settings.json — and prefer a Tailscale address over 0.0.0.0."
            ));
        }
        Ok(SocketAddr::new(ip, self.port))
    }

    /// Whether [`resolve_bind`](Self::resolve_bind) would hand out a non-loopback address —
    /// used only to shout about it in `status` output and the startup log.
    pub fn is_remote_exposed(&self) -> bool {
        self.bind
            .trim()
            .parse::<IpAddr>()
            .map(|ip| !ip.is_loopback())
            .unwrap_or(false)
    }
}

/// Where the SSH server's files live.
///
/// Constructed from the process's real data dirs by [`from_env`](Self::from_env); tests build
/// one [`under`](Self::under) a temp dir so nothing in the suite can touch a developer's real
/// host key.
#[derive(Debug, Clone)]
pub struct SshPaths {
    /// `ssh-settings.json` — user-edited config.
    pub settings: PathBuf,
    /// OpenSSH-format ed25519 private host key. Mode 0600, never logged.
    pub host_key: PathBuf,
    /// `authorized_keys`-format list of client keys allowed to attach.
    pub authorized_keys: PathBuf,
    /// `device-tokens.json` — the paired-device table `hyperpanes pair` writes. Devices that
    /// paired with `--ssh-key` carry a public key here, and the SSH server treats those as a
    /// second source of authorized keys so that one pairing and one `hyperpanes revoke <label>`
    /// cover both the mobile bearer token and the phone's SSH key.
    pub device_tokens: PathBuf,
}

impl SshPaths {
    /// The real locations: settings + authorized keys in the config dir (user-edited),
    /// host key in the state dir (machine-generated runtime state, like `control.json`).
    pub fn from_env() -> Self {
        use hyperpanes_core::persistence::paths;
        let cfg = paths::config_dir();
        let state = paths::state_dir().join("ssh");
        Self {
            settings: cfg.join("ssh-settings.json"),
            host_key: state.join("host_ed25519_key"),
            authorized_keys: cfg.join("ssh-authorized-keys"),
            device_tokens: paths::device_tokens_json(),
        }
    }

    /// The same layout rooted at `dir` — for tests only, so nothing in the suite can reach a
    /// developer's real host key or paired devices.
    #[cfg(test)]
    pub fn under(dir: &Path) -> Self {
        Self {
            settings: dir.join("ssh-settings.json"),
            host_key: dir.join("ssh").join("host_ed25519_key"),
            authorized_keys: dir.join("ssh-authorized-keys"),
            device_tokens: dir.join("device-tokens.json"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_loopback() {
        let s = SshSettings::default();
        assert!(!s.enabled, "the SSH server must be opt-in");
        assert!(!s.allow_remote);
        assert!(
            !s.allow_resize,
            "a phone must not reflow the desktop by default"
        );
        assert_eq!(s.resolve_bind().unwrap(), "127.0.0.1:2222".parse().unwrap());
    }

    #[test]
    fn non_loopback_bind_is_refused_without_allow_remote() {
        for addr in ["0.0.0.0", "::", "192.168.1.10", "100.64.0.1"] {
            let s = SshSettings {
                bind: addr.to_string(),
                ..Default::default()
            };
            let err = s
                .resolve_bind()
                .expect_err("non-loopback must be refused without allowRemote");
            assert!(err.contains("allowRemote"), "unhelpful error: {err}");
        }
    }

    #[test]
    fn non_loopback_bind_is_allowed_once_opted_in() {
        let s = SshSettings {
            bind: "100.64.0.1".to_string(),
            allow_remote: true,
            port: 2200,
            ..Default::default()
        };
        assert_eq!(
            s.resolve_bind().unwrap(),
            "100.64.0.1:2200".parse().unwrap()
        );
        assert!(s.is_remote_exposed());
    }

    #[test]
    fn ipv6_loopback_needs_no_opt_in() {
        let s = SshSettings {
            bind: "::1".to_string(),
            ..Default::default()
        };
        assert_eq!(s.resolve_bind().unwrap(), "[::1]:2222".parse().unwrap());
        assert!(!s.is_remote_exposed());
    }

    #[test]
    fn hostnames_are_not_resolved() {
        let s = SshSettings {
            bind: "localhost".to_string(),
            ..Default::default()
        };
        // A hostname could resolve to anything, including a public address. Refuse it rather
        // than let DNS pick the exposure level.
        assert!(s.resolve_bind().is_err());
    }

    #[test]
    fn round_trips_through_json() {
        let s = SshSettings {
            enabled: true,
            allow_resize: true,
            port: 2300,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            json.contains("\"allowResize\""),
            "camelCase on the wire: {json}"
        );
        assert_eq!(serde_json::from_str::<SshSettings>(&json).unwrap(), s);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let s: SshSettings = serde_json::from_str("{\"enabled\":true}").unwrap();
        assert!(s.enabled);
        assert_eq!(s.bind, DEFAULT_BIND);
        assert_eq!(s.port, DEFAULT_PORT);
    }

    #[test]
    fn a_malformed_file_is_an_error_not_a_silent_default() {
        let dir = super::super::testutil::tmpdir("cfg");
        let p = dir.path().join("ssh-settings.json");
        std::fs::write(&p, b"{ not json").unwrap();
        assert!(SshSettings::load(&p).is_err());
        std::fs::remove_file(&p).unwrap();
        // Absent is fine, though.
        assert_eq!(SshSettings::load(&p).unwrap(), SshSettings::default());
    }
}
