//! The **cross-process claim registry** (mux plan M7): who is hosting which session uid,
//! right now, across every hyperpanes process on the machine.
//!
//! ## Why the daemon is the registry, and not a file
//!
//! M5's left panel lists "detached" sessions — live sessions no window is showing — and
//! offers one-click adoption. To do that safely it must answer two questions that a single
//! process cannot answer for itself:
//!
//! 1. *Is this session already on someone's screen?* (else a second window lists a pane
//!    that is visibly running next door and offers to "adopt" it)
//! 2. *Did I win the race to adopt it?* (else two windows both adopt one orphan)
//!
//! Two shapes were considered:
//!
//! * **A file-based registry** — `claims.json`/`claims.d/` in the runtime dir, one entry per
//!   uid naming the owning pid. Every reader must then decide whether an entry is *stale*,
//!   and the only honest way to do that is pid **plus** process start time (a bare pid is
//!   recycled), which is per-OS code. Mutual exclusion needs a lock protocol layered on top
//!   (`flock` a per-uid file, or one global lock everybody serializes through). Crash safety
//!   is the reader's job, forever, on every read.
//! * **The daemon as the single arbiter** — chosen. The daemon *already* is the one process
//!   that knows every session and that every hyperpanes process is connected to; the claim
//!   map is ordinary in-memory state behind a `Mutex`, so a claim is a genuine atomic
//!   compare-and-set with exactly one winner, no protocol.
//!
//! ## Liveness the OS vouches for
//!
//! **A claim is scoped to a connection, not to a timestamp.** No heartbeat, no lease, no
//! expiry: the owner of a claim is a [`ConnId`], and the daemon calls [`release_conn`] from
//! the per-connection teardown path. That path runs when the socket reaches EOF — and the
//! kernel closes the socket when the owning *process* dies, however it dies (clean exit,
//! panic, `SIGKILL`, OOM kill). So a crashed process's panes become adoptable within one
//! read of the daemon's connection thread, and there is no way to leave a session
//! permanently claimed short of a live process holding a live socket. Nothing here has to
//! guess whether a pid is still the pid it was.
//!
//! [`release_conn`]: ClaimRegistry::release_conn
//!
//! ## What this module is *not*
//!
//! It is not a lock on the session. The daemon multiplexes output, so two connections may
//! legitimately display one session; the claim only records **who is responsible for it** so
//! the panel can stop offering an owned session for adoption. A [`ClaimRegistry::claim`]
//! that loses simply reports the incumbent — the caller decides (the GUI declines to adopt).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// A daemon-assigned identity for one client connection. Minted by
/// [`ClaimRegistry::next_conn_id`] and told to the client in the `Hello` reply, so a client
/// can tell *its own* claims apart from everybody else's without the daemon having to trust
/// (or peer-credential) a pid the client asserts.
///
/// `0` is never minted, so it is a usable "unknown / not a claim owner" sentinel for a peer
/// that predates M7 and left the `Hello` field at its serde default.
pub type ConnId = u64;

/// One entry of the registry as it rides the wire: a claimed uid and the connection that
/// holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimInfo {
    pub uid: String,
    /// The connection holding the claim. A client compares this against the `conn_id` it was
    /// given in `Hello` to split "mine" from "somebody else's".
    pub owner: ConnId,
}

/// The outcome of a [`ClaimRegistry::claim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The caller now owns the uid (it was free, or the caller already owned it).
    Granted,
    /// Another connection owns it; this is who.
    Denied(ConnId),
}

impl ClaimOutcome {
    /// Whether the claim was granted.
    #[tracing::instrument(level = "debug", ret)]
    pub fn granted(self) -> bool {
        matches!(self, ClaimOutcome::Granted)
    }

    /// The incumbent owner when denied.
    #[tracing::instrument(level = "debug", ret)]
    pub fn owner(self) -> Option<ConnId> {
        match self {
            ClaimOutcome::Granted => None,
            ClaimOutcome::Denied(c) => Some(c),
        }
    }
}

