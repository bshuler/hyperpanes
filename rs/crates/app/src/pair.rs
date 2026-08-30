//! `hyperpanes pair` — print pairing info for the mobile app (docs/mobile-client-plan.md).
//!
//! Reads the running app's `control.json` (port + master token), mints a per-device token off it
//! via the control API so the master never leaves the machine, figures
//! out which addresses a phone could reach (the configured `bindAddress`, else the
//! machine's default-route + Tailscale IPs via the connected-UDP-socket trick — no
//! packets are sent), and prints `hp://<host>:<port>/?token=<token>` pairing URLs plus a
//! scannable terminal QR code for the best candidate.
//!
//! Remote reachability requires `bindAddress` in `control-settings.json` (the server
//! binds loopback-only by default); when it's missing we still print the URLs but warn
//! that only this machine can connect.

use std::net::UdpSocket;
use std::path::Path;

use hyperpanes_core::persistence::{control_settings, paths};

pub fn wants_pair(argv: &[String]) -> bool {
    argv.get(1).map(|a| a == "pair").unwrap_or(false)
}

/// Parsed `pair` flags: which device label to stamp, and an optional TTL after which the token
/// self-revokes.
struct PairOpts {
    label: String,
    ttl_ms: Option<i64>,
    /// The device's SSH public key in `authorized_keys` line form, when `--ssh-key` was given.
    /// It rides in the same device record as the bearer token, which is what lets the embedded
    /// SSH server (mux backend M3) accept the phone and lets one `hyperpanes revoke <label>`
    /// close both doors under one label and one TTL.
    ssh_key: Option<String>,
}

impl PairOpts {
    /// `hyperpanes pair [--device <label>] [--ttl <30d|12h|90m|<ms>>] [--ssh-key <key|path>]`.
    /// Label defaults to the machine hostname; TTL omitted = never expires (the master-token
    /// guarantee).
    fn parse(argv: &[String]) -> Result<Self, String> {
        let mut label: Option<String> = None;
        let mut ttl_ms: Option<i64> = None;
        let mut ssh_key: Option<String> = None;
        let mut it = argv.iter().skip(2); // argv[0]=exe, argv[1]="pair"
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--device" | "--label" => {
                    label = Some(it.next().ok_or("--device needs a value")?.clone());
                }
                "--ttl" => {
                    let spec = it.next().ok_or("--ttl needs a value")?;
                    ttl_ms = Some(crate::control_cli::parse_ttl_ms(spec)?);
                }
                "--ssh-key" => {
                    let spec = it.next().ok_or("--ssh-key needs a value")?;
                    ssh_key = Some(normalize_ssh_key(spec)?);
                }
                other => return Err(format!("unknown flag '{other}'")),
            }
        }
        let label = label
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(crate::control_cli::default_device_label);
        Ok(Self {
            label,
            ttl_ms,
            ssh_key,
        })
    }
}

/// Validate `--ssh-key` and return the canonical `authorized_keys` line to store.
///
/// The value may be a key line, the contents of a `.pub` file, or a path to one. Parsing here
/// rather than at the server means a typo is a usage error at the terminal instead of a device
/// that silently cannot log in — and it guarantees only real key material is ever persisted.
#[cfg(unix)]
fn normalize_ssh_key(spec: &str) -> Result<String, String> {
    let key = crate::ssh::keys::parse_public_key(spec).map_err(|e| format!("--ssh-key: {e}"))?;
    key.to_openssh()
        .map_err(|e| format!("--ssh-key: could not re-encode the key ({e})"))
}

/// Windows has no embedded SSH server (see `ssh/mod.rs`), so pairing a key would store a
/// credential nothing reads. Say so instead of accepting it.
#[cfg(not(unix))]
fn normalize_ssh_key(_spec: &str) -> Result<String, String> {
    Err("--ssh-key: the embedded SSH server is not available on Windows".to_string())
}

/// Mint a per-device token by POSTing to the running server's `/devices` (master-authenticated),
/// so the master token itself never travels to the phone. Returns the new device token.
fn mint_device(
    port: u16,
    bind_address: Option<&str>,
    master: &str,
    opts: &PairOpts,
) -> Result<String, String> {
    let base = crate::control_cli::base_url(port, bind_address);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let mut body = serde_json::json!({ "label": opts.label });
    if let Some(ttl) = opts.ttl_ms {
        body["ttlMs"] = serde_json::json!(ttl);
    }
    if let Some(key) = &opts.ssh_key {
        body["sshKey"] = serde_json::json!(key);
    }
    let resp = client
        .post(format!("{base}/devices"))
        .bearer_auth(master)
        .json(&body)
        .send()
        .map_err(|e| format!("POST {base}/devices: {e}"))?;
    if !resp.status().is_success() {
        let code = resp.status();
        let msg = resp.text().unwrap_or_default();
        return Err(format!("server returned {code}: {msg}"));
    }
    let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    v.get("token")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| "response missing token".to_string())
}

