//! The two app-owned scheduler loops of the Hyperpane tab.
//!
//! * The **status loop** (every [`Settings::status_loop_minutes`], 15 by default) asks the
//!   Hyperpane pane's agent to check on every other pane and recover the stuck ones.
//! * The **restart loop** (every [`Settings::restart_loop_hours`], 24 by default) respawns
//!   every monitored agent pane — each tool pane outside the system tab — back into the same
//!   conversation, so a long-running agent gets a fresh process and a fresh context window.
//!
//! Both loops are *schedules*, not timers: the next firing time of each is written to
//! `loops.json` in the state directory whenever it changes, and read back when Hyperpanes
//! starts. A restart of the app therefore resumes the loops where they were rather than
//! starting a new 15-minute / 24-hour countdown, and a firing that fell into a stretch when
//! the app was not running is caught up (once) after a short grace period at startup — the
//! grace so a relaunch first gets its panes and their tools back before anything is typed
//! into them or restarted again.
//!
//! Each loop also records *when it last actually fired*, in the same file. That is the only
//! way to answer "did the restart loop run last night?" after the fact — a schedule alone
//! says when the next one is due, not whether the previous one happened — and it is what
//! [`Loops::publish`] puts on `GET /loops` and `hyperpanes ctl loops` for a headless check.
//!
//! This module owns the *when*: [`Loops::poll`] is called from the app tick and answers
//! which loops are due. The *what* — prompting the Hyperpane pane, restarting the monitored
//! panes — lives in the app, which has the windows, the session manager and the control
//! plane the work needs. The split keeps the schedule itself testable without a GUI.
//!
//! [`Settings::status_loop_minutes`]: crate::prefs::Settings::status_loop_minutes
//! [`Settings::restart_loop_hours`]: crate::prefs::Settings::restart_loop_hours

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hyperpanes_core::control::readmodel::{LoopInfo, LoopsInfo};
use hyperpanes_core::persistence::paths;
use serde::{Deserialize, Serialize};

/// How long after the app started a loop may fire — its panes need to be restored and
/// their tools back at a prompt first.
pub const STARTUP_GRACE: Duration = Duration::from_secs(60);
/// The schedule is consulted at most this often; the app tick runs far more frequently.
const POLL_EVERY: Duration = Duration::from_secs(1);

/// The prompt the status loop types into the Hyperpane pane. Kept short and imperative: it
/// is a standing order the pane's agent already knows from its own instructions, and it lands
/// every quarter of an hour.
pub const DEFAULT_STATUS_PROMPT: &str = "Status check: review every pane, report each agent's \
state in one line, and unstick or restart any agent that is stalled, waiting on a question \
nobody will answer, or has exited.";

/// Which loop fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopKind {
    /// The Hyperpane status prompt.
    Status,
    /// Restart every monitored agent pane.
    Restart,
}

impl LoopKind {
    #[tracing::instrument(level = "debug", ret)]
    pub fn name(self) -> &'static str {
        match self {
            LoopKind::Status => "status",
            LoopKind::Restart => "restart",
        }
    }
}

/// The persisted schedule: the next firing time of each loop, and when each last actually
/// fired, in seconds since the Unix epoch — the same `Option<u64>` epoch-seconds convention
/// the rest of the app persists timestamps in (`Project::last_opened_at`,
/// `PaneRow::last_output_at`). Seconds, not a `SystemTime` or an `Instant`: only an absolute
/// wall-clock stamp survives both a JSON round trip and the process exiting, and a stamp the
/// clock has since jumped past only ever reads as "longer ago than it was", never as a
/// firing that has to be replayed.
///
/// `None` = never (the loop is off, or has not been scheduled / has not yet fired).
///
/// **Compatibility.** Unknown fields are ignored, and a missing known field defaults to
/// `None`, so a `loops.json` written by an older or newer build still loads and the loops
/// keep their rhythm across a downgrade. A known field of the *wrong type* fails the parse
/// and [`Self::load_from`] falls back to an empty schedule — the same whole-file-reject
/// behaviour `prefs::load` has for `settings.json`, and harmless here because the only cost
/// is that both loops are scheduled afresh from now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopSchedule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_next: Option<u64>,
    /// When the status loop last actually fired. Kept when the loop is switched off — it is
    /// a fact about the past, not a schedule nobody will run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_last: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_next: Option<u64>,
    /// When the restart loop last actually fired. See [`Self::status_last`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_last: Option<u64>,
}

