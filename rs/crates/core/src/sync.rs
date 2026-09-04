//! Locking that survives a panic in another thread.
//!
//! `Mutex::lock()` returns `Err` forever once any thread panicked while holding the guard.
//! That is Rust warning you that the protected value may have been left half-written. The
//! usual answer here — `.lock().unwrap()` — turns that warning into a second panic, and
//! because these mutexes are shared between the GUI thread, the control-plane server and
//! the session daemon's reader/writer threads, the second panic lands somewhere unrelated
//! to the first. One background thread falling over takes the whole app with it, and the
//! backtrace points at the innocent thread.
//!
//! For nearly every lock in this codebase that trade is wrong. The protected values are
//! registries, caches and snapshot models: a torn one shows a stale pane or an empty list,
//! which the next sync tick corrects. A dead GUI is not correctable. So the default is to
//! recover the guard and keep going, and to say so once per site so the underlying panic
//! is still visible in the log.
//!
//! It is not the right default everywhere, and this trait deliberately does not hide the
//! choice: a lock guarding an invariant that genuinely cannot be half-applied should keep
//! `.lock().unwrap()` (or handle the error) with a comment saying why. The point is that
//! recovery becomes the considered case rather than the accidental one.

use std::collections::BTreeSet;
use std::panic::Location;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Call sites that have already reported a poisoned lock.
///
/// Poisoning is sticky: once set, *every* subsequent `lock()` on that mutex returns `Err`.
/// Logging each one would bury the original panic under thousands of identical lines, so
/// each site reports once. Keyed by `file:line` of the caller rather than by mutex address,
/// because an address is reused and means nothing to whoever reads the log.
fn reported() -> &'static Mutex<BTreeSet<String>> {
    static REPORTED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    REPORTED.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Log the first recovery at each call site. Never panics — including on itself.
fn note_poisoned(at: &Location<'_>) {
    let site = format!("{}:{}", at.file(), at.line());
    // This set's own mutex is only ever held for a set insert, so it cannot be poisoned by
    // user code; recover anyway rather than let the reporting path be the thing that dies.
    let mut seen = reported().lock().unwrap_or_else(|e| e.into_inner());
    if seen.insert(site.clone()) {
        tracing::warn!(
            site = %site,
            "lock was poisoned by a panic in another thread; recovering the guard \
             (the value may be stale, and the original panic is logged above)"
        );
    }
}

/// `lock()` that recovers from poisoning instead of panicking a second time.
pub trait LockRecover<T: ?Sized> {
    /// Take the guard, poisoned or not.
    ///
    /// On a poisoned lock this reports once per call site and hands back the value as the
    /// panicking thread left it. Prefer this to `.lock().unwrap()` unless a half-written
    /// value would be worse than a stopped process.
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> LockRecover<T> for Mutex<T> {
    #[track_caller]
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        match self.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                note_poisoned(Location::caller());
                poisoned.into_inner()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{self, AssertUnwindSafe};

    /// Poison `m` the way real code does — a panic with the guard held — without the
    /// panic message spraying the test output.
    fn poison<T>(m: &Mutex<T>) {
        let prev = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = m.lock().unwrap();
            panic!("the thread that was holding the lock fell over");
        }));
        panic::set_hook(prev);
    }

    #[test]
    fn an_unpoisoned_lock_behaves_exactly_like_lock_unwrap() {
        let m = Mutex::new(vec![1, 2, 3]);
        m.lock_recover().push(4);
        assert_eq!(*m.lock_recover(), vec![1, 2, 3, 4]);
        assert!(!m.is_poisoned());
    }

    #[test]
    fn a_poisoned_lock_hands_back_the_value_instead_of_panicking() {
        let m = Mutex::new(String::from("half"));
        poison(&m);
        assert!(m.is_poisoned(), "the fixture must actually poison the mutex");
        assert!(m.lock().is_err(), "and plain lock() must still be an error");

        // The whole point: this line is what `.unwrap()` would have panicked on.
        assert_eq!(*m.lock_recover(), "half");

        // Still usable afterwards -- a recovered lock is not a one-shot escape hatch.
        m.lock_recover().push_str("-written");
        assert_eq!(*m.lock_recover(), "half-written");
    }

    #[test]
    fn a_site_reports_its_poisoning_once_however_often_it_locks() {
        let m = Mutex::new(0_u32);
        poison(&m);

        // One site, locked repeatedly: the first call registers it, the rest stay quiet.
        // (`reported()` is process-wide, so assert on this site's own entry, not on size.)
        let before = reported().lock_recover().len();
        for _ in 0..50 {
            *m.lock_recover() += 1;
        }
        let after = reported().lock_recover().len();
        assert_eq!(after, before + 1, "50 locks at one site must report once");
        assert_eq!(*m.lock_recover(), 50);
    }

    #[test]
    fn two_different_sites_each_get_their_own_report() {
        let m = Mutex::new(());
        poison(&m);
        let before = reported().lock_recover().len();
        drop(m.lock_recover()); // site A
        drop(m.lock_recover()); // site B -- a different line, so a second report
        assert_eq!(reported().lock_recover().len(), before + 2);
    }
}
