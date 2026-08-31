//! The fallback for a tool with no session hook: work out which conversation a pane is in
//! by watching what appears on disk, and **refuse to answer** when more than one thing does.
//!
//! # The gap this fills
//!
//! A pane spawned out of the left panel's session list was handed one exact conversation,
//! so its [`ToolSessionMark`] is written at birth. A pane running a tool with a lifecycle
//! hook ([`crate::tools::session_hook`], [`crate::claude_hook`]) has the tool itself name
//! the id, from inside a process carrying `HYPERPANES_PANE_ID`. Neither applies to a pane
//! where the human opened a plain shell and typed a hook-less agent's name: on the next
//! relaunch that pane comes back as a shell in the right directory, and the conversation
//! is gone.
//!
//! What is left is circumstantial: the tool's own history store gains a conversation for
//! that directory shortly after the pane started running it. That is evidence, not proof,
//! and this module is built around the difference.
//!
//! # How it decides, and how it refuses to
//!
//! Take a **baseline** — the set of conversation ids the tool already has for the pane's
//! directory — the first time we look after the pane became a tool pane. Look again later.
//! One id that is new since the baseline, in that directory, is the pane's conversation.
//!
//! **Two or more new ids and we adopt none of them, permanently.** A wrong mark is far
//! worse than no mark: no mark loses a conversation, a wrong mark silently resumes
//! *somebody else's* — the second agent the human started in another pane in the same
//! repo — and the human's next session is spent in a chat they did not open, quite possibly
//! writing into it. There is no tie-break worth having here: "the earlier one" and "the one
//! whose id sorts first" are both guesses wearing a rule's clothes.
//!
//! Ambiguity is **terminal** rather than a state to wait out. The extra ids do not
//! disappear from the store, so "ambiguous now, unambiguous later" can only mean we chose
//! to stop counting one of them — guessing, by another name and one round later.
//!
//! # The accepted miss
//!
//! If the human's first prompt lands before the baseline is taken, the conversation is
//! already IN the baseline and can never be new. That pane keeps no mark. This is the
//! failure the design is aimed at: it errs towards forgetting a conversation, never
//! towards resuming the wrong one.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::claude_panes::{valid_resume_cwd, valid_session_id};
use crate::tools::session_mark::ToolSessionMark;

/// How long to wait between looks.
///
/// Deliberately far off any hot loop: one observation is a whole-store scan of every
/// project the tool has ever seen, which is why the providers are cache-backed at all.
/// It is also far longer than the thing being measured needs — the window only has to be
/// short enough that two unrelated conversations in the SAME directory rarely land inside
/// one of them, and a human starting two agents in one repo within twenty seconds is the
/// case that correctly resolves to "adopt none".
pub const SCAN_EVERY: Duration = Duration::from_secs(20);

/// How many looks before giving up (≈ 10 minutes at [`SCAN_EVERY`]).
///
/// The budget is generous on purpose: most of these tools write nothing to their history
/// store until the human's FIRST PROMPT, which can be minutes after the pane started —
/// giving up after a handful of seconds would mean the fallback almost never fires for
/// the pane a human opened and then went to read something.
///
/// It is bounded on purpose too. A pane that has gone ten minutes without a single new
/// conversation appearing is not one this evidence can still speak to: whatever shows up
/// after that is as likely to be the pane next door, and an unbounded watch would keep a
/// growing per-pane scan alive for the life of the window to buy exactly that.
pub const MAX_OBSERVATIONS: u32 = 30;

/// What one look concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing decided yet — look again after [`SCAN_EVERY`].
    Watching,
    /// Exactly one conversation appeared for this directory since the baseline.
    Adopted(ToolSessionMark),
    /// Stopped looking, with no mark. Terminal.
    Stopped(Stopped),
}

/// Why a watch ended without a mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// Two or more conversations appeared for this directory in the same window. See the
    /// module doc: this is a refusal, not a failure.
    Ambiguous,
    /// The time budget ran out with nothing new. Ordinary — most panes never start a
    /// conversation at all.
    Expired,
}

/// One pane's in-flight inference.
#[derive(Debug, Clone)]
pub struct PaneWatch {
    /// Registry id of the tool whose store is being watched.
    tool: String,
    /// The directory the pane is in — the only directory whose conversations count.
    cwd: String,
    /// Ids the tool already had for `cwd` when we started looking. `None` until the first
    /// observation, which establishes it and can therefore never adopt anything.
    baseline: Option<HashSet<String>>,
    /// Observations spent, against [`MAX_OBSERVATIONS`].
    looks: u32,
    /// When the last observation landed, so the caller can pace itself off the watch
    /// rather than keeping a parallel clock per pane.
    last_look: Option<Instant>,
    /// Set once a decision is made; a finished watch answers [`Outcome::Stopped`] to
    /// every further look rather than silently starting over.
    done: Option<Stopped>,
}