impl LoopSchedule {
    /// Read the schedule from `path`; a missing or unreadable file is an empty schedule
    /// (every loop is scheduled afresh from now) — never a reason not to start the app.
    #[tracing::instrument(level = "debug", ret)]
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "loop schedule unreadable; starting the loops afresh");
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "loop schedule unreadable; starting the loops afresh");
                Self::default()
            }
        }
    }

    /// Write the schedule to `path` atomically. A failure is logged, not fatal: the loops
    /// still run this session and are merely rescheduled from scratch after a restart.
    #[tracing::instrument(level = "debug", ret)]
    pub fn save_to(&self, path: &Path) {
        let json = match serde_json::to_vec_pretty(self) {
            Ok(j) => j,
            Err(e) => {
                tracing::error!(error = %e, "loop schedule did not serialize");
                return;
            }
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = paths::write_atomic(path, &json) {
            tracing::warn!(path = %path.display(), error = %e, "loop schedule not saved");
        }
    }

    #[tracing::instrument(level = "debug", ret)]
    fn next(&self, kind: LoopKind) -> Option<u64> {
        match kind {
            LoopKind::Status => self.status_next,
            LoopKind::Restart => self.restart_next,
        }
    }

    #[tracing::instrument(level = "debug", ret)]
    fn set_next(&mut self, kind: LoopKind, next: Option<u64>) {
        match kind {
            LoopKind::Status => self.status_next = next,
            LoopKind::Restart => self.restart_next = next,
        }
    }

    #[tracing::instrument(level = "debug", ret)]
    fn last(&self, kind: LoopKind) -> Option<u64> {
        match kind {
            LoopKind::Status => self.status_last,
            LoopKind::Restart => self.restart_last,
        }
    }

    #[tracing::instrument(level = "debug", ret)]
    fn set_last(&mut self, kind: LoopKind, last: Option<u64>) {
        match kind {
            LoopKind::Status => self.status_last = last,
            LoopKind::Restart => self.restart_last = last,
        }
    }
}

/// Where the schedule lives: `loops.json` beside the rest of the app's state.
#[tracing::instrument(level = "debug", ret)]
pub fn schedule_file() -> PathBuf {
    paths::state_dir().join("loops.json")
}

/// What one loop should do right now. The pure heart of the scheduler, see [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The loop is off: forget any scheduled time.
    Off,
    /// Not yet; keep `next` as it is.
    Wait,
    /// Nothing was scheduled: schedule the first firing for `next` and do not fire now.
    Schedule { next: u64 },
    /// Fire now, and schedule the following firing for `next`.
    Fire { next: u64 },
}

/// Decide one loop's action at `now` given its `interval` (seconds; `0` = off) and its
/// scheduled `next` firing.
///
/// A due firing advances `next` by whole intervals until it is in the future, so a loop
/// that missed several firings while the app was down fires **once** on catch-up and then
/// falls back into its rhythm — not once per missed interval. A `next` further than one
/// interval ahead (the interval was shortened in Preferences) is pulled in to one interval
/// from now, so a change to the setting takes effect without waiting out the old one.
#[tracing::instrument(level = "debug", ret)]
pub fn decide(now: u64, interval: u64, next: Option<u64>) -> Decision {
    if interval == 0 {
        return Decision::Off;
    }
    match next {
        None => Decision::Schedule {
            next: now + interval,
        },
        Some(n) if n > now + interval => Decision::Schedule {
            next: now + interval,
        },
        Some(n) if n > now => Decision::Wait,
        Some(n) => {
            let missed = (now - n) / interval;
            Decision::Fire {
                next: n + (missed + 1) * interval,
            }
        }
    }
}

/// The scheduler the app owns: the persisted schedule plus the startup clock and the poll
/// throttle. Interior mutability because the app is shared behind an `Rc`.
pub struct Loops {
    path: PathBuf,
    schedule: RefCell<LoopSchedule>,
    started: Instant,
    last_poll: Cell<Option<Instant>>,
}

impl Loops {
    /// Load the schedule from the default state file.
    #[tracing::instrument(level = "debug")]
    pub fn new() -> Self {
        Self::at(schedule_file())
    }

    /// Load the schedule from `path` (tests point this at a scratch file).
    #[tracing::instrument(level = "debug")]
    pub fn at(path: PathBuf) -> Self {
        let schedule = LoopSchedule::load_from(&path);
        tracing::info!(
            status_next = ?schedule.status_next,
            status_last = ?schedule.status_last,
            restart_next = ?schedule.restart_next,
            restart_last = ?schedule.restart_last,
            "loop schedule loaded"
        );
        Loops {
            path,
            schedule: RefCell::new(schedule),
            started: Instant::now(),
            last_poll: Cell::new(None),
        }
    }

