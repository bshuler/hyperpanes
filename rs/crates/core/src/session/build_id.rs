//! The binary's **build identity**: a short string naming *this exact build*, carried in the
//! daemon handshake so a client can tell "the daemon is the build I am" apart from "the
//! daemon is some other build that happens to speak the same protocol".
//!
//! ## Why the protocol version was not enough
//! [`PROTO_VER`](crate::session::proto::PROTO_VER) changes only when the wire *shapes*
//! change, which is rare and deliberate. A rebuild of the same commit, a freshly installed
//! release, and the dev binary versus the one in `/Applications` all share it — so the
//! client's stale-daemon check saw a match and simply attached to whichever daemon happened
//! to already be running. Moving the sessions onto a new build meant `--kill-daemon`, and
//! that closes every pty master: every shell gets `SIGHUP` and the user loses their work.
//!
//! The build id closes that gap. A client that finds a daemon reporting a *different* build
//! asks for the same live upgrade the proto-mismatch path already performs — the successor
//! takes the pty masters over `SCM_RIGHTS` and the incumbent exits, so nothing downstream of
//! the ptys notices (see [`handoff`](crate::session::handoff)). Either side can therefore be
//! upgraded — or rolled back — without dropping a single session.
//!
//! ## Exact identity, not an ordering
//! The rule is "be the build the client asked for", not "be at least as new as". Two builds
//! of the same version are indistinguishable by version alone (a dev rebuild is the common
//! case), so there is no ordering to compare; and pinning to *exact* is what makes stepping
//! back to a known-good older build work at all.
//!
//! ## What it is made of
//! The crate version plus a hash of the executable's identity on disk — its path, its byte
//! length and its modification time. That is deliberately cheap: three fields from one
//! `stat`, no digest over a ~100 MB binary on every launch. It is also exactly the
//! granularity the problem needs — a rebuild changes length or mtime, an install changes
//! both, and the dev build and the installed build differ in path even when nothing else
//! does. The cost of the approximation is a spurious re-id (a copy that preserves nothing
//! but the bytes), and a spurious re-id costs one live takeover, which keeps every session.
//!
//! It is computed **once**, on first call, and cached for the life of the process. A daemon
//! must keep answering with the build it is *running*: installing a new bundle replaces the
//! file under a daemon that is still, correctly, the old build, and re-reading the path
//! would make it claim to be the successor it is supposed to be replaced by.

use std::sync::OnceLock;

/// This process's build identity. Stable for the life of the process; see the module docs.
#[tracing::instrument(level = "debug", ret)]
pub fn build_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(compute)
}

/// Whether a build id reported by a peer names a build that is *not* this one.
///
/// An EMPTY id means "unknown" — a daemon built before this field existed (the field is
/// `#[serde(default)]`, so its shorter reply still parses), or one that could not stat its
/// own executable. Unknown never forces anything: the whole point of the additive field is
/// that meeting an older peer stays exactly as safe as it was before it existed.
#[tracing::instrument(level = "debug", ret)]
pub fn differs(peer: &str) -> bool {
    !peer.is_empty() && peer != build_id()
}

#[tracing::instrument(level = "debug", ret)]
fn compute() -> String {
    let ver = env!("CARGO_PKG_VERSION");
    match fingerprint() {
        Some(h) => format!("{ver}+{h:016x}"),
        // No fingerprint is not a failure, just a coarser identity: every build of this
        // version then looks alike, which is precisely the behaviour we had before.
        None => format!("{ver}+unknown"),
    }
}

/// Hash the executable's on-disk identity: path, length, mtime. `None` if the exe cannot be
/// located or stat'd.
#[tracing::instrument(level = "debug", ret)]
fn fingerprint() -> Option<u64> {
    let exe = std::env::current_exe().ok()?;
    let meta = std::fs::metadata(&exe).ok()?;
    let mut h = Fnv::new();
    h.write(exe.to_string_lossy().as_bytes());
    h.write(&meta.len().to_le_bytes());
    // A filesystem with no mtime just contributes nothing — path and length still separate
    // the cases that matter most (dev vs. installed, and a rebuild that changed size).
    if let Ok(d) = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH) {
        h.write(&d.as_secs().to_le_bytes());
        h.write(&d.subsec_nanos().to_le_bytes());
    }
    Some(h.finish())
}

/// FNV-1a, 64-bit. Inline rather than a dependency: this hashes a few dozen bytes once per
/// process and needs no collision resistance — a collision costs a missed upgrade, not a
/// wrong one, and the version prefix already separates releases.
struct Fnv(u64);

impl Fnv {
    #[tracing::instrument(level = "debug")]
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_id_is_stable_within_a_process() {
        // The daemon answers every `Hello` with it, and a client compares each answer
        // against its own: an id that drifted would look like an endless stream of new
        // builds to upgrade to.
        assert_eq!(build_id(), build_id());
    }

    #[test]
    fn the_build_id_carries_the_crate_version() {
        assert!(
            build_id().starts_with(env!("CARGO_PKG_VERSION")),
            "id {} should be prefixed by the version",
            build_id()
        );
    }

    #[test]
    fn an_unknown_peer_build_never_forces_an_upgrade() {
        // The empty string is what a pre-build-id daemon's reply deserializes to. Treating
        // it as a mismatch would take over every older daemon on sight - exactly the
        // "killed my terminals for an upgrade nobody asked for" failure.
        assert!(!differs(""), "an unknown build is not a mismatch");
        assert!(!differs(build_id()), "our own build is not a mismatch");
        assert!(differs("0.0.0+deadbeefdeadbeef"), "another build is");
    }
}
