//! Host key and `authorized_keys` handling for the embedded SSH server (mux backend M3).
//!
//! Unix only — see [`super`] for why the whole server is gated.
//!
//! # Rules this module exists to enforce
//!
//! * The host key is generated **once** and persisted. A server that regenerates its identity
//!   on every start trains its users to type "yes" at a changed-host-key warning, which is
//!   the exact prompt that is supposed to stop a man in the middle.
//! * The private key file is created with `O_CREAT|O_EXCL` **and** mode `0600` in the same
//!   `open(2)` — never written world-readable and chmod'ed afterwards, and never able to
//!   clobber an existing key (which is what `ssh_key`'s own `write_openssh_file` would do:
//!   it opens with `create(true).truncate(true)`).
//! * A key file with any group/other permission bit is **refused**, the way OpenSSH refuses
//!   one. Same for a group/other-*writable* `authorized_keys`: anything that can write that
//!   file owns every terminal on the machine.
//! * Nothing in here ever logs, prints, or formats private key material. `status` prints a
//!   SHA-256 fingerprint of the *public* half and nothing else.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use ssh_key::private::{Ed25519Keypair, KeypairData};
use ssh_key::{Algorithm, AuthorizedKeys, HashAlg, LineEnding, PrivateKey, PublicKey};

/// Bits that must be clear on a private key file: any access at all for group or other.
const PRIVATE_MODE_MASK: u32 = 0o077;
/// Bits that must be clear on `authorized_keys`: group/other *write*.
const AUTHORIZED_MODE_MASK: u32 = 0o022;

/// Load the persisted ed25519 host key, generating and persisting one on first run.
///
/// Returns the key plus `true` when it was freshly generated (so the caller can tell the
/// operator to expect a first-connection fingerprint prompt).
pub fn load_or_create_host_key(path: &Path) -> Result<(PrivateKey, bool), String> {
    // Two attempts: if a concurrent process wins the `create_new` race we fall back to
    // reading what it wrote rather than failing the start-up.
    for attempt in 0..2 {
        match std::fs::metadata(path) {
            Ok(meta) => {
                check_mode(path, meta.permissions().mode(), PRIVATE_MODE_MASK, "private key")?;
                let key = PrivateKey::read_openssh_file(path)
                    .map_err(|e| format!("{}: unreadable SSH host key: {e}", path.display()))?;
                if key.is_encrypted() {
                    return Err(format!(
                        "{}: the SSH host key is passphrase-encrypted. hyperpanes has no way to \
                         prompt for it from a background daemon; move the file aside and let it \
                         generate a fresh one, or decrypt it with `ssh-keygen -p`.",
                        path.display()
                    ));
                }
                return Ok((key, false));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
        match generate_host_key(path) {
            Ok(key) => return Ok((key, true)),
            // Lost the race with another hyperpanes process — go round and read theirs.
            Err(GenerateError::Exists) if attempt == 0 => continue,
            Err(GenerateError::Exists) => {
                return Err(format!("{}: host key appeared and vanished", path.display()))
            }
            Err(GenerateError::Other(m)) => return Err(m),
        }
    }
    Err(format!("{}: could not obtain a host key", path.display()))
}

enum GenerateError {
    /// The file already exists — somebody else got there first.
    Exists,
    Other(String),
}

/// Create a new ed25519 host key at `path` with mode 0600, failing if it already exists.
fn generate_host_key(path: &Path) -> Result<PrivateKey, GenerateError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)
        .map_err(|e| GenerateError::Other(format!("{}: {e}", dir.display())))?;
    // Best effort: the key file's own 0600 is what actually protects it, but a 0700 dir stops
    // a reader from even learning the key exists.
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));

    // Seed straight from the OS CSPRNG (`getrandom`), with a real error path — no
    // userspace PRNG sits between the kernel and the host key.
    let mut seed = [0u8; 32];
    {
        use rand::rngs::SysRng;
        use rand::TryRng;
        SysRng.try_fill_bytes(&mut seed).map_err(|e| {
            GenerateError::Other(format!("could not read 32 bytes of OS entropy: {e}"))
        })?;
    }
    let keypair = Ed25519Keypair::from_seed(&seed);
    // Best-effort scrub of the copy we control; the authoritative copy now lives inside
    // `PrivateKey`, which zeroizes itself on drop.
    seed.fill(0);

    let key = PrivateKey::new(KeypairData::Ed25519(keypair), "hyperpanes")
        .map_err(|e| GenerateError::Other(format!("could not build a host key: {e}")))?;
    debug_assert_eq!(key.algorithm(), Algorithm::Ed25519);

    // `create_new` + `mode` in one open: never clobbers, never exists world-readable even
    // for an instant. This is why `PrivateKey::write_openssh_file` is not used here.
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(GenerateError::Exists)
        }
        Err(e) => return Err(GenerateError::Other(format!("{}: {e}", path.display()))),
    };
    key.write_openssh(&mut file, LineEnding::LF)
        .map_err(|e| GenerateError::Other(format!("{}: {e}", path.display())))?;
    file.sync_all()
        .map_err(|e| GenerateError::Other(format!("{}: {e}", path.display())))?;

    // The public half, for `ssh-keyscan`-free fingerprint checking. Public — 0644 is fine.
    let pub_path = path.with_extension("pub");
    let pub_text = key
        .public_key()
        .to_openssh()
        .map_err(|e| GenerateError::Other(format!("{}: {e}", pub_path.display())))?;
    let _ = std::fs::write(&pub_path, format!("{pub_text}\n"));

    Ok(key)
}