    /// A read-only view of the current schedule (tests only; the app reads
    /// the persisted file instead).
    #[cfg(test)]
    pub fn schedule(&self) -> LoopSchedule {
        self.schedule.borrow().clone()
    }

    /// Which loops are due now. `status_secs` / `restart_secs` are the configured intervals
    /// in seconds (`0` = off). Throttled to once a second and silent until the startup grace
    /// has passed; every schedule change is persisted before this returns, so a crash between
    /// the decision and the work does not replay the firing.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn poll(&self, status_secs: u64, restart_secs: u64) -> Vec<LoopKind> {
        let now_i = Instant::now();
        if now_i.duration_since(self.started) < STARTUP_GRACE {
            return Vec::new();
        }
        if self
            .last_poll
            .get()
            .is_some_and(|last| now_i.duration_since(last) < POLL_EVERY)
        {
            return Vec::new();
        }
        self.last_poll.set(Some(now_i));
        self.poll_at(unix_now(), status_secs, restart_secs)
    }

    /// [`Self::poll`] without the throttle or grace, at an explicit clock — the testable core.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn poll_at(&self, now: u64, status_secs: u64, restart_secs: u64) -> Vec<LoopKind> {
        let mut fired = Vec::new();
        let mut changed = false;
        {
            let mut sched = self.schedule.borrow_mut();
            for (kind, interval) in [
                (LoopKind::Status, status_secs),
                (LoopKind::Restart, restart_secs),
            ] {
                let before = sched.next(kind);
                let before_last = sched.last(kind);
                match decide(now, interval, before) {
                    Decision::Off => sched.set_next(kind, None),
                    Decision::Wait => {}
                    Decision::Schedule { next } => {
                        tracing::info!(loop_ = kind.name(), next, interval, "loop scheduled");
                        sched.set_next(kind, Some(next));
                    }
                    Decision::Fire { next } => {
                        tracing::info!(
                            loop_ = kind.name(),
                            due = ?before,
                            late_secs = now.saturating_sub(before.unwrap_or(now)),
                            last = ?before_last,
                            next,
                            "loop firing"
                        );
                        sched.set_next(kind, Some(next));
                        // Recorded here, with the reschedule, so the stamp is persisted in the
                        // same write: a crash between deciding and doing loses the work, not
                        // the record that this slot was consumed.
                        sched.set_last(kind, Some(now));
                        fired.push(kind);
                    }
                }
                changed |= sched.next(kind) != before || sched.last(kind) != before_last;
            }
        }
        if changed {
            self.schedule.borrow().save_to(&self.path);
        }
        fired
    }

    /// The schedule as `GET /loops` describes it, at the configured intervals (`0` = off).
    /// Pure: it reads the schedule and computes nothing about the clock, so it is the same
    /// answer during the startup grace as after it — a restored `next_fire_at` already in the
    /// past reads as "due, waiting on the grace", which is the truth.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn info(&self, status_secs: u64, restart_secs: u64) -> LoopsInfo {
        let sched = self.schedule.borrow();
        LoopsInfo {
            status: loop_info(status_secs, sched.status_next, sched.status_last),
            restart: loop_info(restart_secs, sched.restart_next, sched.restart_last),
        }
    }

    /// Publish [`Self::info`] where the control plane can pick it up without the app crate
    /// having to hand it a `ReadModel` (see [`published`]). Called from the app tick, so it
    /// also refreshes during the startup grace, when nothing has polled yet.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn publish(&self, status_secs: u64, restart_secs: u64) {
        let info = self.info(status_secs, restart_secs);
        let mut cur = PUBLISHED.lock().unwrap_or_else(PoisonError::into_inner);
        if *cur != info {
            *cur = info;
        }
    }
}

/// One loop's published status. A loop with an interval of `0` is off, and advertises no next
/// firing even if a stale one is still in `loops.json` — but keeps `last_fired_at`, which is
/// history rather than a schedule.
#[tracing::instrument(level = "debug", ret)]
fn loop_info(interval: u64, next: Option<u64>, last: Option<u64>) -> LoopInfo {
    LoopInfo {
        enabled: interval > 0,
        interval_secs: interval,
        last_fired_at: last,
        next_fire_at: if interval == 0 { None } else { next },
    }
}

