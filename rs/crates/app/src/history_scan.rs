//! Background scanner for the sidebar's expensive enumerations (#6): git worktree
//! listing and Claude session-history reads, both of which used to run synchronously on
//! the UI thread inside the render projection. One dedicated thread (the simpler
//! `update.rs` thread+snapshot pattern, not the full ambient-AI tokio engine) services
//! scan jobs over std mpsc channels; the UI pump drains finished results each tick and
//! folds them into the sidebar's caches, marking the state dirty so the projection
//! re-runs with fresh rows.
//!
//! The session side rides core's [`SessionCache`]: the thread keeps one cache per
//! project, so a refresh request re-parses only transcripts whose mtime/size changed —
//! repeated flyout opens cost a `read_dir` + stats, not a re-read of every `.jsonl`.
//!
//! UI-side, a pending set per job kind dedupes requests: the projection may ask for the
//! same project every dirty tick while a scan is in flight, and only the first ask
//! enqueues a job.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};

use hyperpanes_core::claude_history::{ClaudeSession, SessionCache};
use hyperpanes_core::tools::history::SessionProvider;
use hyperpanes_core::tools::{InferOutcome, PaneWatch, ToolSessionMark};

use crate::leftpanel::{self, ScannedSession};
use crate::sidebar::{self, WorktreeRow};

/// One scan request, sent UI → scanner thread.
enum Job {
    /// Re-scan `~/.claude/projects/<encoded>/*.jsonl` for this project root.
    Sessions(String),
    /// Re-run `git worktree list --porcelain` in this repo.
    Worktrees(String),
    /// Re-scan EVERY project for one tool's resumable sessions — the left panel's tool
    /// modes, not the sidebar's per-project list. Carries the human's `tool id -> path`
    /// overrides because the resumability verdict is decided here, on this thread, once
    /// per scan rather than once per frame on the UI thread.
    ToolSessions(String, BTreeMap<String, String>),
    /// Re-read the raw `(conversation id, project directory)` set out of one tool's history
    /// store, for [`session_infer`](hyperpanes_core::tools::session_infer).
    ///
    /// Deliberately NOT the same job as [`Job::ToolSessions`]: that one is the left panel's
    /// render feed — it resolves resumability, builds row labels, and sorts for the panel's
    /// heading contract. Inference wants none of that and must not be coupled to whether the
    /// panel happens to be showing this tool's mode. It shares the same cached provider on
    /// the scanner thread, so the two cost one warm re-scan between them, not two.
    ToolStore(String, BTreeMap<String, String>),
}

/// One finished scan, sent scanner thread → UI (drained by [`drain`]).
enum ScanResult {
    Sessions(String, Vec<ClaudeSession>),
    Worktrees(String, Vec<WorktreeRow>),
    ToolSessions(String, Vec<ScannedSession>),
    ToolStore(String, Vec<(String, String)>),
}

/// The UI-thread handle: the job sender plus the result receiver the pump drains.
struct Scanner {
    tx: Sender<Job>,
    rx: Receiver<ScanResult>,
}

thread_local! {
    /// The scanner handle, spawned lazily on first use (UI thread only).
    static SCANNER: Scanner = spawn_scanner();
    /// Project roots with a session scan in flight — dedupes per-tick re-requests.
    static PENDING_SESS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// Repo paths with a worktree scan in flight.
    static PENDING_WT: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// Tool ids with a whole-store session scan in flight.
    static PENDING_TOOL: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// Tool ids with an inference store read in flight.
    static PENDING_STORE: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// The latest store read per tool, stamped with the moment it was drained. The stamp is
    /// what lets a watch tell a snapshot it has already counted from one it hasn't: two
    /// looks at the *same* snapshot would find no new conversations and burn the watch's
    /// budget without ever having looked again.
    static STORE: RefCell<HashMap<String, (std::time::Instant, Vec<(String, String)>)>> =
        RefCell::new(HashMap::new());
    /// Panes whose conversation we are still trying to identify, keyed by pane uid, each
    /// with the stamp of the last store snapshot it consumed.
    static WATCHES: RefCell<HashMap<String, (PaneWatch, Option<std::time::Instant>)>> =
        RefCell::new(HashMap::new());
}