impl PaneWatch {
    /// Start watching `tool`'s store for a conversation belonging to `cwd`.
    ///
    /// `None` when `cwd` is not a directory a mark could be built from — a pane whose
    /// tracked cwd is empty or relative can never produce an adoptable mark, so refusing
    /// here is cheaper and clearer than watching it for ten minutes to find out.
    pub fn new(tool: &str, cwd: &str) -> Option<Self> {
        let cwd = normalize(cwd);
        valid_resume_cwd(&cwd).then_some(PaneWatch {
            tool: tool.to_string(),
            cwd,
            baseline: None,
            looks: 0,
            last_look: None,
            done: None,
        })
    }

    /// The tool whose store this watch reads.
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// Whether enough time has passed for another look. `true` before the first one, so a
    /// fresh watch takes its baseline at the next opportunity rather than one interval late.
    pub fn due(&self, now: Instant) -> bool {
        self.done.is_none()
            && self
                .last_look
                .is_none_or(|t| now.duration_since(t) >= SCAN_EVERY)
    }

    /// Fold one scan of the tool's store into the watch.
    ///
    /// `sessions` is every conversation the tool knows about, as `(id, project directory)`
    /// — the whole store, not a pre-filtered slice, because the directory match is part of
    /// the decision and belongs next to the rest of it.
    pub fn observe<'a, I>(&mut self, now: Instant, sessions: I) -> Outcome
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        if let Some(s) = self.done {
            return Outcome::Stopped(s);
        }
        // Ids that would not survive landing on a command line are dropped here rather
        // than at adoption, so a junk row in the store cannot make a real pair of ids look
        // like an ambiguous three.
        let here: HashSet<String> = sessions
            .into_iter()
            .filter(|(id, dir)| valid_session_id(id) && normalize(dir) == self.cwd)
            .map(|(id, _)| id.to_string())
            .collect();
        self.looks += 1;
        self.last_look = Some(now);

        let Some(baseline) = &self.baseline else {
            // First look: this IS the baseline. Nothing here can be "new", including a
            // conversation the human started seconds ago — that is the accepted miss.
            self.baseline = Some(here);
            return self.expire_or_watch();
        };
        let mut fresh = here.difference(baseline);
        let Some(first) = fresh.next() else {
            return self.expire_or_watch();
        };
        if fresh.next().is_some() {
            // Two or more. Adopt NONE — and never reconsider: the extra ids are not going
            // to vanish from the store, so a later look could only "resolve" this by
            // ignoring one of them, which is the guess this whole module exists to refuse.
            self.done = Some(Stopped::Ambiguous);
            return Outcome::Stopped(Stopped::Ambiguous);
        }
        // Both halves were gated on the way in (the id just above, the directory at
        // construction), so this cannot fail — but it is the one constructor for a mark,
        // and re-deriving the gate here rather than reaching past it keeps that true.
        match ToolSessionMark::new(first, &self.cwd) {
            // The tool is stamped on because the pane this rescues is, by definition, one
            // nobody spawned as a tool: its persisted kind is `Terminal`, so without the
            // tool the id is a conversation nothing can re-enter.
            Some(mark) => {
                self.done = Some(Stopped::Expired); // decided; no further looks
                Outcome::Adopted(mark.with_tool(&self.tool))
            }
            None => self.expire_or_watch(),
        }
    }

    /// Keep watching, unless the budget is spent.
    fn expire_or_watch(&mut self) -> Outcome {
        if self.looks >= MAX_OBSERVATIONS {
            self.done = Some(Stopped::Expired);
            return Outcome::Stopped(Stopped::Expired);
        }
        Outcome::Watching
    }
}