/// The daemon-side claim table: `uid -> owning connection`, plus the connection-id source.
///
/// Every mutator takes `&self` (interior mutability) so the daemon can share one
/// `Arc<ClaimRegistry>` across its connection threads/tasks. All of them are `O(claims)` at
/// worst over a map that holds one entry per *visible pane on the machine* — tens, not
/// thousands — so the single mutex is never a contention point.
#[derive(Debug, Default)]
pub struct ClaimRegistry {
    claims: Mutex<HashMap<String, ConnId>>,
    next_conn: AtomicU64,
}

impl ClaimRegistry {
    /// An empty registry whose first minted connection id is `1`.
    #[tracing::instrument(level = "debug", ret)]
    pub fn new() -> Self {
        ClaimRegistry {
            claims: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
        }
    }

    /// Mint the next connection id. Monotonic and never `0` (see [`ConnId`]), so an id is
    /// never reused within a daemon's lifetime and a released claim can never be confused
    /// with a later connection's.
    #[tracing::instrument(level = "debug", ret)]
    pub fn next_conn_id(&self) -> ConnId {
        self.next_conn.fetch_add(1, Ordering::SeqCst)
    }

    /// **The atomic compare-and-set.** Take ownership of `uid` for `conn` if it is free (or
    /// already `conn`'s); otherwise report the incumbent and change nothing.
    ///
    /// This is the whole no-double-adoption guarantee: two connections racing to claim one
    /// orphan both land in this function, the mutex orders them, and the second sees an
    /// occupied entry. There is exactly one [`ClaimOutcome::Granted`] per uid until it is
    /// released.
    #[tracing::instrument(level = "debug", ret)]
    pub fn claim(&self, uid: &str, conn: ConnId) -> ClaimOutcome {
        let mut map = self.claims.lock().unwrap();
        match map.get(uid) {
            // Re-claiming what you already hold is a no-op success, so a client that
            // re-publishes its claim set every frame is idempotent and cheap.
            Some(&owner) if owner == conn => ClaimOutcome::Granted,
            Some(&owner) => ClaimOutcome::Denied(owner),
            None => {
                map.insert(uid.to_string(), conn);
                ClaimOutcome::Granted
            }
        }
    }

    /// Give up `conn`'s claim on `uid`. A release from a connection that does **not** own it
    /// is ignored — one process can never knock another's claim loose. Returns whether
    /// anything changed.
    #[tracing::instrument(level = "debug", ret)]
    pub fn release(&self, uid: &str, conn: ConnId) -> bool {
        let mut map = self.claims.lock().unwrap();
        if map.get(uid) == Some(&conn) {
            map.remove(uid);
            true
        } else {
            false
        }
    }

    /// Drop **every** claim held by `conn` — the crash-safety path, called from the daemon's
    /// per-connection teardown (which runs on socket EOF, i.e. whenever the owning process
    /// dies for any reason). Returns whether anything changed.
    #[tracing::instrument(level = "debug", ret)]
    pub fn release_conn(&self, conn: ConnId) -> bool {
        let mut map = self.claims.lock().unwrap();
        let before = map.len();
        map.retain(|_, &mut owner| owner != conn);
        map.len() != before
    }

    /// Drop any claim on `uid` regardless of owner — for a session that no longer exists
    /// (natural exit, `Kill`). Keeps the table from pinning uids that can never be adopted.
    /// Returns whether anything changed.
    #[tracing::instrument(level = "debug", ret)]
    pub fn forget_uid(&self, uid: &str) -> bool {
        self.claims.lock().unwrap().remove(uid).is_some()
    }

    /// Drop every claim — for `KillAll`, where no session survives to be claimed.
    #[tracing::instrument(level = "debug", ret)]
    pub fn clear(&self) -> bool {
        let mut map = self.claims.lock().unwrap();
        let had = !map.is_empty();
        map.clear();
        had
    }