/// Refuse a key file whose mode lets anyone but the owner at it — the same check OpenSSH
/// makes, and for the same reason.
fn check_mode(path: &Path, mode: u32, mask: u32, what: &str) -> Result<(), String> {
    if mode & mask != 0 {
        return Err(format!(
            "{}: refusing to use a {what} with permissions {:04o} — it is accessible to users \
             other than the owner. Fix it with `chmod {} {}`.",
            path.display(),
            mode & 0o7777,
            if mask == PRIVATE_MODE_MASK { "600" } else { "644" },
            path.display()
        ));
    }
    Ok(())
}

/// `SHA256:…` fingerprint of a public key — the only thing about a key that is ever printed.
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// Fingerprint of a private key's public half.
pub fn host_fingerprint(key: &PrivateKey) -> String {
    fingerprint(key.public_key())
}

/// A human label for a key: its `authorized_keys` comment when it has one, else its
/// fingerprint. Used in logs and in `hyperpanes ssh keys`.
pub fn label_for(key: &PublicKey) -> String {
    let comment = key.comment().as_str_lossy().trim().to_string();
    if comment.is_empty() {
        fingerprint(key)
    } else {
        comment
    }
}

/// The parsed contents of `authorized_keys`.
#[derive(Debug, Default, Clone)]
pub struct AuthorizedKeySet {
    /// Keys that parsed.
    pub keys: Vec<PublicKey>,
    /// Lines that did not, with their line numbers. Surfaced rather than swallowed: a user who
    /// pasted a broken key must not believe it is installed.
    pub warnings: Vec<String>,
}

impl AuthorizedKeySet {
    /// Whether `offered` is in the set, and under what label.
    ///
    /// Compares [`PublicKey::key_data`] — the actual key material — so a client that reordered
    /// or dropped the trailing comment still matches, and a comment alone can never authorize
    /// anything.
    pub fn authorize(&self, offered: &PublicKey) -> Option<String> {
        self.keys
            .iter()
            .find(|k| k.key_data() == offered.key_data())
            .map(label_for)
    }

    /// How many keys are installed.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether nobody can log in.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Read `authorized_keys`. A missing file is an **empty** set, not an error: that is the
/// fail-closed state (nobody can authenticate) and it is what a fresh install looks like.
pub fn load_authorized(path: &Path) -> Result<AuthorizedKeySet, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AuthorizedKeySet::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mode = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .permissions()
        .mode();
    check_mode(path, mode, AUTHORIZED_MODE_MASK, "authorized-keys file")?;
    Ok(parse_authorized(&text))
}

/// Parse `authorized_keys` text. Split out from [`load_authorized`] so it is testable without
/// a filesystem.
pub fn parse_authorized(text: &str) -> AuthorizedKeySet {
    let mut set = AuthorizedKeySet::default();
    // `AuthorizedKeys` skips blanks and `#` comments itself, but it stops at the first bad
    // entry — so line-by-line, to report the bad ones and keep the good ones.
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match AuthorizedKeys::new(trimmed).next() {
            Some(Ok(entry)) => set.keys.push(entry.public_key().clone()),
            Some(Err(e)) => set
                .warnings
                .push(format!("line {}: ignored, not a valid public key ({e})", i + 1)),
            None => set
                .warnings
                .push(format!("line {}: ignored, not a valid public key", i + 1)),
        }
    }
    set
}

/// Append `key` to `authorized_keys`, creating the file 0600 if it does not exist.
///
/// Returns `false` when the key was already present (idempotent, so re-pairing a phone is
/// harmless). `label`, when given, replaces the key's comment.
pub fn authorize_key(path: &Path, key_text: &str, label: Option<&str>) -> Result<bool, String> {
    let mut key = parse_public_key(key_text)?;
    if let Some(label) = label {
        key.set_comment(label);
    }
    let existing = load_authorized(path)?;
    if existing.authorize(&key).is_some() {
        return Ok(false);
    }
    let line = key
        .to_openssh()
        .map_err(|e| format!("could not re-encode the key: {e}"))?;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writeln!(f, "{line}").map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(true)
}