/// The starting value of [`PUBLISHED`]: both loops off and never fired, the same honest
/// default `ReadModel` itself carries before anything publishes. Spelled out rather than
/// `LoopsInfo::default()` because a `static` initializer must be `const`.
const NOTHING_PUBLISHED: LoopsInfo = LoopsInfo {
    status: LoopInfo {
        enabled: false,
        interval_secs: 0,
        last_fired_at: None,
        next_fire_at: None,
    },
    restart: LoopInfo {
        enabled: false,
        interval_secs: 0,
        last_fired_at: None,
        next_fire_at: None,
    },
};

/// The schedule as last published by the app tick, for the control host to copy into the
/// read-model. A process global and not a field on anything because the two ends never meet:
/// `Loops` lives on the GUI-thread `App` behind an `Rc`, and the read-model is republished
/// from `ControlHost::sync`, which is handed only the windows and the session manager.
static PUBLISHED: Mutex<LoopsInfo> = Mutex::new(NOTHING_PUBLISHED);

/// What the app last published about its loops. `NOTHING_PUBLISHED` until the first tick,
/// so a caller before then sees the same defaults as a build with no publisher at all.
#[tracing::instrument(level = "debug", ret)]
pub fn published() -> LoopsInfo {
    *PUBLISHED.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Default for Loops {
    #[tracing::instrument(level = "debug")]
    fn default() -> Self {
        Self::new()
    }
}

/// Seconds since the Unix epoch; `0` on a clock set before 1970, which only ever delays
/// the loops (nothing fires in the past).
#[tracing::instrument(level = "debug", ret)]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The command line typed into a freshly restarted monitored pane to put its tool back into
/// its conversation: `cd '<cwd>' && <prefix><bin> <resume args>` — or just `<bin>` when the
/// conversation is unknown, so the agent at least comes back up.
///
/// `prefix` is an environment assignment with its trailing space (`CLAUDE_CONFIG_DIR='…' `)
/// or empty; `session` is the tool's own resume key when one is known. A tool whose resume
/// shape [`hyperpanes_core::tools::resume_args`] does not vouch for starts fresh rather
/// than with a guessed flag.
#[tracing::instrument(level = "debug", ret)]
pub fn restart_line(
    tool_id: &str,
    bin: &str,
    cwd: Option<&str>,
    prefix: &str,
    session: Option<&str>,
) -> String {
    let args = session
        .and_then(|id| hyperpanes_core::tools::resume_args(tool_id, id))
        .unwrap_or_default();
    let mut cmd = format!("{prefix}{bin}");
    for a in &args {
        cmd.push(' ');
        cmd.push_str(a);
    }
    match cwd {
        Some(cwd) if !cwd.is_empty() => format!("cd '{cwd}' && {cmd}\r"),
        _ => format!("{cmd}\r"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh scratch directory per test (removed on drop).
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("hp-loops-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn file(&self) -> PathBuf {
            self.0.join("loops.json")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_interval_of_zero_switches_the_loop_off() {
        assert_eq!(decide(1000, 0, Some(900)), Decision::Off);
        assert_eq!(decide(1000, 0, None), Decision::Off);
    }

    #[test]
    fn an_unscheduled_loop_is_scheduled_one_interval_out_without_firing() {
        assert_eq!(decide(1000, 60, None), Decision::Schedule { next: 1060 });
    }

    #[test]
    fn a_loop_waits_until_its_time() {
        assert_eq!(decide(1000, 60, Some(1001)), Decision::Wait);
        assert_eq!(decide(1000, 60, Some(1060)), Decision::Wait);
    }

    #[test]
    fn a_due_loop_fires_and_is_rescheduled_in_rhythm() {
        assert_eq!(decide(1000, 60, Some(1000)), Decision::Fire { next: 1060 });
        assert_eq!(decide(1010, 60, Some(1000)), Decision::Fire { next: 1060 });
    }

    #[test]
    fn a_loop_that_missed_many_firings_while_down_fires_once_and_catches_up() {
        // Due at 1000, it is now 1250: four firings were missed; fire once, next at 1300.
        assert_eq!(decide(1250, 60, Some(1000)), Decision::Fire { next: 1300 });
    }

    #[test]
    fn a_shortened_interval_pulls_the_next_firing_in() {
        // Was scheduled a day out; the user set the loop to an hour.
        assert_eq!(
            decide(1000, 3600, Some(1000 + 86_400)),
            Decision::Schedule { next: 4600 }
        );
    }

    #[test]
    fn the_schedule_round_trips_through_its_file() {
        let dir = Scratch::new("roundtrip");
        let path = dir.file();
        assert_eq!(LoopSchedule::load_from(&path), LoopSchedule::default());
        let s = LoopSchedule {
            status_next: Some(42),
            status_last: Some(7),
            restart_next: None,
            restart_last: None,
        };
        s.save_to(&path);
        assert_eq!(LoopSchedule::load_from(&path), s);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(LoopSchedule::load_from(&path), LoopSchedule::default());
    }

    #[test]
    fn a_schedule_with_an_unknown_field_still_loads() {
        // Written by a newer build that grew a third loop. The two fields this build knows
        // must survive — dropping them would restart both countdowns on every downgrade.
        let dir = Scratch::new("unknown-field");
        let path = dir.file();
        std::fs::write(
            &path,
            r#"{"status_next":1900,"status_last":1000,"restart_next":87400,"digest_next":5,
                "digest": {"nested": [1, 2, 3]}}"#,
        )
        .unwrap();
        assert_eq!(
            LoopSchedule::load_from(&path),
            LoopSchedule {
                status_next: Some(1900),
                status_last: Some(1000),
                restart_next: Some(87_400),
                restart_last: None,
            }
        );
    }

    #[test]
    fn a_schedule_missing_a_known_field_defaults_it_to_unscheduled() {
        // Written by the build before `*_last` existed: the loops keep their rhythm and
        // merely report "never fired" until they next do.
        let dir = Scratch::new("missing-field");
        let path = dir.file();
        std::fs::write(&path, r#"{"status_next":1900,"restart_next":87400}"#).unwrap();
        assert_eq!(
            LoopSchedule::load_from(&path),
            LoopSchedule {
                status_next: Some(1900),
                status_last: None,
                restart_next: Some(87_400),
                restart_last: None,
            }
        );
        // Every field missing is the empty schedule, not an error.
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(LoopSchedule::load_from(&path), LoopSchedule::default());
    }

    #[test]
    fn a_schedule_with_a_malformed_known_field_starts_the_loops_afresh() {
        // Whole-file reject, matching `prefs::load` on a bad `settings.json`. Never a panic,
        // and never a partial schedule: both loops are simply scheduled again from now.
        let dir = Scratch::new("malformed-field");
        let path = dir.file();
        for bad in [
            r#"{"status_next":"soon","restart_next":87400}"#,
            r#"{"status_last":-1}"#,
            r#"{"status_next":[1900]}"#,
            "[]",
        ] {
            std::fs::write(&path, bad).unwrap();
            assert_eq!(
                LoopSchedule::load_from(&path),
                LoopSchedule::default(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_restart_resumes_the_schedule_it_left_behind() {
        let dir = Scratch::new("relaunch");
        let path = dir.file();
        let first = Loops::at(path.clone());
        // Fresh: both loops get scheduled, nothing fires.
        assert!(first.poll_at(1000, 900, 86_400).is_empty());
        assert_eq!(
            first.schedule(),
            LoopSchedule {
                status_next: Some(1900),
                status_last: None,
                restart_next: Some(87_400),
                restart_last: None,
            }
        );
        drop(first);
        // "Relaunch" past the status loop's time: it fires exactly once, and the restart
        // loop keeps its original time rather than starting a new day.
        let second = Loops::at(path);
        assert_eq!(second.poll_at(2000, 900, 86_400), vec![LoopKind::Status]);
        assert_eq!(second.poll_at(2001, 900, 86_400), Vec::<LoopKind>::new());
        assert_eq!(
            second.schedule(),
            LoopSchedule {
                status_next: Some(2800),
                status_last: Some(2000),
                restart_next: Some(87_400),
                restart_last: None,
            }
        );
    }

    #[test]
    fn a_firing_records_when_it_happened_and_that_survives_a_relaunch() {
        let dir = Scratch::new("last-fired");
        let path = dir.file();
        let first = Loops::at(path.clone());
        first.poll_at(1000, 60, 0);
        assert_eq!(first.schedule().status_last, None, "scheduling is not firing");
        assert_eq!(first.poll_at(1100, 60, 0), vec![LoopKind::Status]);
        assert_eq!(first.schedule().status_last, Some(1100));
        drop(first);
        // The stamp is on disk, not merely in memory: a relaunch still knows when it fired.
        let second = Loops::at(path);
        assert_eq!(second.schedule().status_last, Some(1100));
        // …and the startup grace does not disturb it: `poll` is silent for the first minute,
        // so nothing overwrites the restored stamp before the loop is genuinely due again.
        assert!(second.poll(60, 0).is_empty());
        assert_eq!(second.schedule().status_last, Some(1100));
    }

    #[test]
    fn a_loop_switched_off_forgets_its_next_firing_but_not_its_last() {
        let dir = Scratch::new("off-keeps-last");
        let l = Loops::at(dir.file());
        l.poll_at(1000, 60, 0);
        l.poll_at(1100, 60, 0);
        l.poll_at(1101, 0, 0);
        assert_eq!(l.schedule().status_next, None);
        assert_eq!(l.schedule().status_last, Some(1100), "history is not a schedule");
    }

    #[test]
    fn the_published_schedule_reports_the_interval_the_last_firing_and_the_next() {
        let dir = Scratch::new("info");
        let l = Loops::at(dir.file());
        l.poll_at(1000, 60, 86_400);
        l.poll_at(1100, 60, 86_400);
        let info = l.info(60, 86_400);
        assert_eq!(
            info.status,
            LoopInfo {
                enabled: true,
                interval_secs: 60,
                last_fired_at: Some(1100),
                next_fire_at: Some(1120),
            }
        );
        assert_eq!(
            info.restart,
            LoopInfo {
                enabled: true,
                interval_secs: 86_400,
                last_fired_at: None,
                next_fire_at: Some(87_400),
            }
        );
    }

    #[test]
    fn a_disabled_loop_publishes_no_next_firing_even_with_a_stale_one_on_disk() {
        let dir = Scratch::new("info-off");
        let path = dir.file();
        // A schedule left behind by a build where the loop was still on — nothing has polled
        // yet this run, so `status_next` is still set while the interval says off.
        std::fs::write(&path, r#"{"status_next":1900,"status_last":1000}"#).unwrap();
        let l = Loops::at(path);
        let status = l.info(0, 0).status;
        assert!(!status.enabled);
        assert_eq!(status.interval_secs, 0);
        assert_eq!(status.next_fire_at, None);
        assert_eq!(status.last_fired_at, Some(1000));
    }

    #[test]
    fn publishing_hands_the_control_plane_the_current_schedule() {
        let dir = Scratch::new("publish");
        let l = Loops::at(dir.file());
        l.poll_at(1000, 900, 0);
        l.poll_at(1900, 900, 0);
        l.publish(900, 0);
        let p = published();
        assert_eq!(p.status.last_fired_at, Some(1900));
        assert_eq!(p.status.next_fire_at, Some(2800));
        assert!(p.status.enabled);
        assert!(!p.restart.enabled);
    }

    #[test]
    fn switching_a_loop_off_forgets_its_schedule_and_on_again_restarts_it() {
        let dir = Scratch::new("toggle");
        let l = Loops::at(dir.file());
        l.poll_at(1000, 900, 86_400);
        l.poll_at(1001, 0, 86_400);
        assert_eq!(l.schedule().status_next, None);
        l.poll_at(1002, 900, 86_400);
        assert_eq!(l.schedule().status_next, Some(1902));
    }

    #[test]
    fn both_loops_can_fire_in_one_poll() {
        let dir = Scratch::new("both");
        let l = Loops::at(dir.file());
        l.poll_at(1000, 60, 120);
        assert_eq!(
            l.poll_at(1200, 60, 120),
            vec![LoopKind::Status, LoopKind::Restart]
        );
    }

    #[test]
    fn the_restart_line_resumes_a_known_conversation_in_its_directory() {
        assert_eq!(
            restart_line("claude", "claude", Some("/w/proj"), "", Some("abc-123")),
            "cd '/w/proj' && claude --resume abc-123\r"
        );
        assert_eq!(
            restart_line("claude", "claude", Some("/w/proj"), "CLAUDE_CONFIG_DIR='/c' ", Some("abc-123")),
            "cd '/w/proj' && CLAUDE_CONFIG_DIR='/c' claude --resume abc-123\r"
        );
    }

    #[test]
    fn the_restart_line_starts_fresh_when_the_conversation_is_unknown_or_unresumable() {
        assert_eq!(restart_line("claude", "claude", None, "", None), "claude\r");
        // A tool without a vouched-for resume shape never gets a guessed flag.
        assert_eq!(
            restart_line("aider", "aider", Some("/w"), "", Some("x")),
            "cd '/w' && aider\r"
        );
    }
}
