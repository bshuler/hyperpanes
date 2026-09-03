//! UI-thread operations the control plane cannot perform itself.
//!
//! The read model is authoritative for PANES: `dispatch` mutates it in place off the UI
//! thread and `ControlHost::reconcile` adopts the result into the GUI. TABS run the other
//! way round — the GUI's `State` owns them, and every sync tick *republishes* the whole
//! windows→tabs→panes tree (`ReadModel::publish_replace`), so a tab edit written into the
//! read model would be overwritten a frame later. Preferences are the same story: they live
//! in the GUI's `Settings` and their side effects (font reload, palette remap, per-pane
//! repaint) can only run on the UI thread.
//!
//! So tab + settings writes are QUEUED here instead. The route validates the request and
//! pushes an op; the GUI host drains the queue at the top of its next sync tick and applies
//! each op to `State` before publishing. That is the same shape as the `restartApp` flag —
//! set off-thread, performed on the UI thread — only with a payload.
//!
//! One consequence is deliberate and worth stating plainly: these commands are **accepted,
//! not completed**, when the HTTP call returns. `newTab` can still report the id the tab
//! *will* have, because tab ids are positional (`"{window_id}:{index}"`) and an append lands
//! at the current tab count — and only the UI thread ever appends. A caller that needs to see
//! the result reads `/state` afterwards.
//!
//! In a headless embedder nothing drains the queue, which is why it is capped: a queue that
//! nobody reads must not grow without bound.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

/// The answer to a [`UiOp::PatchSettings`]: `Ok` once every window has applied the patch,
/// `Err(reason)` when the preferences layer rejected it. Settings are the one queued op whose
/// outcome the caller must see — a rejected value would otherwise be reported as `ok` and
/// silently left unapplied — so the route holds the receiving end and waits (bounded) for
/// this to be answered on the UI thread. Cloneable and comparable so [`UiOp`] can stay
/// `Clone + PartialEq`; two handles are equal only when they are the same channel.
#[derive(Clone, Debug)]
pub struct PatchReply(Arc<Mutex<Option<PatchSender>>>);

/// The sending half a [`PatchReply`] gives up on its first `send`.
type PatchSender = oneshot::Sender<Result<(), String>>;

impl PatchReply {
    /// A fresh channel: the handle to queue with the op, and the receiver the route awaits.
    #[tracing::instrument(level = "debug", ret)]
    pub fn new() -> (Self, oneshot::Receiver<Result<(), String>>) {
        let (tx, rx) = oneshot::channel();
        (PatchReply(Arc::new(Mutex::new(Some(tx)))), rx)
    }

    /// Answer the waiting route. Only the first call delivers; later calls are no-ops, and a
    /// receiver that already gave up (timed out) is ignored rather than an error.
    #[tracing::instrument(level = "debug", ret)]
    pub fn send(&self, result: Result<(), String>) {
        if let Some(tx) = self.0.lock().unwrap().take() {
            let _ = tx.send(result);
        }
    }
}

impl PartialEq for PatchReply {
    #[tracing::instrument(level = "debug", ret)]
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// One queued edit to the GUI's tabs or preferences.
#[derive(Debug, Clone, PartialEq)]
pub enum UiOp {
    /// Append a tab to `window_id`, optionally titled, seeded with one shell in `cwd`.
    NewTab {
        window_id: i64,
        title: Option<String>,
        cwd: Option<String>,
    },
    /// Close the tab and kill its panes. The GUI refuses this for a system tab.
    CloseTab { tab_id: String },
    /// Retitle the tab.
    RenameTab { tab_id: String, title: String },
    /// Make the tab active in its window.
    FocusTab { tab_id: String },
    /// Move the tab to index `to` within its window (clamped to the tab count).
    MoveTab { tab_id: String, to: usize },
    /// Mirror of the `setLayout` verb. The dispatch already wrote it to the read model, but the
    /// GUI's `publish` rebuilds every tab from its own state each tick and would snap the layout
    /// back — so the visible change has to be made on the UI thread as well.
    SetTabLayout { tab_id: String, layout: String },
    /// Merge a camelCase JSON object into the app preferences and apply it live. `reply`, when
    /// present, is answered by the GUI host with the outcome (see [`PatchReply`]); a `None`
    /// is fire-and-forget.
    PatchSettings {
        patch: serde_json::Value,
        reply: Option<PatchReply>,
    },
}

/// FIFO of pending [`UiOp`]s, drained once per GUI sync tick.
#[derive(Debug, Default)]
pub struct UiOpQueue {
    pending: VecDeque<UiOp>,
}

impl UiOpQueue {
    /// How many ops may be pending before pushes are refused. Sized for a burst of scripted
    /// edits (a skill laying out a dozen tabs) with room to spare, but small enough that a
    /// headless embedder — where nothing ever drains — stays bounded.
    pub const MAX_PENDING: usize = 256;

    #[tracing::instrument(level = "debug", ret)]
    pub fn new() -> Self {
        UiOpQueue {
            pending: VecDeque::new(),
        }
    }

    /// Queue an op. Returns `false` (and drops it) when the queue is full — the caller turns
    /// that into a 503 rather than silently losing the edit.
    #[tracing::instrument(level = "debug", ret)]
    pub fn push(&mut self, op: UiOp) -> bool {
        if self.pending.len() >= Self::MAX_PENDING {
            return false;
        }
        self.pending.push_back(op);
        true
    }

    /// Take everything pending, in submission order.
    #[tracing::instrument(level = "debug", ret)]
    pub fn drain(&mut self) -> Vec<UiOp> {
        self.pending.drain(..).collect()
    }

    #[tracing::instrument(level = "debug", ret)]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    #[tracing::instrument(level = "debug", ret)]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rename(n: &str) -> UiOp {
        UiOp::RenameTab {
            tab_id: "0:0".to_string(),
            title: n.to_string(),
        }
    }

    #[test]
    fn drain_is_fifo_and_empties_the_queue() {
        let mut q = UiOpQueue::new();
        assert!(q.push(rename("a")));
        assert!(q.push(rename("b")));
        assert_eq!(q.len(), 2);
        let got = q.drain();
        assert_eq!(got, vec![rename("a"), rename("b")]);
        assert!(q.is_empty());
    }

    #[test]
    fn a_full_queue_refuses_rather_than_dropping_the_oldest() {
        let mut q = UiOpQueue::new();
        for i in 0..UiOpQueue::MAX_PENDING {
            assert!(q.push(rename(&i.to_string())));
        }
        assert!(!q.push(rename("overflow")));
        // The refusal is the only loss: everything accepted is still there, in order.
        let got = q.drain();
        assert_eq!(got.len(), UiOpQueue::MAX_PENDING);
        assert_eq!(got[0], rename("0"));
    }
}