/// Drop every key whose label or fingerprint matches `needle`. Returns how many went.
pub fn revoke_key(path: &Path, needle: &str) -> Result<usize, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut kept = String::new();
    let mut removed = 0usize;
    for line in text.lines() {
        let trimmed = line.trim();
        let matches = if trimmed.is_empty() || trimmed.starts_with('#') {
            false
        } else {
            match AuthorizedKeys::new(trimmed).next() {
                Some(Ok(entry)) => {
                    let key = entry.public_key();
                    label_for(key) == needle || fingerprint(key) == needle
                }
                _ => false,
            }
        };
        if matches {
            removed += 1;
        } else {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    if removed > 0 {
        // Not `write_atomic`: its temp file is created with the process umask, which would
        // briefly expose the list. Rewrite in place through a 0600 handle instead.
        let mut f = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        f.write_all(kept.as_bytes())
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(removed)
}

/// Accept either an `authorized_keys` line, a whole `id_ed25519.pub` file's contents, or a
/// path to one.
pub fn parse_public_key(input: &str) -> Result<PublicKey, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("no public key given".to_string());
    }
    if let Ok(k) = PublicKey::from_openssh(trimmed) {
        return Ok(k);
    }
    let as_path = Path::new(trimmed);
    if as_path.is_file() {
        let text = std::fs::read_to_string(as_path)
            .map_err(|e| format!("{}: {e}", as_path.display()))?;
        return PublicKey::from_openssh(text.trim())
            .map_err(|e| format!("{}: not an OpenSSH public key: {e}", as_path.display()));
    }
    Err(
        "not an OpenSSH public key. Expected something like \
         `ssh-ed25519 AAAAC3Nz... you@phone`, or the path to a `.pub` file."
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::testutil::tmpdir;

    fn a_key() -> PrivateKey {
        let mut seed = [7u8; 32];
        seed[0] = 1;
        PrivateKey::new(
            KeypairData::Ed25519(Ed25519Keypair::from_seed(&seed)),
            "test",
        )
        .unwrap()
    }

    fn another_key() -> PrivateKey {
        PrivateKey::new(
            KeypairData::Ed25519(Ed25519Keypair::from_seed(&[9u8; 32])),
            "other",
        )
        .unwrap()
    }

    // ---- host key ----

    #[test]
    fn host_key_is_generated_once_and_reused() {
        let dir = tmpdir("hostkey");
        let path = dir.path().join("ssh").join("host_ed25519_key");
        let (first, fresh) = load_or_create_host_key(&path).unwrap();
        assert!(fresh, "first call must generate");
        let (second, fresh2) = load_or_create_host_key(&path).unwrap();
        assert!(!fresh2, "second call must reuse — a changing host key trains users to click through MITM warnings");
        assert_eq!(host_fingerprint(&first), host_fingerprint(&second));
        assert_eq!(first.algorithm(), Algorithm::Ed25519);
    }

    #[test]
    fn host_key_file_is_0600_and_the_public_half_is_written() {
        let dir = tmpdir("hostperm");
        let path = dir.path().join("ssh").join("host_ed25519_key");
        let (key, _) = load_or_create_host_key(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "host key must be owner-only, got {mode:04o}");
        let pub_text = std::fs::read_to_string(path.with_extension("pub")).unwrap();
        assert!(pub_text.starts_with("ssh-ed25519 "), "{pub_text}");
        assert_eq!(
            PublicKey::from_openssh(pub_text.trim()).unwrap().key_data(),
            key.public_key().key_data()
        );
    }

    #[test]
    fn two_host_keys_are_not_the_same_key() {
        // Guards against a constant/zero seed sneaking in.
        let a = tmpdir("rand-a");
        let b = tmpdir("rand-b");
        let (ka, _) = load_or_create_host_key(&a.path().join("k")).unwrap();
        let (kb, _) = load_or_create_host_key(&b.path().join("k")).unwrap();
        assert_ne!(host_fingerprint(&ka), host_fingerprint(&kb));
    }

    #[test]
    fn a_group_readable_host_key_is_refused() {
        let dir = tmpdir("hostloose");
        let path = dir.path().join("ssh").join("host_ed25519_key");
        load_or_create_host_key(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = load_or_create_host_key(&path).expect_err("0644 host key must be refused");
        assert!(err.contains("chmod 600"), "{err}");
    }

    #[test]
    fn generating_never_clobbers_an_existing_file() {
        let dir = tmpdir("noclobber");
        let path = dir.path().join("ssh").join("host_ed25519_key");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"i am not a key").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_or_create_host_key(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"i am not a key");
    }

    // ---- authorized keys ----

    #[test]
    fn an_absent_authorized_keys_file_authorizes_nobody() {
        let dir = tmpdir("noauth");
        let set = load_authorized(&dir.path().join("ssh-authorized-keys")).unwrap();
        assert!(set.is_empty());
        assert!(set.authorize(a_key().public_key()).is_none());
    }

    #[test]
    fn an_authorized_key_matches_and_an_unlisted_one_does_not() {
        let dir = tmpdir("auth");
        let path = dir.path().join("ssh-authorized-keys");
        let allowed = a_key();
        assert!(authorize_key(
            &path,
            &allowed.public_key().to_openssh().unwrap(),
            Some("phone")
        )
        .unwrap());
        let set = load_authorized(&path).unwrap();
        assert_eq!(set.authorize(allowed.public_key()).as_deref(), Some("phone"));
        assert!(set.authorize(another_key().public_key()).is_none());
    }

    #[test]
    fn authorizing_the_same_key_twice_is_a_no_op() {
        let dir = tmpdir("dedup");
        let path = dir.path().join("ssh-authorized-keys");
        let text = a_key().public_key().to_openssh().unwrap();
        assert!(authorize_key(&path, &text, Some("phone")).unwrap());
        assert!(!authorize_key(&path, &text, Some("phone")).unwrap());
        assert_eq!(load_authorized(&path).unwrap().len(), 1);
    }

    #[test]
    fn authorized_keys_is_created_0600() {
        let dir = tmpdir("authperm");
        let path = dir.path().join("ssh-authorized-keys");
        authorize_key(&path, &a_key().public_key().to_openssh().unwrap(), None).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:04o}");
    }

    #[test]
    fn a_world_writable_authorized_keys_is_refused() {
        let dir = tmpdir("authloose");
        let path = dir.path().join("ssh-authorized-keys");
        authorize_key(&path, &a_key().public_key().to_openssh().unwrap(), None).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let err = load_authorized(&path).expect_err("world-writable must be refused");
        assert!(err.contains("chmod"), "{err}");
    }

    #[test]
    fn revoking_removes_exactly_one_key() {
        let dir = tmpdir("revoke");
        let path = dir.path().join("ssh-authorized-keys");
        let keep = a_key();
        let drop = another_key();
        authorize_key(&path, &keep.public_key().to_openssh().unwrap(), Some("keep")).unwrap();
        authorize_key(&path, &drop.public_key().to_openssh().unwrap(), Some("drop")).unwrap();
        assert_eq!(revoke_key(&path, "drop").unwrap(), 1);
        let set = load_authorized(&path).unwrap();
        assert_eq!(set.len(), 1);
        assert!(set.authorize(keep.public_key()).is_some());
        assert!(set.authorize(drop.public_key()).is_none());
        assert_eq!(revoke_key(&path, "nobody").unwrap(), 0);
    }

    #[test]
    fn revoking_by_fingerprint_works() {
        let dir = tmpdir("revokefp");
        let path = dir.path().join("ssh-authorized-keys");
        let k = a_key();
        authorize_key(&path, &k.public_key().to_openssh().unwrap(), Some("phone")).unwrap();
        let fp = fingerprint(k.public_key());
        assert_eq!(revoke_key(&path, &fp).unwrap(), 1);
        assert!(load_authorized(&path).unwrap().is_empty());
    }

    #[test]
    fn a_broken_line_is_reported_and_the_good_ones_survive() {
        let good = a_key().public_key().to_openssh().unwrap();
        let text = format!("# a comment\n\nssh-ed25519 NOT-BASE64 broken\n{good}\n");
        let set = parse_authorized(&text);
        assert_eq!(set.len(), 1);
        assert_eq!(set.warnings.len(), 1, "{:?}", set.warnings);
        assert!(set.warnings[0].contains("line 3"), "{:?}", set.warnings);
    }

    #[test]
    fn a_comment_alone_cannot_authorize() {
        // Same label, different key material: must not match.
        let dir = tmpdir("comment");
        let path = dir.path().join("ssh-authorized-keys");
        authorize_key(
            &path,
            &a_key().public_key().to_openssh().unwrap(),
            Some("phone"),
        )
        .unwrap();
        let mut impostor = another_key().public_key().clone();
        impostor.set_comment("phone");
        assert!(load_authorized(&path).unwrap().authorize(&impostor).is_none());
    }

    #[test]
    fn parse_public_key_accepts_a_file_path() {
        let dir = tmpdir("keyfile");
        let p = dir.path().join("id_ed25519.pub");
        let text = a_key().public_key().to_openssh().unwrap();
        std::fs::write(&p, format!("{text}\n")).unwrap();
        let parsed = parse_public_key(p.to_str().unwrap()).unwrap();
        assert_eq!(parsed.key_data(), a_key().public_key().key_data());
        assert!(parse_public_key("hello").is_err());
    }
}