pub fn run(argv: &[String]) -> std::io::Result<()> {
    let opts = match PairOpts::parse(argv) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("hyperpanes pair: {e}");
            eprintln!(
                "usage: hyperpanes pair [--device <label>] [--ttl <30d|12h|90m|<ms>>] \
                 [--ssh-key <key|path/to/key.pub>]"
            );
            std::process::exit(2);
        }
    };
    // Panes inherit HYPERPANES_CONTROL_FILE set-but-EMPTY from the app; treat empty as
    // unset or `pair` run inside a pane resolves a blank path instead of the state dir.
    let control_file = std::env::var_os("HYPERPANES_CONTROL_FILE")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(paths::control_json);
    let Some((port, token)) = read_discovery(&control_file) else {
        eprintln!(
            "No running control API found ({}).\n\
             Start hyperpanes and enable Preferences → Control API, then re-run `hyperpanes pair`.",
            control_file.display()
        );
        std::process::exit(1);
    };

    let settings = control_settings::load();
    let bound_remote = settings.bind_address.is_some();
    let hosts = candidate_hosts(settings.bind_address.as_deref());

    println!("hyperpanes pairing — control API on port {port}\n");
    if !bound_remote {
        println!(
            "⚠ control server is bound to 127.0.0.1 (loopback only) — a phone CANNOT connect yet.\n\
             Add a bind address to {}:\n\
             {{ \"enabled\": true, \"allowInput\": true, \"bindAddress\": \"<this machine's Tailscale/LAN IP>\", \"port\": {port} }}\n\
             then toggle Preferences → Control API (or restart), and re-run `hyperpanes pair`.\n\
             Prefer a Tailscale IP (100.x.y.z): WireGuard-encrypted, no open LAN ports.\n",
            paths::control_settings_json().display()
        );
    }
    if !settings.allow_input {
        println!("⚠ allowInput is off — the mobile app will be read-only (no typing/keys).\n");
    }

    // Mint a per-device token via the control API so the MASTER token never leaves this machine.
    // The phone carries only this named, revocable credential (`hyperpanes devices` / `revoke`).
    let device_token = match mint_device(port, settings.bind_address.as_deref(), &token, &opts) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("failed to mint device token: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "paired device \"{}\"{}\n",
        opts.label,
        opts.ttl_ms.map(|_| " (expiring)").unwrap_or("")
    );
    if opts.ssh_key.is_some() {
        print_ssh_hint(&hosts, &opts.label);
    }

    let urls: Vec<String> = hosts
        .iter()
        .map(|h| pairing_url(h, port, &device_token))
        .collect();
    for u in &urls {
        println!("  {u}");
    }
    if let Some(best) = urls.first() {
        println!("\nScan with the hyperpanes mobile app:\n");
        match qr_text(best) {
            Some(qr) => println!("{qr}"),
            None => println!("(QR render failed — paste the URL manually)"),
        }
    }
    Ok(())
}

/// After pairing a key, say what it is now good for: the SSH front door, on its own port, which
/// has to be switched on separately. `hyperpanes revoke <label>` still takes both away at once.
fn print_ssh_hint(hosts: &[String], label: &str) {
    let host = hosts.first().map(String::as_str).unwrap_or("127.0.0.1");
    let port =
        crate::ssh::config::SshSettings::load(&crate::ssh::config::SshPaths::from_env().settings)
            .map(|s| s.port)
            .unwrap_or(crate::ssh::config::DEFAULT_PORT);
    println!(
        "this device's SSH key is paired too — any SSH client holding it can attach:\n\
        \n  ssh -p {port} {host}\n\
        \nThe SSH server is separate and off by default: `hyperpanes ssh enable`, and it binds\n\
         127.0.0.1 unless you also set allowRemote. `hyperpanes revoke {label}` drops the key\n\
         and the token together.\n"
    );
}

/// `{ port, token }` from control.json, or `None` when missing/corrupt.
fn read_discovery(path: &Path) -> Option<(u16, String)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let port = v
        .get("port")?
        .as_u64()
        .filter(|&p| (1..=65535).contains(&p))? as u16;
    let token = v.get("token")?.as_str()?.to_string();
    Some((port, token))
}