/// Spawn the scanner thread and return its UI-side handle. The thread owns one
/// [`SessionCache`] per project (the mtime/size fingerprints live there, across
/// flyout open/close cycles) and exits when the UI side drops the channels.
fn spawn_scanner() -> Scanner {
    let (job_tx, job_rx) = channel::<Job>();
    let (res_tx, res_rx) = channel::<ScanResult>();
    std::thread::Builder::new()
        .name("history-scan".to_string())
        .spawn(move || {
            let mut caches: HashMap<String, SessionCache> = HashMap::new();
            // One provider per tool, kept for the life of the thread: the mtime/size
            // fingerprints live inside it, so a re-scan re-parses only the transcripts that
            // changed. On this machine that is the difference between ~4s and ~8ms.
            //
            // Keyed with the overrides it was BUILT with, and rebuilt when they change: a
            // provider decides resumability with the binary it was handed, so a human
            // editing the path to their `claude` must not keep getting the old verdict. That
            // costs one cold re-scan, which is the right price for a rare, deliberate edit.
            type Cached = (BTreeMap<String, String>, Box<dyn SessionProvider>);
            let mut providers: HashMap<String, Cached> = HashMap::new();
            while let Ok(job) = job_rx.recv() {
                let res = match job {
                    Job::Sessions(root) => {
                        let cache = caches.entry(root.clone()).or_default();
                        // Union every account's transcript store: a session run under a
                        // rotated/non-default CLAUDE_CONFIG_DIR lives in that account's
                        // projects/, not ~/.claude (multi-account resume/history).
                        let sessions = cache.scan_project_all(Path::new(&root));
                        ScanResult::Sessions(root, sessions)
                    }
                    Job::Worktrees(repo) => {
                        let rows = sidebar::enumerate_worktrees(&repo);
                        ScanResult::Worktrees(repo, rows)
                    }
                    Job::ToolSessions(tool_id, overrides) => {
                        let stale = providers
                            .get(&tool_id)
                            .is_some_and(|(built_with, _)| *built_with != overrides);
                        if stale {
                            providers.remove(&tool_id);
                        }
                        let entry = match providers.entry(tool_id.clone()) {
                            std::collections::hash_map::Entry::Occupied(e) => Some(e.into_mut()),
                            std::collections::hash_map::Entry::Vacant(e) => {
                                provider_for(&tool_id, &overrides)
                                    .map(|p| e.insert((overrides.clone(), p)))
                            }
                        };
                        let rows = entry
                            .map(|(_, p)| leftpanel::scan_with(p.as_mut()))
                            .unwrap_or_default();
                        ScanResult::ToolSessions(tool_id, rows)
                    }
                    Job::ToolStore(tool_id, overrides) => {
                        let stale = providers
                            .get(&tool_id)
                            .is_some_and(|(built_with, _)| *built_with != overrides);
                        if stale {
                            providers.remove(&tool_id);
                        }
                        let entry = match providers.entry(tool_id.clone()) {
                            std::collections::hash_map::Entry::Occupied(e) => Some(e.into_mut()),
                            std::collections::hash_map::Entry::Vacant(e) => {
                                provider_for(&tool_id, &overrides)
                                    .map(|p| e.insert((overrides.clone(), p)))
                            }
                        };
                        let rows = entry
                            .map(|(_, p)| {
                                p.scan()
                                    .into_iter()
                                    // A non-exact origin's `project` is a LABEL the provider
                                    // reconstructed, not a directory it read — matching a
                                    // pane's cwd against one would be matching against a
                                    // guess, which is the thing inference exists to avoid.
                                    .filter(|s| s.project_origin.is_exact())
                                    .map(|s| (s.id, s.project.to_string_lossy().into_owned()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        ScanResult::ToolStore(tool_id, rows)
                    }
                };
                if res_tx.send(res).is_err() {
                    break;
                }
            }
        })
        .ok();
    Scanner {
        tx: job_tx,
        rx: res_rx,
    }
}

/// The provider serving `tool_id`, or `None` for a tool that has no history provider yet
/// (it still gets a mode in the strip — the panel shows its empty state rather than the
/// tool vanishing from a list the human curated).
fn provider_for(
    tool_id: &str,
    overrides: &BTreeMap<String, String>,
) -> Option<Box<dyn SessionProvider>> {
    match tool_id {
        hyperpanes_core::tools::history::claude::TOOL_ID => Some(Box::new(
            hyperpanes_core::tools::history::claude::ClaudeProvider::with_overrides(
                overrides.clone(),
            ),
        )),
        hyperpanes_core::tools::history::cursor::TOOL_ID => Some(Box::new(
            hyperpanes_core::tools::history::cursor::CursorProvider::with_overrides(
                overrides.clone(),
            ),
        )),
        hyperpanes_core::tools::history::copilot::TOOL_ID => Some(Box::new(
            hyperpanes_core::tools::history::copilot::CopilotProvider::with_overrides(
                overrides.clone(),
            ),
        )),
        _ => None,
    }
}

/// Ask for a (re-)scan of `project_root`'s Claude sessions. No-op while one is already
/// in flight for that project.
pub fn request_sessions(project_root: &str) {
    let fresh = PENDING_SESS.with(|p| p.borrow_mut().insert(project_root.to_string()));
    if fresh {
        SCANNER.with(|s| {
            let _ = s.tx.send(Job::Sessions(project_root.to_string()));
        });
    }
}

/// Ask for a (re-)enumeration of `repo_path`'s worktrees. No-op while one is in flight.
pub fn request_worktrees(repo_path: &str) {
    let fresh = PENDING_WT.with(|p| p.borrow_mut().insert(repo_path.to_string()));
    if fresh {
        SCANNER.with(|s| {
            let _ = s.tx.send(Job::Worktrees(repo_path.to_string()));
        });
    }
}

/// Ask for a (re-)scan of `tool_id`'s resumable sessions across every project. No-op while
/// one is already in flight for that tool — the projection asks on every dirty tick the
/// panel is showing that mode, and only the first ask enqueues a job.
/// Whether a tool-session scan for `tool_id` is still in flight.
///
/// The panel needs this to tell "looked, found nothing" apart from "haven't looked yet": a
/// cold cache hands back no rows, and announcing "No resumable sessions found" while the
/// scanner is still walking the transcripts states a verdict on a question nobody has
/// answered. `PENDING_TOOL` already holds exactly that fact — it is what keeps a second
/// request from queueing a duplicate job — so this only reads it.
pub fn tool_scan_pending(tool_id: &str) -> bool {
    PENDING_TOOL.with(|p| p.borrow().contains(tool_id))
}

pub fn request_tool_sessions(tool_id: &str, overrides: BTreeMap<String, String>) {
    let fresh = PENDING_TOOL.with(|p| p.borrow_mut().insert(tool_id.to_string()));
    if fresh {
        SCANNER.with(|s| {
            let _ = s.tx.send(Job::ToolSessions(tool_id.to_string(), overrides));
        });
    }
}

/// Start trying to work out which conversation pane `uid` is in, by watching `tool`'s
/// history store for a conversation that appears in `cwd` after this moment.
///
/// Idempotent per pane: asking again for a pane already being watched for the same tool
/// leaves the existing watch — and its baseline — alone, which matters because the caller
/// is the pump and asks on every tick the pane still has no conversation. A pane seen
/// running a *different* tool starts over; the old tool's baseline says nothing about the
/// new one.
///
/// A pane with a directory the resume path could not use is not watched at all
/// ([`PaneWatch::new`] refuses it), because an adoption made there could never be spent.
pub fn watch_pane(uid: &str, tool: &str, cwd: &str) {
    WATCHES.with(|w| {
        let mut w = w.borrow_mut();
        if w.get(uid).is_some_and(|(pw, _)| pw.tool() == tool) {
            return;
        }
        if let Some(pw) = PaneWatch::new(tool, cwd) {
            w.insert(uid.to_string(), (pw, None));
        } else {
            w.remove(uid);
        }
    });
}

/// Stop watching pane `uid` — it closed, or it has a conversation now. Watches are keyed
/// by uid and would otherwise outlive every pane the session ever opened.
pub fn forget_pane(uid: &str) {
    WATCHES.with(|w| {
        w.borrow_mut().remove(uid);
    });
}

/// Drop every watch whose pane is not in `alive`. The pump passes the panes that still
/// want a conversation found, so this collects both closed panes and panes that got one.
pub fn retain_panes(alive: &HashSet<String>) {
    WATCHES.with(|w| {
        w.borrow_mut().retain(|uid, _| alive.contains(uid));
    });
}

/// Advance every watch that is due and return the conversations that got identified, as
/// `(pane uid, mark)`.
///
/// The pacing is the point. A watch asks for a store read only when it is due *and* the
/// snapshot it can see is one it has already counted, so the scan rate is one read per
/// tool per [`session_infer::SCAN_EVERY`], not one per pump tick — and the reads coalesce,
/// because `PENDING_STORE` keeps a second request from queueing while one is in flight and
/// every watch on the same tool consumes the same snapshot.
///
/// [`session_infer::SCAN_EVERY`]: hyperpanes_core::tools::session_infer::SCAN_EVERY
pub fn poll_inference(overrides: &BTreeMap<String, String>) -> Vec<(String, ToolSessionMark)> {
    let now = std::time::Instant::now();
    let mut adopted: Vec<(String, ToolSessionMark)> = Vec::new();
    let mut wanted: Vec<String> = Vec::new();
    WATCHES.with(|w| {
        let mut w = w.borrow_mut();
        STORE.with(|store| {
            let store = store.borrow();
            w.retain(|uid, (watch, seen)| {
                if !watch.due(now) {
                    return true;
                }
                let Some((stamp, rows)) = store.get(watch.tool()) else {
                    wanted.push(watch.tool().to_string());
                    return true;
                };
                if *seen == Some(*stamp) {
                    wanted.push(watch.tool().to_string());
                    return true;
                }
                *seen = Some(*stamp);
                let pairs = rows.iter().map(|(id, dir)| (id.as_str(), dir.as_str()));
                match watch.observe(now, pairs) {
                    InferOutcome::Watching => true,
                    InferOutcome::Adopted(mark) => {
                        adopted.push((uid.clone(), mark));
                        false
                    }
                    // Ambiguous or out of budget: this pane's conversation will not be
                    // worked out, and re-looking cannot change that.
                    InferOutcome::Stopped(_) => false,
                }
            });
        });
    });
    for tool in wanted {
        request_tool_store(&tool, overrides.clone());
    }
    adopted
}

/// Ask for a raw store read for `tool_id`. No-op while one is in flight.
fn request_tool_store(tool_id: &str, overrides: BTreeMap<String, String>) {
    let fresh = PENDING_STORE.with(|p| p.borrow_mut().insert(tool_id.to_string()));
    if fresh {
        SCANNER.with(|s| {
            let _ = s.tx.send(Job::ToolStore(tool_id.to_string(), overrides));
        });
    }
}

/// Drain every finished scan into the sidebar caches. Returns `true` when anything
/// landed — the caller (the pump) marks the state dirty so the projection re-runs and
/// the flyout refreshes. Called every tick; an empty channel is a cheap `try_recv` miss.
pub fn drain() -> bool {
    let mut any = false;
    SCANNER.with(|s| {
        while let Ok(res) = s.rx.try_recv() {
            any = true;
            match res {
                ScanResult::Sessions(root, sessions) => {
                    PENDING_SESS.with(|p| {
                        p.borrow_mut().remove(&root);
                    });
                    sidebar::apply_sessions(&root, sessions);
                }
                ScanResult::Worktrees(repo, rows) => {
                    PENDING_WT.with(|p| {
                        p.borrow_mut().remove(&repo);
                    });
                    sidebar::apply_worktrees(&repo, rows);
                }
                ScanResult::ToolSessions(tool_id, rows) => {
                    PENDING_TOOL.with(|p| {
                        p.borrow_mut().remove(&tool_id);
                    });
                    leftpanel::apply_tool_sessions(&tool_id, rows);
                }
                ScanResult::ToolStore(tool_id, rows) => {
                    PENDING_STORE.with(|p| {
                        p.borrow_mut().remove(&tool_id);
                    });
                    STORE.with(|st| {
                        st.borrow_mut()
                            .insert(tool_id, (std::time::Instant::now(), rows));
                    });
                }
            }
        }
    });
    any
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unscanned_tool_reads_as_pending_until_the_result_is_drained() {
        let id = "hp-test-no-such-tool";
        // Nothing has been asked for yet, so nothing is in flight.
        assert!(!tool_scan_pending(id));

        request_tool_sessions(id, BTreeMap::new());
        // Pending the moment the job is queued — this is the fact the panel reads to say
        // "looking" instead of "none found" over a cache that has never been filled.
        assert!(tool_scan_pending(id));

        // Only `drain` clears it, so the flag cannot go false while the answer is unknown.
        // The scanner finds no such tool and returns an empty list; draining it is what
        // turns the panel's message into the real verdict.
        while tool_scan_pending(id) {
            drain();
        }
        assert!(!tool_scan_pending(id));
    }

    fn watched(uid: &str) -> bool {
        WATCHES.with(|w| w.borrow().contains_key(uid))
    }

    fn store_pending(tool: &str) -> bool {
        PENDING_STORE.with(|p| p.borrow().contains(tool))
    }

    #[test]
    fn a_pane_stops_being_watched_the_moment_it_is_not_asked_for() {
        watch_pane("hp-test-pane", "claude", "/tmp");
        assert!(watched("hp-test-pane"));
        // Asking again for the same pane and tool must not restart the watch: the baseline
        // it took on its first look is the whole basis of "new since the pane started", and
        // the pump asks on every pass the pane still has no conversation.
        watch_pane("hp-test-pane", "claude", "/tmp");
        assert!(watched("hp-test-pane"));

        // The pump passes the panes that still want one; everything else — closed panes,
        // panes that just got a conversation — falls out, so the map cannot grow with
        // every pane the session ever opened.
        retain_panes(&HashSet::new());
        assert!(!watched("hp-test-pane"));
    }

    #[test]
    fn a_pane_with_no_usable_directory_is_never_watched() {
        // Nothing scoped to this directory could ever be spent: the restore path refuses a
        // relative cwd, so an adoption made here would be a mark that cannot resume.
        watch_pane("hp-test-relative", "claude", "some/relative/dir");
        assert!(!watched("hp-test-relative"));
    }

    #[test]
    fn a_watch_asks_for_one_store_read_per_interval_not_one_per_tick() {
        let tool = "hp-test-no-such-tool";
        watch_pane("hp-test-poll", tool, "/tmp");

        // Due, and no snapshot to look at yet: one read is queued.
        assert!(poll_inference(&BTreeMap::new()).is_empty());
        assert!(store_pending(tool));
        // The pump polls every couple of seconds; a second poll while the read is in
        // flight must not queue a second one.
        assert!(poll_inference(&BTreeMap::new()).is_empty());
        assert!(store_pending(tool));

        while store_pending(tool) {
            drain();
        }
        // The snapshot has landed (empty — there is no such tool). Consuming it is the
        // watch's first look, which only establishes the baseline...
        assert!(poll_inference(&BTreeMap::new()).is_empty());
        assert!(watched("hp-test-poll"));
        // ...and having just looked, the watch is not due again for a full scan interval,
        // so nothing is queued. This is what keeps the fallback off the pump's cadence.
        assert!(poll_inference(&BTreeMap::new()).is_empty());
        assert!(!store_pending(tool));
    }
}