/// Compare directories the way the providers report them: trailing separators are noise,
/// and a pane's tracked cwd and a tool's recorded project directory disagree about them
/// often enough to matter. Nothing more clever than that — resolving symlinks would make
/// two genuinely different recorded paths compare equal, which is exactly the kind of
/// widening that turns "one new conversation" into "two".
fn normalize(dir: &str) -> String {
    let t = dir.trim_end_matches(['/', '\\']);
    if t.is_empty() {
        dir.to_string()
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn the_first_look_only_establishes_the_baseline() {
        let mut w = PaneWatch::new("cursor-agent", "/tmp/proj").unwrap();
        // Even a conversation sitting right there in the pane's directory is baseline, not
        // evidence: we have no idea whether this pane started it.
        let out = w.observe(t0(), [("aaaa-1111", "/tmp/proj")]);
        assert_eq!(out, Outcome::Watching);
    }

    #[test]
    fn one_new_conversation_in_the_panes_directory_is_the_panes_conversation() {
        let mut w = PaneWatch::new("cursor-agent", "/tmp/proj").unwrap();
        assert_eq!(
            w.observe(t0(), [("aaaa-1111", "/tmp/proj")]),
            Outcome::Watching
        );
        let out = w.observe(
            t0(),
            [("aaaa-1111", "/tmp/proj"), ("bbbb-2222", "/tmp/proj")],
        );
        match out {
            Outcome::Adopted(m) => {
                assert_eq!(m.id, "bbbb-2222");
                assert_eq!(m.cwd, "/tmp/proj");
                assert_eq!(m.tool.as_deref(), Some("cursor-agent"));
            }
            other => panic!("expected an adoption, got {other:?}"),
        }
        // Decided is decided: a further look does not re-open the question.
        assert_eq!(
            w.observe(t0(), [("cccc-3333", "/tmp/proj")]),
            Outcome::Stopped(Stopped::Expired)
        );
    }

    #[test]
    fn two_new_conversations_at_once_adopt_none_of_them_and_never_will() {
        // The rule the whole module exists for. A wrong mark silently resumes somebody
        // else's conversation; no mark merely forgets one.
        let mut w = PaneWatch::new("cursor-agent", "/tmp/proj").unwrap();
        w.observe(t0(), [("aaaa-1111", "/tmp/proj")]);
        assert_eq!(
            w.observe(
                t0(),
                [
                    ("aaaa-1111", "/tmp/proj"),
                    ("bbbb-2222", "/tmp/proj"),
                    ("cccc-3333", "/tmp/proj"),
                ],
            ),
            Outcome::Stopped(Stopped::Ambiguous)
        );
        // And it stays refused, even once one of the two would look like the "obvious"
        // answer — the extra ids never leave the store, so there is nothing to resolve.
        assert_eq!(
            w.observe(
                t0(),
                [("aaaa-1111", "/tmp/proj"), ("bbbb-2222", "/tmp/proj")]
            ),
            Outcome::Stopped(Stopped::Ambiguous)
        );
    }

    #[test]
    fn a_conversation_in_another_directory_is_not_this_panes() {
        let mut w = PaneWatch::new("cursor-agent", "/tmp/proj").unwrap();
        w.observe(t0(), [("aaaa-1111", "/tmp/proj")]);
        // Two new ids, but only one of them is here — the other belongs to a pane in a
        // different repo and must not make this one ambiguous either.
        let out = w.observe(
            t0(),
            [
                ("aaaa-1111", "/tmp/proj"),
                ("bbbb-2222", "/tmp/proj"),
                ("dddd-4444", "/tmp/other"),
            ],
        );
        assert!(matches!(out, Outcome::Adopted(m) if m.id == "bbbb-2222"));
    }

    #[test]
    fn a_trailing_separator_does_not_hide_the_panes_own_conversation() {
        let mut w = PaneWatch::new("cursor-agent", "/tmp/proj/").unwrap();
        w.observe(t0(), []);
        let out = w.observe(t0(), [("bbbb-2222", "/tmp/proj")]);
        assert!(matches!(out, Outcome::Adopted(m) if m.cwd == "/tmp/proj"));
    }

    #[test]
    fn a_pane_that_never_starts_a_conversation_stops_looking_on_a_bounded_budget() {
        let mut w = PaneWatch::new("cursor-agent", "/tmp/proj").unwrap();
        for _ in 0..MAX_OBSERVATIONS - 1 {
            assert_eq!(
                w.observe(t0(), [("aaaa-1111", "/tmp/proj")]),
                Outcome::Watching
            );
        }
        assert_eq!(
            w.observe(t0(), [("aaaa-1111", "/tmp/proj")]),
            Outcome::Stopped(Stopped::Expired)
        );
        assert!(!w.due(t0()), "a finished watch is never due again");
    }

    #[test]
    fn a_hostile_id_is_invisible_rather_than_ambiguous() {
        // Provider output is parsed off disk. An id that would not survive landing on a
        // command line is dropped before the count, so it can neither be adopted nor turn
        // the one real new conversation into an "ambiguous" pair.
        let mut w = PaneWatch::new("cursor-agent", "/tmp/proj").unwrap();
        w.observe(t0(), []);
        assert_eq!(
            w.observe(t0(), [("x; rm -rf /", "/tmp/proj")]),
            Outcome::Watching
        );
        let out = w.observe(
            t0(),
            [("x; rm -rf /", "/tmp/proj"), ("bbbb-2222", "/tmp/proj")],
        );
        assert!(matches!(out, Outcome::Adopted(m) if m.id == "bbbb-2222"));
    }

    #[test]
    fn a_pane_with_no_usable_directory_is_never_watched_at_all() {
        // Resume is directory-scoped for every one of these tools, so a mark built without
        // a real directory could not be spent anyway.
        assert!(PaneWatch::new("cursor-agent", "").is_none());
        assert!(PaneWatch::new("cursor-agent", "relative/path").is_none());
        assert!(PaneWatch::new("cursor-agent", "/has'quote").is_none());
    }

    #[test]
    fn a_fresh_watch_is_due_at_once_but_not_again_until_the_interval_passes() {
        let mut w = PaneWatch::new("cursor-agent", "/tmp/proj").unwrap();
        let now = t0();
        assert!(
            w.due(now),
            "the baseline must be taken as early as possible"
        );
        w.observe(now, []);
        assert!(!w.due(now));
        assert!(w.due(now + SCAN_EVERY));
    }
}