/// The pairing URL the mobile app parses (keep in sync with `mobile/…/pairing.dart`).
fn pairing_url(host: &str, port: u16, token: &str) -> String {
    let h = if host.contains(':') {
        format!("[{host}]") // IPv6 literal
    } else {
        host.to_string()
    };
    format!("hp://{h}:{port}/?token={token}&v=1")
}

/// Addresses a phone could dial, best first. A configured SPECIFIC bind address wins
/// outright (that's the only address the server listens on); an unspecified bind
/// (`0.0.0.0`) or no config falls back to discovering this machine's Tailscale +
/// default-route IPs.
fn candidate_hosts(bind_address: Option<&str>) -> Vec<String> {
    if let Some(addr) = bind_address {
        if let Ok(ip) = addr.parse::<std::net::IpAddr>() {
            if !ip.is_unspecified() {
                return vec![addr.to_string()];
            }
        }
    }
    let mut out = Vec::new();
    // Tailscale first (encrypted path; 100.100.100.100 is the tailnet's MagicDNS resolver,
    // so the OS routes this via the tailscale interface). connect() sends nothing.
    if let Some(ip) = local_ip_toward("100.100.100.100:53") {
        if ip.starts_with("100.") {
            out.push(ip);
        }
    }
    // Default-route LAN IP.
    if let Some(ip) = local_ip_toward("8.8.8.8:53") {
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    if out.is_empty() {
        out.push("127.0.0.1".to_string());
    }
    out
}

/// The local address the OS would use to reach `target` — connected-UDP trick, no I/O.
fn local_ip_toward(target: &str) -> Option<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect(target).ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}

/// Render `data` as a terminal QR (quiet zone + half-block cells, dark-on-light).
fn qr_text(data: &str) -> Option<String> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    Some(
        code.render::<qrcode::render::unicode::Dense1x2>()
            .dark_color(qrcode::render::unicode::Dense1x2::Dark)
            .light_color(qrcode::render::unicode::Dense1x2::Light)
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_url_shape() {
        assert_eq!(
            pairing_url("100.71.2.9", 51888, "tok123"),
            "hp://100.71.2.9:51888/?token=tok123&v=1"
        );
        // IPv6 hosts get bracketed so port parsing stays unambiguous.
        assert_eq!(
            pairing_url("fd7a::1", 51888, "t"),
            "hp://[fd7a::1]:51888/?token=t&v=1"
        );
    }

    #[test]
    fn discovery_parses_port_and_token() {
        let dir = std::env::temp_dir().join(format!("hp-pair-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("control.json");
        std::fs::write(
            &p,
            br#"{ "port": 51888, "token": "abc", "pid": 1, "version": "x", "events": "ws://..." }"#,
        )
        .unwrap();
        assert_eq!(read_discovery(&p), Some((51888, "abc".to_string())));
        std::fs::write(&p, b"not json").unwrap();
        assert_eq!(read_discovery(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn specific_bind_address_wins() {
        assert_eq!(candidate_hosts(Some("100.71.2.9")), vec!["100.71.2.9"]);
        // Unspecified bind → discovery path (non-empty, never contains 0.0.0.0).
        let hosts = candidate_hosts(Some("0.0.0.0"));
        assert!(!hosts.is_empty());
        assert!(hosts.iter().all(|h| h != "0.0.0.0"));
    }

    #[test]
    fn qr_renders() {
        assert!(qr_text("hp://100.71.2.9:51888/?token=t&v=1").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn ssh_key_is_validated_at_the_terminal_and_stored_canonically() {
        let argv = |extra: &[&str]| {
            let mut v = vec!["hyperpanes".to_string(), "pair".to_string()];
            v.extend(extra.iter().map(|s| s.to_string()));
            v
        };
        // Absent by default — pairing a phone that only speaks the control API stores no key.
        assert_eq!(PairOpts::parse(&argv(&[])).unwrap().ssh_key, None);

        // A real key round-trips to its canonical authorized_keys line...
        let key = ssh_key::PrivateKey::new(
            ssh_key::private::KeypairData::Ed25519(ssh_key::private::Ed25519Keypair::from_seed(
                &[3u8; 32],
            )),
            "phone",
        )
        .unwrap();
        let line = key.public_key().to_openssh().unwrap();
        let opts = PairOpts::parse(&argv(&["--ssh-key", &line])).unwrap();
        assert_eq!(opts.ssh_key.as_deref(), Some(line.as_str()));

        // ...and a typo is a usage error here, not a device that silently cannot log in.
        let err = PairOpts::parse(&argv(&["--ssh-key", "ssh-ed25519 nope"]))
            .err()
            .expect("a malformed key must be refused");
        assert!(err.starts_with("--ssh-key:"), "{err}");
        assert!(PairOpts::parse(&argv(&["--ssh-key"])).is_err());
    }
}