    /// The whole table as wire records, sorted by uid so a snapshot is byte-stable and a
    /// client (or a test) can compare two of them directly.
    #[tracing::instrument(level = "debug", ret)]
    pub fn snapshot(&self) -> Vec<ClaimInfo> {
        let map = self.claims.lock().unwrap();
        let mut out: Vec<ClaimInfo> = map
            .iter()
            .map(|(uid, &owner)| ClaimInfo {
                uid: uid.clone(),
                owner,
            })
            .collect();
        out.sort_by(|a, b| a.uid.cmp(&b.uid));
        out
    }

    /// The connection holding `uid`, if any.
    #[tracing::instrument(level = "debug", ret)]
    pub fn owner_of(&self, uid: &str) -> Option<ConnId> {
        self.claims.lock().unwrap().get(uid).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conn_ids_are_monotonic_and_never_zero() {
        let r = ClaimRegistry::new();
        let a = r.next_conn_id();
        let b = r.next_conn_id();
        assert_ne!(a, 0);
        assert!(b > a);
    }

    #[test]
    fn first_claim_wins_and_the_second_is_told_who_holds_it() {
        let r = ClaimRegistry::new();
        assert_eq!(r.claim("pane-a", 1), ClaimOutcome::Granted);
        assert_eq!(r.claim("pane-a", 2), ClaimOutcome::Denied(1));
        assert_eq!(r.owner_of("pane-a"), Some(1));
    }

    #[test]
    fn reclaiming_your_own_uid_is_idempotent() {
        let r = ClaimRegistry::new();
        assert!(r.claim("pane-a", 7).granted());
        assert!(r.claim("pane-a", 7).granted());
        assert_eq!(r.snapshot().len(), 1);
    }

    #[test]
    fn a_non_owner_cannot_release_someone_elses_claim() {
        let r = ClaimRegistry::new();
        r.claim("pane-a", 1);
        assert!(!r.release("pane-a", 2));
        assert_eq!(r.owner_of("pane-a"), Some(1));
        assert!(r.release("pane-a", 1));
        assert_eq!(r.owner_of("pane-a"), None);
    }

    #[test]
    fn release_conn_drops_exactly_that_connections_claims() {
        let r = ClaimRegistry::new();
        r.claim("pane-a", 1);
        r.claim("pane-b", 1);
        r.claim("pane-c", 2);
        assert!(r.release_conn(1));
        let snap = r.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].uid, "pane-c");
        // A second teardown for the same connection is a no-op, not a double-free.
        assert!(!r.release_conn(1));
    }

    #[test]
    fn a_released_uid_is_immediately_claimable_by_the_next_process() {
        let r = ClaimRegistry::new();
        r.claim("pane-a", 1);
        r.release_conn(1); // the owner "crashed"
        assert_eq!(r.claim("pane-a", 2), ClaimOutcome::Granted);
    }

    #[test]
    fn forget_uid_and_clear_drop_claims_regardless_of_owner() {
        let r = ClaimRegistry::new();
        r.claim("pane-a", 1);
        r.claim("pane-b", 2);
        assert!(r.forget_uid("pane-a"));
        assert!(!r.forget_uid("pane-a"));
        assert!(r.clear());
        assert!(r.snapshot().is_empty());
        assert!(!r.clear());
    }

    #[test]
    fn snapshot_is_sorted_by_uid() {
        let r = ClaimRegistry::new();
        r.claim("pane-c", 1);
        r.claim("pane-a", 2);
        r.claim("pane-b", 3);
        let uids: Vec<String> = r.snapshot().into_iter().map(|c| c.uid).collect();
        assert_eq!(uids, vec!["pane-a", "pane-b", "pane-c"]);
    }

    /// In-process contention is not the real proof (that lives in the multi-process
    /// integration test), but it does pin the mutex's exactly-one-winner property under
    /// genuine parallelism.
    #[test]
    fn many_threads_racing_one_uid_produce_exactly_one_winner() {
        use std::sync::Arc;
        let r = Arc::new(ClaimRegistry::new());
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let r = Arc::clone(&r);
                let b = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    b.wait();
                    r.claim("pane-contended", i + 1).granted()
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .filter(|_| true)
            .map(|h| h.join().unwrap())
            .filter(|g| *g)
            .count();
        assert_eq!(wins, 1, "exactly one connection may own a uid");
    }
}
