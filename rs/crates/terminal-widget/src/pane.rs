//! [`TerminalPane`] — the reusable Rust controller for one terminal pane.
//!
//! It owns the renderer-agnostic grid model ([`TermGrid`]) and a chosen [`PaneRenderer`]
//! (software or GPU), and exposes a small, Wave-2-friendly API. It is deliberately
//! **decoupled from the session transport**: the caller pumps it.
//!
//! ## Lifecycle (how the app-shell drives N of these)
//! 1. Spawn/attach a session in `hyperpanes_core::session_manager` sized to your initial
//!    `cols`×`rows`, and construct a `TerminalPane` of the same size.
//! 2. On each `SessionEvent::Data { data, .. }` for this pane → [`feed`](Self::feed), then
//!    drain [`take_replies`](Self::take_replies) and `SessionManager::write` them back
//!    (DSR/DA answers — the shell hangs without them).
//! 3. On a Slint key event → [`crate::keys::encode_key`] → `SessionManager::write`.
//! 4. On a geometry change → compute new `cols`×`rows` from the pixel size and the font
//!    cell metrics, [`resize`](Self::resize) the pane, and `SessionManager::resize` the
//!    session if it returns `true`.
//! 5. Each frame (a Slint `Timer`): if [`take_dirty`](Self::take_dirty) (or the cursor
//!    blink flipped) → [`render`](Self::render) and hand the `slint::Image` to your model.
//!
//! The `Font` is passed in at render time so a whole fleet of panes can share one glyph
//! cache (it is `&mut` because rasterization is lazy/cached).

use crate::clipboard::Clipboard;
use crate::font::Font;
use crate::grid::TermGrid;
use crate::links::{
    extract_commit_candidates, extract_path_candidates, extract_url_candidates, is_path_root,
    trim_trailing_punct, PathCandidate, UrlCandidate,
};
use crate::render::{PaneRenderer, RenderOpts};
use crate::search::{self, Match};
use crate::selection::{self, Selection};
use hyperpanes_core::git;
use hyperpanes_core::paths::{self, ResolveResult};
use slint::Image;
use std::collections::HashMap;
use std::time::Instant;

/// How long a copy/paste indicator ("toast") stays up, in ms — matches the Electron pane's
/// 1.6s auto-dismiss in `Terminal.tsx`.
const TOAST_MS: u128 = 1600;

/// Minimum pointer travel from the press point (logical px) before a left-press is treated as a
/// drag-select rather than a click. A click frequently twitches a pixel or two; if that twitch
/// straddles a cell boundary the selection would otherwise flip to `dragged` and copy-on-select,
/// clobbering the clipboard right before a paste (the `$c.Dispose()`-rides-along bug). Below this
/// slop the head never tracks, so a click can never copy — matching the few-px dead zone xterm /
/// the Electron pane allow before a drag begins.
const DRAG_THRESHOLD_PX: f32 = 4.0;

/// Controller for a single terminal pane: grid model + a pluggable renderer.
pub struct TerminalPane {
    grid: TermGrid,
    renderer: Box<dyn PaneRenderer>,
    /// This pane's working directory, used to resolve relative path tokens (the renderer-side
    /// half of `core::paths`). `None` falls back to the home dir, matching the pty start dir.
    cwd: Option<String>,
    /// Verified paths cached for this pane's lifetime, keyed by `cwd\x1ftoken`. Only *existing*
    /// paths are cached (negatives aren't), so a file the shell creates becomes clickable on the
    /// next hover — mirroring the Electron renderer's `verified` map.
    verified: HashMap<String, ResolveResult>,
    /// Commit hashes asked about in this pane, keyed by `cwd\x1ftoken`; the value is the full
    /// object name, or `None` for a hex-shaped word that names nothing. Unlike `verified`, the
    /// misses ARE cached: history is append-only, so a word that is not an object now will not
    /// become one, and every uncached lookup is a `git rev-parse` subprocess per hover.
    commits: HashMap<String, Option<String>>,
    /// Names this pane looked for across the whole repository, after the pane's own cwd could
    /// not place them, keyed by `cwd\x1ftoken`. Both answers are cached, misses included: the
    /// lookup is a `git ls-files` subprocess, and a hover that crosses a paragraph of prose
    /// would otherwise fire one per word that happens to end in `.py`. The price is a file
    /// created *after* it was first mentioned staying dark until the pane's cwd changes —
    /// paid only by this fallback, never by a path the cwd can resolve on its own.
    found: HashMap<String, Option<ResolveResult>>,
    /// The live drag-selection, if any (our own cell-range model — see [`crate::selection`]).
    /// `None` until a press starts one; a non-dragged selection (a plain click) is held but
    /// renders nothing, so the same press can still resolve to a link click.
    selection: Option<Selection>,
    /// The (logical-px) point of the active selection press, used to gate a real drag from a click
    /// twitch: the head only starts tracking once the pointer moves past [`DRAG_THRESHOLD_PX`] from
    /// here. `None` when no press is in flight.
    select_origin: Option<(f32, f32)>,
    /// System clipboard handle for copy-on-select / right-click paste (kept open for the pane's
    /// life — see [`crate::clipboard`]).
    clipboard: Clipboard,
    /// The transient copy/paste indicator ("toast") + when it was raised; auto-expires after
    /// [`TOAST_MS`]. Drained by [`toast_text`](Self::toast_text).
    toast: Option<(String, Instant)>,
    /// Whether the in-pane search box (Ctrl+F) is open.
    search_shown: bool,
    /// The current search query (the search box text).
    search_query: String,
    /// All matches for `search_query` across the grid + scrollback, top to bottom.
    search_matches: Vec<Match>,
    /// Index into `search_matches` of the active (highlighted/revealed) match, if any.
    search_index: Option<usize>,
    /// When the scrollback viewport was last moved by an explicit scroll gesture (wheel /
    /// Shift+PageUp/Down / scroll-to-edge). Drives the vim-style scrollbar's show-then-fade: the
    /// bar is opaque for [`SCROLLBAR_SHOW_MS`], fades over [`SCROLLBAR_FADE_MS`], then is hidden.
    /// `None` once it has fully faded. NOT stamped by the keystroke-driven snap-to-bottom, so the
    /// bar never flashes while you type.
    scroll_activity: Option<Instant>,
    /// The live drag-selection pointer `(x, y, surf_w, surf_h)` in logical px while the button is
    /// held, for edge-autoscroll: when it sits in the top/bottom edge band, the pump's
    /// [`selection_autoscroll_tick`](Self::selection_autoscroll_tick) scrolls the viewport and
    /// grows the selection into off-screen scrollback. `None` when no drag is in flight.
    drag_pointer: Option<(f32, f32, f32, f32)>,
}

/// How long the scrollbar stays fully opaque after a scroll gesture before it begins to fade.
const SCROLLBAR_SHOW_MS: u128 = 900;
/// How long the scrollbar takes to fade out once the show window elapses.
const SCROLLBAR_FADE_MS: u128 = 350;
/// Minimum scrollbar thumb height (logical px) so it stays grabbable/visible on a huge buffer.
const SCROLLBAR_MIN_THUMB_PX: f32 = 24.0;
/// Lines scrolled per wheel notch (mirrors the widget's `scroll-requested(±3)` magnitude) — used
/// to collapse a notch back to one mouse-wheel report when forwarding to a mouse-grabbing app.
const WHEEL_LINES_PER_NOTCH: i32 = 3;

/// A link under the cursor — a path, an http/https URL, or a commit hash: where to draw the hover
/// underline (in the pane's *logical* pixel space) plus the target. Returned by
/// [`TerminalPane::link_at`], which only ever hands back paths that exist, and by
/// [`TerminalPane::link_target_at`], which does not.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkHit {
    /// Underline rect in logical px within the pane surface.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    /// Absolute path the link points at — the URL itself when [`is_url`](Self::is_url), or the
    /// full 40-character object name when [`is_commit`](Self::is_commit).
    pub abs_path: String,
    pub line: Option<u32>,
    pub col: Option<u32>,
    /// Tooltip label (`abs_path` with any `:line[:col]` suffix appended; the URL verbatim).
    pub tip: String,
    /// `true` for an http/https URL (routed by the app, never disk-verified).
    pub is_url: bool,
    /// `true` for a commit hash git confirmed exists in this pane's repository. `abs_path` then
    /// holds the full object name and `commit_cwd` the directory the lookup ran in — the app
    /// needs both to load the commit back out of the right repository.
    pub is_commit: bool,
    /// The directory the commit was resolved against, for an [`is_commit`](Self::is_commit) hit.
    pub commit_cwd: String,
    /// Whether the path was found on disk. Always `true` for a hover hit and for a URL; only a
    /// [`link_target_at`](TerminalPane::link_target_at) lookup can report `false`. Copying a path
    /// works either way — a build log naming a file that failed to generate is exactly when the
    /// path is worth having — but revealing one that isn't there has nothing to show.
    pub exists: bool,
}

/// The outcome of activating (clicking) a link: a plain click reveals the target (the left file
/// tree for a path, the app's browser preference for a URL), Ctrl/Cmd-click copies it.
///
/// Every variant hands the target *back* rather than acting on it. The widget cannot see the
/// left panel or the browser preference, and a widget that shelled out to the OS default would
/// silently bypass both.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkAction {
    /// Ctrl/Cmd-click — the caller should copy this absolute path (or URL) to the clipboard.
    Copy(String),
    /// Plain click on a path — the caller should reveal it in the left file tree, from where the
    /// human picks what to do with it (open in an editor pane, a terminal, a viewer). Opening it
    /// straight into whatever the OS thinks owns the extension takes that choice away.
    Reveal {
        path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
    /// Plain click on a URL — the caller should route it through Preferences → Browser (the OS
    /// default, one chosen browser, or the "ask each time" chooser).
    OpenUrl(String),
    /// Plain click on a commit hash — the caller should show that commit in the left git panel,
    /// from where the human can read the message, see the diff, or act on any file it touched.
    /// `cwd` is the directory the hash resolved in, which is what says *which* repository.
    ShowCommit { cwd: String, hash: String },
}

impl TerminalPane {
    /// Create a pane of `cols`×`rows` cells driving the given renderer. Use
    /// [`crate::render::SoftwareRenderer`] (always available) or
    /// [`crate::render::GpuRenderer`] (when a wgpu device is in hand).
    pub fn new(cols: usize, rows: usize, renderer: Box<dyn PaneRenderer>) -> Self {
        Self {
            grid: TermGrid::new(cols, rows),
            renderer,
            cwd: None,
            verified: HashMap::new(),
            commits: HashMap::new(),
            found: HashMap::new(),
            selection: None,
            select_origin: None,
            clipboard: Clipboard::new(),
            toast: None,
            search_shown: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_index: None,
            scroll_activity: None,
            drag_pointer: None,
        }
    }

    /// Feed a chunk of session output (the `data` of a `SessionEvent::Data`) into the grid.
    pub fn feed(&mut self, data: &str) {
        self.grid.feed(data.as_bytes());
    }

    /// Feed raw output bytes (when you have bytes rather than a decoded `String`).
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        self.grid.feed(bytes);
    }

    /// Drain terminal-originated replies (DSR/DA/etc.) that must be written back to the
    /// session's pty. Empty when there is nothing to forward. **Must** be forwarded or a
    /// real conpty blocks at startup (see [`TermGrid::take_replies`]).
    pub fn take_replies(&mut self) -> Vec<u8> {
        self.grid.take_replies()
    }

    /// Resize the pane's grid. Returns `true` if the cell dimensions changed — in which
    /// case the caller should also `SessionManager::resize` the bound session.
    pub fn resize(&mut self, cols: usize, rows: usize) -> bool {
        self.grid.resize(cols, rows)
    }

    /// Take-and-clear the repaint flag. `true` means the grid changed since the last call.
    pub fn take_dirty(&mut self) -> bool {
        self.grid.take_dirty()
    }

    /// Render the current grid to a `slint::Image` at the pane's *physical* pixel
    /// resolution (`cols*cell_w × rows*cell_h`). Cheap to call repeatedly — the renderer
    /// caches its buffers/atlas — but gate it on [`take_dirty`](Self::take_dirty) plus the
    /// cursor blink for minimal CPU.
    pub fn render(&mut self, font: &mut Font, opts: &RenderOpts) -> Image {
        let snap = self.grid.snapshot();
        self.renderer.render(&snap, font, opts)
    }

    /// Current grid size in `(cols, rows)`.
    pub fn grid_size(&self) -> (usize, usize) {
        let s = self.grid.size();
        (s.cols, s.rows)
    }

    /// The visible screen as plain text — one line per viewport row (blank cells as spaces),
    /// with trailing whitespace and trailing blank lines trimmed. Lets a host feed an ambient
    /// summariser the *rendered* screen (what the user actually sees) instead of the raw redraw
    /// byte stream, so a continuously-repainting TUI (e.g. an agent CLI) is captured cleanly
    /// rather than as redraw noise.
    pub fn screen_text(&self) -> String {
        let snap = self.grid.snapshot();
        let mut lines: Vec<String> = (0..snap.rows)
            .map(|r| Self::row_text(&snap, r).trim_end().to_string())
            .collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// A human-readable name for the active renderer (e.g. for a HUD/debug overlay).
    pub fn renderer_name(&self) -> &'static str {
        self.renderer.name()
    }

    /// Apply a colour theme: override the 16 base ANSI colours (index 0 = default background,
    /// 7 = default foreground). See [`crate::grid::TermGrid::set_base16`].
    pub fn set_palette(&mut self, base: [[u8; 3]; 16]) {
        self.grid.set_base16(base);
    }

    /// Swap the renderer at runtime (e.g. GPU↔software on a device-lost / RDP transition).
    /// The next [`render`](Self::render) rebuilds from the live grid, so the swap is seamless.
    pub fn set_renderer(&mut self, renderer: Box<dyn PaneRenderer>) {
        self.renderer = renderer;
    }

    // ---- Clickable file paths --------------------------------------------------------------
    //
    // Plain click opens the file (editor / OS default); Ctrl/Cmd-click copies the resolved
    // absolute path. Paths are verified on disk (against this pane's cwd) before they linkify,
    // so prose tokens don't light up. The grid extraction lives in [`crate::links`]; resolve +
    // open live in [`hyperpanes_core::paths`]. This is the renderer-side glue (Spike's
    // `Terminal.tsx` link provider, ported to the cell grid).

    /// Set this pane's working directory (the base for resolving relative path tokens). Clearing
    /// or changing it drops the verify cache, since the same token can resolve elsewhere.
    pub fn set_cwd(&mut self, cwd: Option<String>) {
        if cwd != self.cwd {
            self.cwd = cwd;
            self.verified.clear();
            self.commits.clear();
            self.found.clear();
        }
    }

    /// The logical-px cell size for a surface of `surf_w`×`surf_h`, or `None` for a degenerate
    /// pane.
    ///
    /// `surf_w`/`surf_h` must be the size of the RENDERED IMAGE (`cols*cell_w × rows*cell_h`,
    /// converted to logical px), not of the widget body it sits in. The widget once stretched
    /// the image over the body with `image-fit: fill`, when the two were interchangeable; it
    /// now pins the image 1:1 at its source resolution for crisp glyphs, so the body is up to
    /// a cell wider and a row taller. Passing the body divides that slack back into every cell,
    /// and because the error is multiplied by the column index it reaches a whole cell by the
    /// right-hand edge — the pointer selects one glyph left of the one under it.
    fn cell_logical(&self, surf_w: f32, surf_h: f32) -> Option<(f32, f32, usize, usize)> {
        let (cols, rows) = self.grid_size();
        if cols == 0 || rows == 0 || surf_w <= 0.0 || surf_h <= 0.0 {
            return None;
        }
        Some((surf_w / cols as f32, surf_h / rows as f32, cols, rows))
    }

    /// Reconstruct one viewport row's text (one char per column; blanks as spaces) so the
    /// `links` extractor's column indices line up with cells. Exact for ASCII paths.
    fn row_text(snap: &crate::grid::GridSnapshot, row: usize) -> String {
        (0..snap.cols)
            .map(|col| {
                let ch = snap.cell(col, row).ch;
                if ch == '\0' {
                    ' '
                } else {
                    ch
                }
            })
            .collect()
    }

    /// The hovered cell's *logical* line: viewport rows joined across soft wraps, the hovered
    /// cell's index into that text, and the first row of the run.
    ///
    /// A URL or path wider than the pane is one logical token that alacritty stores across two
    /// viewport rows, with `WRAPLINE` on the first. Extracting per visual row cuts it in half
    /// and the half under the cursor is what gets opened -- a link reading
    /// `http://127.0.0.1:51551/row18` opened `http://127.0.0.1:51`. The extractor in
    /// [`crate::links`] was always written against a wrap-joined line (see `cell_from_index`);
    /// this is the join it was waiting for.
    fn logical_line(
        &self,
        snap: &crate::grid::GridSnapshot,
        row: usize,
        col: usize,
    ) -> Option<(String, usize, usize)> {
        if row >= snap.rows || col >= snap.cols {
            return None;
        }
        let mut first = row;
        while first > 0 && self.grid.row_wraps(first - 1) {
            first -= 1;
        }
        let mut last = row;
        while last + 1 < snap.rows && self.grid.row_wraps(last) {
            last += 1;
        }
        let mut text = String::with_capacity((last - first + 1) * snap.cols);
        for r in first..=last {
            text.push_str(&Self::row_text(snap, r));
        }
        Some((text, (row - first) * snap.cols + col, first))
    }

    /// The part of a logical-line span `[start, end)` that falls on visual `row` of a wrap run
    /// beginning at `first`, as columns of that row. The underline is one rect per [`LinkHit`],
    /// so a token spanning two rows underlines the row the cursor is actually on.
    fn row_segment(
        start: usize,
        end: usize,
        row: usize,
        first: usize,
        cols: usize,
    ) -> (usize, usize) {
        let off = (row - first) * cols;
        (
            start.saturating_sub(off).min(cols),
            (end.min(off + cols)).saturating_sub(off),
        )
    }

    fn cache_key(&self, token: &str) -> String {
        format!("{}\u{1f}{}", self.cwd.as_deref().unwrap_or(""), token)
    }

    /// Find a path token under the (logical-px) point, returning the resolved record, the
    /// candidate's column span, and the cell metrics. Resolution is cached per (cwd, token);
    /// only existing paths are cached, so freshly-created files linkify on a later hover.
    ///
    /// `require_exists` is what separates the two callers. Hovering underlines a path only when
    /// it is really there, because an underline is a promise. Copying makes no such promise, and
    /// the token a human most wants off their screen is often one that does *not* exist yet — a
    /// compiler naming the output it failed to write, a traceback from another machine.
    fn locate(
        &mut self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
        require_exists: bool,
    ) -> Option<(
        ResolveResult,
        Option<u32>,
        Option<u32>,
        usize,
        usize,
        usize,
        f32,
        f32,
    )> {
        let (cell_w, cell_h, cols, rows) = self.cell_logical(surf_w, surf_h)?;
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let col = (x / cell_w) as usize;
        let row = (y / cell_h) as usize;
        if col >= cols || row >= rows {
            return None;
        }

        let snap = self.grid.snapshot();
        let (text, idx, first) = self.logical_line(&snap, row, col)?;
        let mut cand = extract_path_candidates(&text)
            .into_iter()
            .find(|c| idx >= c.start && idx < c.end)?;

        // A path with a space in it reaches here in pieces, because whitespace is the only
        // token boundary an unquoted line offers. Only disk can say where such a path ends, so
        // the repair happens here and not in `links`, which is deliberately diskless.
        if let Some(wide) = self.widen_across_spaces(&text, &cand) {
            cand = wide;
        }

        // A candidate that runs to the last column of a run that does NOT wrap was cut off by
        // the program that printed it — it chose its own line width and emitted a hard newline
        // mid-path — and nothing in the grid records what came after. The danger is not the
        // missing link, it is that the surviving prefix can be a real directory: hovering
        // `/System/Volumes/Data` broken at the edge would offer to open `/System/Volumes`, and
        // a link that opens the wrong thing is worse than no link at all. So: refuse.
        let run_rows = text.chars().count() / snap.cols.max(1);
        if cand.end >= text.chars().count()
            && run_rows > 0
            && !self.grid.row_wraps(first + run_rows - 1)
        {
            return None;
        }

        let key = self.cache_key(&cand.path);
        let resolved = match self.verified.get(&key) {
            Some(hit) => hit.clone(),
            None => {
                let r = paths::resolve_path(self.cwd.as_deref(), &cand.path);
                if r.exists {
                    self.verified.insert(key, r.clone());
                    r
                } else if let Some(found) = self.elsewhere(&cand.path) {
                    // Not where this pane is standing, but somewhere in its repository — and
                    // only one such file, or `elsewhere` would have declined to guess.
                    self.verified.insert(key, found.clone());
                    found
                } else if require_exists {
                    return None;
                } else {
                    // A miss is never cached: a file that appears a second later has to
                    // linkify on the next hover rather than stay dark for the life of the pane.
                    r
                }
            }
        };
        // The candidate's line/col rides out with it: re-extracting to recover them would have
        // to rebuild the same joined line, and the span it matched on is a logical index now.
        let (start, end) = Self::row_segment(cand.start, cand.end, row, first, snap.cols);
        Some((
            resolved, cand.line, cand.col, start, end, row, cell_w, cell_h,
        ))
    }

    /// Grow `cand` across single spaces, in both directions, while the result keeps naming a
    /// real file.
    ///
    /// `Artifact(/Users/bshuler/Library/Application Support/hyperpanes/lane-watch.html)` is one
    /// path, but nothing in the *text* says so: the space between `Application` and `Support` is
    /// indistinguishable from the space between two words of prose. Where such a path starts and
    /// ends is not a question the characters can answer — only `stat` can — which is why this
    /// lives beside the grid and not in [`crate::links`], whose whole suite runs without
    /// touching a disk.
    ///
    /// Both directions, because the human hovers wherever the interesting part is: over the
    /// directory at the front on one line, over the filename at the back on the next. A widening
    /// that only grew rightwards would light up the first half of that path and leave the half
    /// with the filename in it dark, which is the harder failure to explain.
    ///
    /// So: collect the word boundaries reachable on either side, and keep the LONGEST span that
    /// exists. Longest rather than first, because `…/Application Support` may itself be a real
    /// directory on the way to the file that was actually meant.
    ///
    /// Three things bound it. The seed must already have failed to resolve, so a path that
    /// stands on its own is never touched. A grown *start* must land on a path root (`/`, `~/`,
    /// `./`, `C:\`) — the shape a program prints when it announces a file, and one prose does
    /// not have. And a grown span is only ever accepted if it stats true, so the failure mode is
    /// "no link", never "wrong link".
    fn widen_across_spaces(&self, text: &str, cand: &PathCandidate) -> Option<PathCandidate> {
        /// Four words each way covers `Application Support` and the deepest spaced name seen in
        /// the wild; past that the odds tilt towards swallowing the prose around the path.
        const MAX_WORDS: usize = 4;
        /// A hard stop for a pathological line of single-character words.
        const MAX_CHARS: usize = 200;

        if paths::resolve_path(self.cwd.as_deref(), &cand.path).exists {
            return None;
        }
        let chars: Vec<char> = text.chars().collect();

        // Where the path could START: here, or at the root marker inside any of the few words
        // to the left. Leftmost root within each word, not rightmost — the leading `/` of
        // `Artifact(/Users/bshuler/Library/Application`, not the one before `Application`.
        let mut starts = vec![cand.start];
        let mut word = cand.start;
        for _ in 0..MAX_WORDS {
            if word == 0 || chars[word - 1] != ' ' {
                break;
            }
            let mut b = word - 1;
            while b > 0 && chars[b - 1] != ' ' {
                b -= 1;
            }
            if let Some(k) = (b..word - 1).find(|&i| is_path_root(&chars[i..])) {
                starts.push(k);
            }
            word = b;
        }

        // Where it could END: here, or at the end of any of the few words to the right, with
        // that word's sentence punctuation trimmed — the `)` closing `Artifact(`, the full stop
        // ending the line — so the span stops where the name does.
        let mut ends = vec![cand.end];
        let mut e = cand.end;
        for _ in 0..MAX_WORDS {
            if chars.get(e) != Some(&' ') {
                break;
            }
            let ws = e + 1;
            let mut we = ws;
            while we < chars.len() && chars[we] != ' ' {
                we += 1;
            }
            if we == ws {
                break;
            }
            let w: String = chars[ws..we].iter().collect();
            ends.push(we - (w.chars().count() - trim_trailing_punct(&w).chars().count()));
            e = we;
        }

        let mut best: Option<PathCandidate> = None;
        for &st in &starts {
            for &en in &ends {
                // The seed itself is the one pair already known to miss.
                if (st, en) == (cand.start, cand.end) || en <= st || en - st > MAX_CHARS {
                    continue;
                }
                if best.as_ref().is_some_and(|b| en - st <= b.end - b.start) {
                    continue;
                }
                let path: String = chars[st..en].iter().collect();
                if paths::resolve_path(self.cwd.as_deref(), &path).exists {
                    best = Some(PathCandidate {
                        path,
                        line: cand.line,
                        col: cand.col,
                        start: st,
                        end: en,
                    });
                }
            }
        }
        best
    }

    /// The repository-wide fallback for a name the pane's own cwd could not place.
    ///
    /// A coding session says `b99_price.py` and means a file it is not standing next to. The
    /// repository is the only corpus that makes that name an answer rather than a guess, and
    /// [`git::find_in_repo`] declines outright when more than one file could be meant.
    fn elsewhere(&mut self, token: &str) -> Option<ResolveResult> {
        let cwd = self.cwd.clone()?;
        let key = self.cache_key(token);
        if let Some(hit) = self.found.get(&key) {
            return hit.clone();
        }
        let r = git::find_in_repo(std::path::Path::new(&cwd), token).and_then(|abs| {
            let abs_path = abs.to_string_lossy().into_owned();
            // Listed by git is not the same as present on disk — an index entry outlives a
            // `rm`. The link means "open this", so it has to be there.
            let md = std::fs::metadata(&abs).ok()?;
            Some(ResolveResult {
                token: token.to_string(),
                is_exe: paths::is_executable_ext(&abs_path),
                is_dir: md.is_dir(),
                exists: true,
                abs_path,
            })
        });
        self.found.insert(key, r.clone());
        r
    }

    /// Find an http/https URL under the (logical-px) point, returning the candidate, its row,
    /// and the cell metrics. URLs linkify on shape alone — no disk/network verification (so no
    /// cache either; extraction per hover is cheap, same as the path re-extract in `link_at`).
    fn url_under(
        &self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
    ) -> Option<(UrlCandidate, usize, usize, usize, f32, f32)> {
        let (cell_w, cell_h, cols, rows) = self.cell_logical(surf_w, surf_h)?;
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let col = (x / cell_w) as usize;
        let row = (y / cell_h) as usize;
        if col >= cols || row >= rows {
            return None;
        }
        let snap = self.grid.snapshot();
        let (text, idx, first) = self.logical_line(&snap, row, col)?;
        let cand = extract_url_candidates(&text)
            .into_iter()
            .find(|c| idx >= c.start && idx < c.end)?;
        let (start, end) = Self::row_segment(cand.start, cand.end, row, first, snap.cols);
        Some((cand, start, end, row, cell_w, cell_h))
    }

    /// Find a commit hash under the (logical-px) point, returning the full object name git
    /// resolved it to, the candidate's column span, and the cell metrics.
    ///
    /// The shape gate in [`crate::links`] is deliberately loose, so git is the real one: a
    /// hex-shaped word only linkifies once `rev-parse` confirms this repository holds a commit by
    /// that name. That costs one subprocess the first time a given word is hovered in a given
    /// cwd, and nothing afterwards — both hits and misses are cached, because history does not
    /// un-write itself.
    fn commit_under(
        &mut self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
    ) -> Option<(String, String, usize, usize, usize, f32, f32)> {
        // No cwd means no repository to ask. Unlike a path, a commit has no sensible fallback:
        // resolving it against the home directory would answer for whatever repo happens to be
        // there, which is never the one the text came from.
        let cwd = self.cwd.clone()?;
        let (cell_w, cell_h, cols, rows) = self.cell_logical(surf_w, surf_h)?;
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let col = (x / cell_w) as usize;
        let row = (y / cell_h) as usize;
        if col >= cols || row >= rows {
            return None;
        }
        let snap = self.grid.snapshot();
        let (text, idx, first) = self.logical_line(&snap, row, col)?;
        let cand = extract_commit_candidates(&text)
            .into_iter()
            .find(|c| idx >= c.start && idx < c.end)?;

        let key = self.cache_key(&cand.hash);
        let full = match self.commits.get(&key) {
            Some(hit) => hit.clone(),
            None => {
                let r = git::resolve_commit(std::path::Path::new(&cwd), &cand.hash);
                self.commits.insert(key, r.clone());
                r
            }
        }?;
        let (start, end) = Self::row_segment(cand.start, cand.end, row, first, snap.cols);
        Some((full, cwd, start, end, row, cell_w, cell_h))
    }

    /// Hit-test a (logical-px) hover point against the rendered grid. Returns the underline rect +
    /// target when the point is over an http/https URL or a path that exists on disk, else `None`.
    /// The candidate's `:line[:col]` is carried through (and shown in the tooltip), but only the
    /// resolved path is verified — mirroring the Electron link provider. URLs win when a token is
    /// both (a URL is path-shaped but never disk-verifies anyway).
    pub fn link_at(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) -> Option<LinkHit> {
        if let Some((cand, start, end, row, cell_w, cell_h)) = self.url_under(x, y, surf_w, surf_h)
        {
            return Some(LinkHit {
                x: start as f32 * cell_w,
                y: (row as f32 + 1.0) * cell_h - 1.0, // a hairline along the cell's baseline
                w: (end - start) as f32 * cell_w,
                tip: cand.url.clone(),
                abs_path: cand.url,
                line: None,
                col: None,
                is_url: true,
                is_commit: false,
                commit_cwd: String::new(),
                exists: true,
            });
        }
        self.path_hit(x, y, surf_w, surf_h, true)
            .or_else(|| self.commit_hit(x, y, surf_w, surf_h))
    }

    /// Hit-test for a link the human has *asked about* — a Ctrl/Cmd-click or a context menu —
    /// rather than merely hovered. Same geometry as [`link_at`](Self::link_at), minus the
    /// on-disk requirement: the returned [`LinkHit::exists`] says which kind it is, so a caller
    /// can copy any path-shaped token while still refusing to reveal one that isn't there.
    pub fn link_target_at(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) -> Option<LinkHit> {
        if let Some((cand, start, end, row, cell_w, cell_h)) = self.url_under(x, y, surf_w, surf_h)
        {
            return Some(LinkHit {
                x: start as f32 * cell_w,
                y: (row as f32 + 1.0) * cell_h - 1.0,
                w: (end - start) as f32 * cell_w,
                tip: cand.url.clone(),
                abs_path: cand.url,
                line: None,
                col: None,
                is_url: true,
                is_commit: false,
                commit_cwd: String::new(),
                exists: true,
            });
        }
        self.path_hit(x, y, surf_w, surf_h, false)
            .or_else(|| self.commit_hit(x, y, surf_w, surf_h))
    }

    /// The commit half of both hit-tests. It is the same for either caller: a hash git cannot
    /// resolve is not a target anybody can copy or open, so there is no "missing but wanted"
    /// case the way there is for a path.
    fn commit_hit(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) -> Option<LinkHit> {
        let (full, cwd, start, end, row, cell_w, cell_h) =
            self.commit_under(x, y, surf_w, surf_h)?;
        Some(LinkHit {
            x: start as f32 * cell_w,
            y: (row as f32 + 1.0) * cell_h - 1.0,
            w: (end - start) as f32 * cell_w,
            tip: format!("commit {}", &full[..full.len().min(12)]),
            abs_path: full,
            line: None,
            col: None,
            is_url: false,
            is_commit: true,
            commit_cwd: cwd,
            exists: true,
        })
    }

    /// The path half of both hit-tests, differing only in whether a missing file still counts.
    fn path_hit(
        &mut self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
        require_exists: bool,
    ) -> Option<LinkHit> {
        let (r, line, col, start, end, row, cell_w, cell_h) =
            self.locate(x, y, surf_w, surf_h, require_exists)?;

        let tip = match (line, col) {
            (Some(l), Some(c)) => format!("{}:{}:{}", r.abs_path, l, c),
            (Some(l), None) => format!("{}:{}", r.abs_path, l),
            _ => r.abs_path.clone(),
        };
        Some(LinkHit {
            x: start as f32 * cell_w,
            y: (row as f32 + 1.0) * cell_h - 1.0, // a hairline along the cell's baseline
            w: (end - start) as f32 * cell_w,
            abs_path: r.abs_path,
            line,
            col,
            tip,
            is_url: false,
            is_commit: false,
            commit_cwd: String::new(),
            exists: r.exists,
        })
    }

    /// Activate the link under a (logical-px) click. `ctrl` (Ctrl or Cmd held) copies the
    /// absolute path — whether or not it exists — while a plain click asks the caller to reveal
    /// it, which only a path that is really there can satisfy. `None` when the click wasn't over
    /// a link at all.
    pub fn activate_link(
        &mut self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
        ctrl: bool,
    ) -> Option<LinkAction> {
        // Suppress link open/copy at the end of a drag-selection. The widget fires
        // `selection-end` then `link-activated` on the same left-button release, and a dragged
        // selection is kept alive (copied, not cleared) by the shell — so a release that just
        // finished selecting text must NOT also open/copy the link under the release point. A
        // plain click begins a fresh, non-dragged selection, so this never blocks real clicks.
        if self.selection_is_drag() {
            return None;
        }
        // Ctrl/Cmd-click looks up the ungated way: a path that failed to exist never underlined,
        // but the human aimed at it deliberately and wants the text.
        let hit = if ctrl {
            self.link_target_at(x, y, surf_w, surf_h)?
        } else {
            self.link_at(x, y, surf_w, surf_h)?
        };
        if ctrl {
            // Copy here with the pane's own (arboard) clipboard handle — the proven path the
            // selection copy uses — instead of relying on the caller: the app shell's `clip`
            // shell-out failed silently in live testing. The action is still returned so the
            // caller can do its (now best-effort, redundant) copy and any follow-up UX.
            if self.clipboard.copy(&hit.abs_path) {
                self.set_toast(format!(
                    "Copied {} to clipboard",
                    if hit.is_url {
                        "link"
                    } else if hit.is_commit {
                        "commit"
                    } else {
                        "path"
                    }
                ));
            }
            return Some(LinkAction::Copy(hit.abs_path));
        }
        if hit.is_url {
            // Handed back, not opened — see `LinkAction::OpenUrl`.
            return Some(LinkAction::OpenUrl(hit.abs_path));
        }
        if hit.is_commit {
            return Some(LinkAction::ShowCommit {
                cwd: hit.commit_cwd,
                hash: hit.abs_path,
            });
        }
        Some(LinkAction::Reveal {
            path: hit.abs_path,
            line: hit.line,
            col: hit.col,
        })
    }

    // ---- Text selection ---------------------------------------------------------------------
    //
    // Drag to select a cell range; the controller turns it into highlight rects (for the Slint
    // overlay) and, on release, the selected text (copy-on-select — see `copy_selection`). The
    // model is our own (`crate::selection`) rather than alacritty's, kept in viewport cells.

    /// The (clamped) viewport cell under a logical-px point. Unlike [`locate`](Self::locate),
    /// this never returns `None` for an in-pane drag that strays past an edge — it clamps to the
    /// nearest cell so a selection can run to the grid border.
    fn cell_at_clamped(&self, x: f32, y: f32, surf_w: f32, surf_h: f32) -> Option<selection::Cell> {
        let (cell_w, cell_h, cols, rows) = self.cell_logical(surf_w, surf_h)?;
        let col = (x / cell_w).floor().clamp(0.0, (cols - 1) as f32) as usize;
        let row = (y / cell_h).floor().clamp(0.0, (rows - 1) as f32) as usize;
        // Anchor to the ABSOLUTE grid line so the selection stays glued to its text as the viewport
        // scrolls: a viewport row `r` shows absolute line `r - display_offset`.
        let line = row as i32 - self.grid.display_offset() as i32;
        Some(selection::Cell { col, line })
    }

    /// Begin a drag-selection anchored at the (logical-px) press point. Replaces any prior
    /// selection. The selection only starts *rendering* once the drag leaves the anchor cell, so
    /// a click that doesn't move still falls through to a link activation.
    pub fn selection_begin(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) {
        self.selection = self
            .cell_at_clamped(x, y, surf_w, surf_h)
            .map(Selection::new);
        self.select_origin = Some((x, y));
        self.drag_pointer = Some((x, y, surf_w, surf_h));
    }

    /// Extend the active selection's head to the (logical-px) cursor point during a drag.
    ///
    /// Gated on [`DRAG_THRESHOLD_PX`]: until the pointer has moved that far from the press point
    /// the head stays pinned to the anchor (so the selection never becomes `dragged` and a click
    /// can't copy-on-select). This dead zone is what stops a stray click twitch — especially one
    /// that straddles a cell boundary — from clobbering the clipboard ahead of a paste.
    pub fn selection_update(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) {
        // Track the live pointer for edge-autoscroll even below the drag threshold (autoscroll
        // itself only kicks in once the selection is actually dragged).
        self.drag_pointer = Some((x, y, surf_w, surf_h));
        if let Some((ox, oy)) = self.select_origin {
            if (x - ox).hypot(y - oy) < DRAG_THRESHOLD_PX {
                return;
            }
        }
        let c = match self.cell_at_clamped(x, y, surf_w, surf_h) {
            Some(c) => c,
            None => return,
        };
        if let Some(sel) = self.selection.as_mut() {
            sel.update(c);
        }
    }

    /// Drop the current selection (and its highlight).
    pub fn selection_clear(&mut self) {
        self.selection = None;
        self.select_origin = None;
        self.drag_pointer = None;
    }

    /// End the drag (button released) — stops edge-autoscroll while KEEPING any selection (the
    /// controller may still copy it). Call from the `selection-end` handler.
    pub fn end_selection_drag(&mut self) {
        self.drag_pointer = None;
        self.select_origin = None;
    }

    /// One edge-autoscroll step while a selection drag is held in the top/bottom edge band: scroll
    /// the viewport one line toward that edge and extend the selection head to the just-revealed
    /// edge row, so the selection grows into off-screen scrollback (the vim/iTerm/Claude drag
    /// behavior). Returns `true` if it scrolled — the pump uses that to keep ticking + repainting.
    /// No-op unless a real (dragged) selection is in flight with the pointer at an edge.
    pub fn selection_autoscroll_tick(&mut self) -> bool {
        let (x, y, sw, sh) = match self.drag_pointer {
            Some(d) => d,
            None => return false,
        };
        if !self.selection.is_some_and(|s| s.dragged) {
            return false;
        }
        let (_, cell_h, _, _) = match self.cell_logical(sw, sh) {
            Some(t) => t,
            None => return false,
        };
        let edge = cell_h.max(8.0); // a one-row band (min 8px) at each edge
        let dir = if y < edge {
            1 // top edge → scroll up into history
        } else if y > sh - edge {
            -1 // bottom edge → scroll down toward the live edge
        } else {
            return false;
        };
        let before = self.grid.display_offset();
        self.scroll_by(dir);
        if self.grid.display_offset() == before {
            return false; // clamped at the top/bottom of the buffer
        }
        // Re-map the head to the edge row at the NEW offset → the line just scrolled into view.
        let edge_y = if dir > 0 { 0.0 } else { sh };
        if let Some(c) = self.cell_at_clamped(x, edge_y, sw, sh) {
            if let Some(sel) = self.selection.as_mut() {
                sel.update(c);
            }
        }
        true
    }

    /// Select the entire viewport (every visible cell), marked `dragged` so it renders and is
    /// copyable. This is the context menu's "Select All" — viewport-scoped (the region
    /// [`selection_text`](Self::selection_text) can reconstruct), mirroring xterm's `selectAll`
    /// over the on-screen buffer. A subsequent [`copy_selection`](Self::copy_selection) copies it.
    pub fn select_all(&mut self) {
        let (cols, rows) = self.grid_size();
        if cols == 0 || rows == 0 {
            self.selection = None;
            return;
        }
        // The visible viewport, in absolute lines: top viewport row 0 is line `-offset`.
        let off = self.grid.display_offset() as i32;
        let mut sel = Selection::new(selection::Cell { col: 0, line: -off });
        sel.update(selection::Cell {
            col: cols - 1,
            line: rows as i32 - 1 - off,
        });
        self.selection = Some(sel);
    }

    /// Clear the screen **and** scrollback (the context menu's "Clear"), dropping any selection
    /// and pinning the viewport to the bottom. Feeds the ED escapes (erase display + erase
    /// scrollback) so it runs through the same parser path as live output — the native analog of
    /// xterm's `term.clear()`.
    pub fn clear(&mut self) {
        self.selection = None;
        self.grid.feed(b"\x1b[H\x1b[2J\x1b[3J");
        self.grid.scroll_to_bottom();
    }

    /// True once the active selection has actually been dragged across cells (i.e. it's a real
    /// selection, not a stationary click). The caller uses this to choose copy-vs-click on release.
    pub fn selection_is_drag(&self) -> bool {
        self.selection.is_some_and(|s| s.dragged)
    }

    /// True when there's an active *dragged* selection lying entirely on the cursor's own viewport
    /// row — i.e. over the live shell input line, the only row a terminal can safely treat as
    /// editable text. Scopes type-over-selection to the prompt line: typing over a selection here
    /// drops the highlight (you're replacing your own input), whereas a selection on any other row
    /// (scrollback / command output) isn't in the shell's buffer and is left untouched, so no
    /// speculative deletes are ever sent (no PTY corruption). False for no selection, a non-dragged
    /// click, a multi-row span, or when the cursor is scrolled out of view.
    pub fn selection_on_cursor_row(&self) -> bool {
        let sel = match &self.selection {
            Some(s) if s.dragged => s,
            _ => return false,
        };
        let (start, end) = sel.ordered();
        if start.line != end.line {
            return false; // a multi-row selection is never a single prompt line
        }
        // Compare in absolute lines: the cursor's viewport row maps to absolute `row - offset`.
        match self.grid.cursor_row() {
            Some(crow) => start.line == crow as i32 - self.grid.display_offset() as i32,
            None => false,
        }
    }

    /// Type-over selection: the byte sequence that ERASES the selected prompt-line text, to be
    /// written to the pty *before* a printable keystroke so the key replaces the selection.
    /// `None` (and no state change) unless the dragged selection lies on the cursor's own
    /// logical line — its visual row, or any rows soft-WRAPPED into it (a long shell input
    /// spanning several visual rows is still one editable line) — and the main screen is
    /// active; on `Some` the selection is also cleared.
    ///
    /// Safety model — the sequence is built ONLY from edit keys the line editor **clamps at the
    /// input-region boundaries** (left/right arrows, backspace, forward-delete are no-ops at the
    /// edges in PSReadLine/readline): if the selection overlaps the prompt decoration itself, the
    /// surplus moves/deletes simply do nothing — only the selected chars inside the editable
    /// input are removed, and nothing left of the input start can ever be touched. The
    /// alternate screen is excluded ([`TermGrid::alt_screen`](crate::grid::TermGrid::alt_screen)):
    /// there these bytes would be app commands (vim motions), not edits.
    ///
    /// Cell↔char caveat: counts are in grid cells, exact for ASCII (same wide-glyph caveat as
    /// [`selection_text`](Self::selection_text)).
    pub fn type_over_selection(&mut self) -> Option<Vec<u8>> {
        if self.grid.alt_screen() {
            return None;
        }
        let sel = match &self.selection {
            Some(s) if s.dragged => s,
            _ => return None,
        };
        let (start, end) = sel.ordered();
        let crow = self.grid.cursor_row()?;
        let cline = crow as i32 - self.grid.display_offset() as i32;
        let (cols, _) = self.grid_size();
        if cols == 0 {
            return None;
        }
        // The selection must lie on the SAME WRAPPED LOGICAL LINE as the cursor: every grid line
        // between the selection and the cursor (inclusive bounds, exclusive of the last) must carry
        // the WRAPLINE continuation flag. A single-line selection on the cursor's own line is the
        // degenerate case (empty range). This is what makes a long soft-wrapped shell input
        // editable across its visual rows, while a selection on a genuinely different line
        // (scrollback, command output) still declines.
        let lo = start.line.min(cline);
        let hi = end.line.max(cline);
        if (lo..hi).any(|r| !self.grid.line_wraps(r)) {
            return None;
        }
        // Linear cell offsets within the wrapped line: a wrapped row is always `cols` cells wide,
        // and the line editor's arrows/backspace walk straight through the wrap, so the single-row
        // arithmetic below holds verbatim in linear space. Absolute line numbers are fine — only
        // the differences between these offsets matter.
        let lin = |line: i32, col: usize| line * cols as i32 + col as i32;
        let s = lin(start.line, start.col);
        let e = lin(end.line, end.col);
        let c = lin(cline, self.grid.cursor_col());
        const LEFT: &[u8] = b"\x1b[D";
        const RIGHT: &[u8] = b"\x1b[C";
        const BS: &[u8] = &[0x7f];
        const FDEL: &[u8] = b"\x1b[3~"; // forward delete (DeleteChar)
        let mut bytes = Vec::new();
        let mut rep = |seq: &[u8], n: i32| {
            for _ in 0..n.max(0) {
                bytes.extend_from_slice(seq);
            }
        };
        if c > e {
            // Caret right of the selection (the common case — you selected text you just
            // typed): step left to the selection end, then backspace it away.
            rep(LEFT, c - (e + 1));
            rep(BS, e - s + 1);
        } else if c <= s {
            // Caret left of (or at) the selection: step right to its start, forward-delete it.
            rep(RIGHT, s - c);
            rep(FDEL, e - s + 1);
        } else {
            // Caret inside the selection: backspace the left part, forward-delete the rest.
            rep(BS, c - s);
            rep(FDEL, e - c + 1);
        }
        self.selection_clear();
        Some(bytes)
    }

    /// A plain click on the shell's input line → the arrow keys that walk the caret to the
    /// clicked cell, or `None` when the click cannot safely be read as "put the caret here".
    ///
    /// Every other terminal-embedded line editor (readline, PSReadLine, Claude Code's prompt)
    /// already lets you move the caret with the arrow keys; the only thing missing was a way to
    /// say where you want it without counting the characters yourself. A terminal cannot ask the
    /// application to move its caret — there is no such escape sequence — so it does the only
    /// thing it can: it presses the arrow key for you, the right number of times.
    ///
    /// Safety model, deliberately the same one [`type_over_selection`](Self::type_over_selection)
    /// already relies on:
    ///
    ///  * **Left/right arrows only**, and every line editor **clamps them at the input-region
    ///    boundaries**. So a click on the prompt decoration, on a box border, or past the end of
    ///    what you typed lands the caret at the nearest end of the editable text rather than
    ///    doing something destructive. Nothing outside the input can be touched, and no vertical
    ///    motion is emitted — up/down is history recall in most editors, which would be exactly
    ///    the surprise this is trying to avoid.
    ///  * **Alternate screen excluded**: there these bytes are application commands (vim
    ///    motions), not caret movement.
    ///  * **Mouse-grabbing apps excluded**: they asked for the click themselves and get it
    ///    verbatim, as before.
    ///  * **The click must land on the cursor's own wrapped logical line.** A click on
    ///    scrollback, on command output, or on a different row of a drawn box is not an edit
    ///    position, and declining is free — the click keeps its old meaning.
    ///
    /// Cell↔char caveat: counts are in grid cells, exact for ASCII (same wide-glyph caveat as
    /// [`selection_text`](Self::selection_text)).
    pub fn click_move_cursor(&self) -> Option<Vec<u8>> {
        if self.grid.alt_screen() || self.grid.mouse_mode() {
            return None;
        }
        // The press anchor IS the clicked cell; a dragged selection is a selection, not a click.
        let target = match &self.selection {
            Some(s) if !s.dragged => s.anchor,
            _ => return None,
        };
        let crow = self.grid.cursor_row()?;
        let cline = crow as i32 - self.grid.display_offset() as i32;
        let (cols, _) = self.grid_size();
        if cols == 0 {
            return None;
        }
        // Same wrapped-line test as `type_over_selection`: every grid line from the upper of the
        // two to (exclusive) the lower must carry the WRAPLINE continuation flag, so a soft-wrapped
        // input is one editable line while genuinely separate rows are not.
        let (lo, hi) = (target.line.min(cline), target.line.max(cline));
        if (lo..hi).any(|r| !self.grid.line_wraps(r)) {
            return None;
        }
        // Linear cell offsets within the wrapped line — a wrapped row is always `cols` wide and the
        // editor's arrows walk straight through the wrap, so this arithmetic holds across rows.
        let lin = |line: i32, col: usize| line * cols as i32 + col as i32;
        let steps = lin(target.line, target.col) - lin(cline, self.grid.cursor_col());
        if steps == 0 {
            return None; // clicked the caret's own cell: nothing to send
        }
        const LEFT: &[u8] = b"\x1b[D";
        const RIGHT: &[u8] = b"\x1b[C";
        let seq = if steps > 0 { RIGHT } else { LEFT };
        let mut bytes = Vec::with_capacity(steps.unsigned_abs() as usize * seq.len());
        for _ in 0..steps.abs() {
            bytes.extend_from_slice(seq);
        }
        Some(bytes)
    }

    /// Highlight rectangles (logical px) for the active *dragged* selection over a surface of
    /// `surf_w`×`surf_h`. Empty for no selection or a non-dragged click — so a plain click never
    /// leaves a stray one-cell highlight.
    pub fn selection_rects(&self, surf_w: f32, surf_h: f32) -> Vec<(f32, f32, f32, f32)> {
        let sel = match &self.selection {
            Some(s) if s.dragged => s,
            _ => return Vec::new(),
        };
        let (cell_w, cell_h, cols, rows) = match self.cell_logical(surf_w, surf_h) {
            Some(t) => t,
            None => return Vec::new(),
        };
        // Project the absolute-line selection back through the current scroll position so the
        // highlight rides the content (and clips at the viewport edges) as the user scrolls.
        let off = self.grid.display_offset() as i32;
        selection::selection_rects(sel, cols, cell_w, cell_h, off, rows)
    }

    /// The text covered by the active *dragged* selection, reconstructed from the grid snapshot
    /// (one char per cell, blanks as spaces, each line right-trimmed, rows joined by `\n`).
    /// `None` when there's no real selection. Exact for ASCII (the same wide-glyph caveat as the
    /// link extractor).
    pub fn selection_text(&self) -> Option<String> {
        let sel = match &self.selection {
            Some(s) if s.dragged => s,
            _ => return None,
        };
        let (start, end) = sel.ordered();
        let (cols, _) = self.grid_size();
        if cols == 0 {
            return None;
        }
        let last_col = cols - 1;
        // Read straight from the grid by ABSOLUTE line, so a selection anchored in scrollback (or
        // straddling the history/viewport boundary) reconstructs correctly regardless of scroll.
        let mut lines = Vec::new();
        for line_i in start.line..=end.line {
            let row_text = match self.grid.line_text(line_i) {
                Some(t) => t,
                None => continue, // line outside the buffer (shouldn't happen for a live selection)
            };
            let chars: Vec<char> = row_text.chars().collect();
            let col_start = if line_i == start.line { start.col } else { 0 };
            let col_end = if line_i == end.line {
                end.col
            } else {
                last_col
            };
            let mut line = String::new();
            for col in col_start..=col_end.min(last_col) {
                let ch = chars.get(col).copied().unwrap_or(' ');
                line.push(if ch == '\0' { ' ' } else { ch });
            }
            lines.push(line.trim_end().to_string());
        }
        let text = lines.join("\n");
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Copy the current selection to the system clipboard and raise a "Copied …" indicator
    /// (the copy-on-select behavior, also bound to Ctrl+C / Ctrl+Shift+C). Returns the number of
    /// characters copied, or `None` if there was no selection or the clipboard was unavailable.
    pub fn copy_selection(&mut self) -> Option<usize> {
        let text = self.selection_text()?;
        let n = text.chars().count();
        if self.clipboard.copy(&text) {
            self.set_toast(format!(
                "Copied {} char{} to clipboard",
                n,
                if n == 1 { "" } else { "s" }
            ));
            Some(n)
        } else {
            None
        }
    }

    /// Copy arbitrary `text` (a Ctrl+clicked link/path) to the system clipboard and raise the
    /// "Copied …" indicator — the same arboard instance + toast as
    /// [`copy_selection`](Self::copy_selection). Replaces the app's `clip.exe` shell-out, which
    /// blocked the UI thread on `child.wait()` for every Ctrl+click (a visible freeze) and
    /// showed no indicator.
    pub fn copy_text(&mut self, text: &str) -> bool {
        if self.clipboard.copy(text) {
            self.set_toast("Copied to clipboard");
            true
        } else {
            false
        }
    }

    /// Read the system clipboard for a right-click / Ctrl+V paste, raising a "Pasted …"
    /// indicator. Returns the text the caller should write to this pane's session (the controller
    /// doesn't own the session transport), or `None` when the clipboard is empty/unavailable.
    pub fn paste_from_clipboard(&mut self) -> Option<String> {
        let text = self.clipboard.paste()?;
        let n = text.chars().count();
        self.set_toast(format!(
            "Pasted {} char{}",
            n,
            if n == 1 { "" } else { "s" }
        ));
        Some(prepare_paste(&text, self.grid.bracketed_paste()))
    }

    /// Wrap `text` for insertion exactly as a paste would — bracketed-paste markers when
    /// the program in this pane asked for them, CR-normalized newlines otherwise.
    ///
    /// For text the app supplies itself rather than reading from the clipboard (the OS file
    /// drop). Sharing `prepare_paste` is the point: a TUI that distinguishes pasted content
    /// from typing must see a drop the same way it sees a paste, or a dropped path arrives
    /// as if it had been hand-typed one key at a time.
    pub fn prepare_insert(&self, text: &str) -> String {
        prepare_paste(text, self.grid.bracketed_paste())
    }

    /// Whether the OS clipboard holds an image (vs text). The controller uses this to decide
    /// whether a Ctrl+V with no clipboard text should forward a literal 0x16 to an in-pane TUI
    /// (Claude Code) that reads the clipboard image itself — see [`Clipboard::has_image`].
    pub fn clipboard_has_image(&mut self) -> bool {
        self.clipboard.has_image()
    }

    /// Pin the viewport back to the live edge (display offset 0) so the cursor is visible at the
    /// end of whatever was just written — e.g. after a paste, regardless of scrollback position.
    pub fn scroll_to_bottom(&mut self) {
        self.grid.scroll_to_bottom();
    }

    /// Scroll the scrollback viewport by `delta_lines` (positive = up into history, negative =
    /// toward the live edge), clamped to the history bounds. Stamps the scrollbar's show timer.
    pub fn scroll_by(&mut self, delta_lines: i32) {
        self.grid.scroll_by(delta_lines);
        self.scroll_activity = Some(Instant::now());
    }

    /// Scroll the scrollback viewport by one page (`up` = into history, else toward the live
    /// edge). A page is the visible row count less one row of overlap, so successive pages keep a
    /// line of context. Drives Shift+PageUp / Shift+PageDown.
    pub fn scroll_page(&mut self, up: bool) {
        let (_, rows) = self.grid_size();
        let page = (rows as i32 - 1).max(1);
        self.scroll_by(if up { page } else { -page });
    }

    /// Jump the viewport to the very top of scrollback (Shift+Home). Stamps the scrollbar timer.
    pub fn scroll_to_top(&mut self) {
        let (hist, _, off) = self.grid.scroll_metrics();
        if hist > off {
            self.scroll_by((hist - off) as i32);
        }
    }

    /// Handle a mouse-wheel notch over the pane. `delta_lines` is positive for wheel-up (into
    /// history) / negative for wheel-down, in scrollback lines (the widget sends ±3). Returns the
    /// bytes the controller should write to the pty when the wheel belongs to the **application**
    /// instead of our scrollback:
    ///
    /// * a **mouse-grabbing app** (DECSET 1000/1002/1003 — vim, htop, Claude Code) → a mouse-wheel
    ///   report at the pointer cell, so the app scrolls its own view;
    /// * the **alternate screen** with no mouse mode (less, man, a pager) → up/down arrow keys
    ///   (xterm's "alternate scroll"), so the pager scrolls a line at a time.
    ///
    /// Otherwise it scrolls our own scrollback viewport and returns `None`. This is the fix for
    /// "can't scroll Claude": in the alt screen there is no scrollback for `scroll_by` to move, so
    /// the wheel must be forwarded to the app.
    pub fn wheel(
        &mut self,
        delta_lines: i32,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
    ) -> Option<Vec<u8>> {
        if delta_lines == 0 {
            return None;
        }
        if self.grid.mouse_mode() {
            return Some(self.mouse_wheel_report(delta_lines, x, y, surf_w, surf_h));
        }
        if self.grid.alt_screen() {
            return Some(alt_scroll_arrows(delta_lines, self.grid.app_cursor()));
        }
        self.scroll_by(delta_lines);
        None
    }

    /// Build mouse-wheel report bytes for a mouse-grabbing app: one report per wheel notch (a notch
    /// is [`WHEEL_LINES_PER_NOTCH`] of `delta_lines`), encoded SGR (`ESC[<Cb;Cx;Cy M`) when the app
    /// asked for it (DECSET 1006), else legacy X10 (`ESC[M` + 3 bytes). Button 64 = wheel up, 65 =
    /// down; the position is the 1-based cell under the pointer.
    /// 1-based `(col, row)` of the pointer at logical px `(x, y)` over a `surf_w`×`surf_h` surface,
    /// clamped into the grid. Shared by every mouse report.
    fn cell_1based(&self, x: f32, y: f32, surf_w: f32, surf_h: f32) -> (usize, usize) {
        match self.cell_logical(surf_w, surf_h) {
            Some((cw, ch, cols, rows)) => {
                let c = (x / cw).floor().clamp(0.0, (cols - 1) as f32) as usize + 1;
                let r = (y / ch).floor().clamp(0.0, (rows - 1) as f32) as usize + 1;
                (c, r)
            }
            None => (1, 1),
        }
    }

    /// Encode ONE mouse report: `cb` is the button/event code (motion already includes the +32
    /// motion bit; wheel is 64/65), `release` picks SGR final `m`/X10 button-3. SGR (`ESC[<cb;c;r
    /// M|m`) when the app asked for it (DECSET 1006), else legacy X10 (`ESC[M` + 3 bytes, each +32).
    fn fmt_mouse(&self, cb: u32, col: usize, row: usize, release: bool) -> Vec<u8> {
        if self.grid.sgr_mouse() {
            let term = if release { 'm' } else { 'M' };
            format!("\x1b[<{cb};{col};{row}{term}").into_bytes()
        } else {
            let b = if release { 3 } else { cb }; // X10 can't say which button released
            vec![
                0x1b,
                b'[',
                b'M',
                (b + 32).min(255) as u8,
                (col as u32 + 32).min(255) as u8,
                (row as u32 + 32).min(255) as u8,
            ]
        }
    }

    fn mouse_wheel_report(
        &self,
        delta_lines: i32,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
    ) -> Vec<u8> {
        let (col, row) = self.cell_1based(x, y, surf_w, surf_h);
        let cb: u32 = if delta_lines > 0 { 64 } else { 65 };
        let notches = (delta_lines.unsigned_abs() / WHEEL_LINES_PER_NOTCH as u32).max(1);
        let mut out = Vec::new();
        for _ in 0..notches {
            out.extend_from_slice(&self.fmt_mouse(cb, col, row, false));
        }
        out
    }

    /// Whether the application has grabbed the mouse (DECSET 1000/1002/1003). When true the
    /// controller forwards button/drag/release events to the app (its own selection/clicks) instead
    /// of doing a local terminal selection — unless Shift is held (see [`mouse_report`]).
    ///
    /// [`mouse_report`]: Self::mouse_report
    pub fn app_grabs_mouse(&self) -> bool {
        self.grid.mouse_mode()
    }

    /// Bytes to forward to the pty for a pointer event over a mouse-grabbing app, or `None` to
    /// suppress it. `kind`: 0 = button press, 1 = motion, 2 = release. `button`: 0 = left, 1 =
    /// middle, 2 = right, 3 = none (a bare move). Motion is only reported when the app asked for it
    /// (1002 = while a button is held, 1003 = always); press/release always report. Returns `None`
    /// if the app isn't grabbing the mouse.
    pub fn mouse_report(
        &self,
        kind: i32,
        button: i32,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
    ) -> Option<Vec<u8>> {
        if !self.grid.mouse_mode() {
            return None;
        }
        let (col, row) = self.cell_1based(x, y, surf_w, surf_h);
        let btn = button.clamp(0, 3) as u32;
        match kind {
            0 => Some(self.fmt_mouse(btn, col, row, false)), // press
            2 => Some(self.fmt_mouse(btn.min(2), col, row, true)), // release
            1 => {
                // Motion: forward on any-motion (1003), or on drag (1002) only while a button is held.
                let held = button != 3;
                let want = self.grid.mouse_any_motion() || (self.grid.mouse_drag() && held);
                if !want {
                    return None;
                }
                let mb = if held { btn } else { 3 };
                Some(self.fmt_mouse(32 + mb, col, row, false))
            }
            _ => None,
        }
    }

    // ---- scroll overlays (vim scrollbar + jump-to-bottom HUD) --------------------------------

    /// How far the viewport is scrolled up from the live edge, in lines (0 = pinned to the bottom).
    /// Drives the jump-to-bottom HUD: shown whenever this is non-zero.
    pub fn scroll_offset(&self) -> usize {
        self.grid.scroll_metrics().2
    }

    /// The vim-style scrollbar to draw right now, or `None` when there is no scrollback or the bar
    /// has fully faded. Returns `(thumb_y, thumb_h, opacity)` in logical px over a `surf_h`-tall
    /// pane: the thumb height is proportional to the visible fraction of the buffer, its position to
    /// how far down the buffer the viewport sits, and the opacity ramps from 1 down to 0 over the
    /// show-then-fade window since the last scroll gesture (so the bar is invisible while idle).
    pub fn scrollbar(&self, surf_h: f32) -> Option<(f32, f32, f32)> {
        let opacity = self.scrollbar_opacity()?;
        let (hist, rows, off) = self.grid.scroll_metrics();
        if hist == 0 || rows == 0 || surf_h <= 0.0 {
            return None;
        }
        let total = (hist + rows) as f32;
        let thumb_h = (surf_h * rows as f32 / total)
            .max(SCROLLBAR_MIN_THUMB_PX)
            .min(surf_h);
        // Fraction of the way down the buffer the viewport TOP sits: 0 at the very top of history,
        // 1 at the live edge. `hist - off` lines sit above the viewport top, out of `hist` total.
        let frac = if hist == 0 {
            1.0
        } else {
            (hist - off) as f32 / hist as f32
        };
        let thumb_y = (surf_h - thumb_h) * frac;
        Some((thumb_y, thumb_h, opacity))
    }

    /// Current scrollbar opacity from the show-then-fade timer, or `None` once it has fully faded
    /// (so the projection can drop the bar entirely while idle).
    fn scrollbar_opacity(&self) -> Option<f32> {
        let e = self.scroll_activity?.elapsed().as_millis();
        if e < SCROLLBAR_SHOW_MS {
            Some(1.0)
        } else if e < SCROLLBAR_SHOW_MS + SCROLLBAR_FADE_MS {
            Some(1.0 - (e - SCROLLBAR_SHOW_MS) as f32 / SCROLLBAR_FADE_MS as f32)
        } else {
            None
        }
    }

    // ---- Copy/paste indicator ("toast") -----------------------------------------------------

    /// Raise a transient indicator over the pane (e.g. "Copied 12 chars to clipboard"). It
    /// auto-expires after [`TOAST_MS`]; poll it each frame with [`toast_text`](Self::toast_text).
    pub fn set_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// The indicator text to display right now, or `None` once it has expired (which also clears
    /// it). Call this every frame and push the result to the pane's `toast` property.
    pub fn toast_text(&mut self) -> Option<String> {
        let expired = match &self.toast {
            Some((_, at)) => at.elapsed().as_millis() >= TOAST_MS,
            None => return None,
        };
        if expired {
            self.toast = None;
            return None;
        }
        self.toast.as_ref().map(|(m, _)| m.clone())
    }

    // ---- In-pane search (Ctrl+F) ------------------------------------------------------------
    //
    // Open a search box, type to find/highlight matches across the grid + scrollback, and step
    // through them (Enter / Shift+Enter), revealing each by scrolling it into view. Mirrors the
    // xterm `@xterm/addon-search` wiring in the Electron `Terminal.tsx` / `SearchBox.tsx`.

    /// Open the search box (Ctrl+F). The query starts empty; type to search.
    pub fn search_open(&mut self) {
        self.search_shown = true;
    }

    /// Close the search box, dropping the query/matches and pinning the viewport back to the
    /// bottom (the live prompt).
    pub fn search_close(&mut self) {
        self.search_shown = false;
        self.search_query.clear();
        self.search_matches.clear();
        self.search_index = None;
        self.grid.scroll_to_bottom();
    }

    /// Whether the search box is open.
    pub fn search_is_open(&self) -> bool {
        self.search_shown
    }

    /// The current query text.
    pub fn search_query(&self) -> &str {
        &self.search_query
    }

    /// Set the query (find-as-you-type): recompute matches across the grid + scrollback, pick the
    /// match nearest the current viewport, and scroll it into view.
    pub fn search_set_query(&mut self, query: &str) {
        self.search_query = query.to_string();
        self.search_recompute();
        self.search_reveal_active();
    }

    /// Step to the next (`forward`) / previous match, wrapping around, and reveal it.
    pub fn search_step(&mut self, forward: bool) {
        if self.search_matches.is_empty() {
            return;
        }
        let i = self.search_index.unwrap_or(0);
        self.search_index = Some(search::step(i, self.search_matches.len(), forward));
        self.search_reveal_active();
    }

    /// `(current_1_based, total)` for the match counter — `(0, 0)` when there are no matches.
    pub fn search_count(&self) -> (usize, usize) {
        let total = self.search_matches.len();
        let cur = self.search_index.map(|i| i + 1).unwrap_or(0);
        (cur, total)
    }

    /// Highlight rectangles (logical px) for every match currently in the viewport, plus the
    /// active match's rect on its own (so the pane can draw it distinctly). Matches scrolled out
    /// of view are omitted.
    // pre-existing; deferred per repo lint policy (test.yml)
    #[allow(clippy::type_complexity)]
    pub fn search_view_rects(
        &self,
        surf_w: f32,
        surf_h: f32,
    ) -> (Vec<(f32, f32, f32, f32)>, Option<(f32, f32, f32, f32)>) {
        let (cell_w, cell_h, _cols, rows) = match self.cell_logical(surf_w, surf_h) {
            Some(t) => t,
            None => return (Vec::new(), None),
        };
        let off = self.grid.display_offset() as i32;
        let mut rects = Vec::new();
        let mut active = None;
        for (i, m) in self.search_matches.iter().enumerate() {
            let row = m.line + off;
            if row < 0 || row >= rows as i32 {
                continue;
            }
            let rect = (
                m.start as f32 * cell_w,
                row as f32 * cell_h,
                (m.end.saturating_sub(m.start)) as f32 * cell_w,
                cell_h,
            );
            if Some(i) == self.search_index {
                active = Some(rect);
            } else {
                rects.push(rect);
            }
        }
        (rects, active)
    }

    /// Recompute `search_matches` for the current query, choosing an initial active match nearest
    /// the viewport top. Clears everything for an empty query.
    fn search_recompute(&mut self) {
        if self.search_query.is_empty() {
            self.search_matches.clear();
            self.search_index = None;
            return;
        }
        let lines = self.grid.history_lines();
        self.search_matches = search::find_matches(&lines, &self.search_query);
        self.search_index = if self.search_matches.is_empty() {
            None
        } else {
            let prefer_line = -(self.grid.display_offset() as i32);
            search::initial_index(&self.search_matches, prefer_line)
        };
    }

    /// Scroll the active match into view (no-op if already visible or there's none).
    fn search_reveal_active(&mut self) {
        if let Some(m) = self
            .search_index
            .and_then(|i| self.search_matches.get(i).copied())
        {
            self.grid.scroll_to_visible(m.line);
        }
    }

    /// Recompute matches against the (possibly reflowed) grid — call after a [`resize`](Self::resize)
    /// so the highlight rects keep tracking the rewrapped text while the search box stays open. A
    /// no-op when search is closed; doesn't force-scroll (the viewport stays where the user left it).
    pub fn search_reflow(&mut self) {
        if self.search_shown {
            self.search_recompute();
        }
    }
}

/// Compute the cell grid that fits a pane of `width_px`×`height_px` *physical* pixels for
/// a font with `cell_w`×`cell_h` cells. Clamped to a sane minimum so a collapsed pane
/// never produces a 0-sized grid. A small free helper the app-shell can reuse for the
/// geometry→resize step.
pub fn cells_for_px(width_px: f32, height_px: f32, cell_w: u32, cell_h: u32) -> (usize, usize) {
    let cols = ((width_px as u32) / cell_w.max(1)).max(2) as usize;
    let rows = ((height_px as u32) / cell_h.max(1)).max(1) as usize;
    (cols, rows)
}

/// Alternate-scroll arrow keys for a wheel notch in the alternate screen (no mouse mode): one
/// Up/Down per scrollback line in `delta_lines` (positive = up). Encoded as application cursor keys
/// (`ESC O A/B`) when DECCKM is set, else normal (`ESC [ A/B`) — what xterm sends so pagers (less,
/// man) scroll on the wheel. This is the no-mouse-grab leg of the "can't scroll Claude" fix.
fn alt_scroll_arrows(delta_lines: i32, app_cursor: bool) -> Vec<u8> {
    let seq: &[u8] = match (delta_lines > 0, app_cursor) {
        (true, false) => b"\x1b[A",
        (false, false) => b"\x1b[B",
        (true, true) => b"\x1bOA",
        (false, true) => b"\x1bOB",
    };
    seq.repeat(delta_lines.unsigned_abs().max(1) as usize)
}

/// Turn raw clipboard text into the exact bytes to write to the pty for a paste.
///
/// Two transforms, both matching how Windows Terminal feeds a paste to conpty:
/// 1. **Normalize line endings to CR (`\r`).** Windows console input treats CR as Enter; a bare
///    LF (`\n`) is mishandled by conpty/PSReadLine, which strands the caret and fragments a
///    multi-line paste across `>>` continuation prompts. Our selection text joins rows with `\n`,
///    and external clipboards carry `\r\n`/`\n` — all collapse to `\r` here.
/// 2. **Bracket** the payload in `ESC[200~ … ESC[201~` *only* when the app enabled bracketed-paste
///    mode (DECSET 2004 — modern PSReadLine / PowerShell 7). Then the shell inserts it as one
///    literal paste (caret at the end, no premature execution). Old shells (Windows PowerShell 5.1)
///    don't set the mode, so the CR-normalized text is sent bare — still the correct Enter handling.
fn prepare_paste(text: &str, bracketed: bool) -> String {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        format!("\u{1b}[200~{normalized}\u{1b}[201~")
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::SoftwareRenderer;

    #[test]
    fn feed_then_dirty_then_render_roundtrips() {
        let mut p = TerminalPane::new(20, 4, Box::new(SoftwareRenderer::new()));
        assert!(p.take_dirty()); // starts dirty
        assert!(!p.take_dirty());
        p.feed("hi");
        assert!(p.take_dirty());
        assert_eq!(p.grid_size(), (20, 4));
    }

    #[test]
    fn resize_signals_session_resize_need() {
        let mut p = TerminalPane::new(20, 4, Box::new(SoftwareRenderer::new()));
        assert!(!p.resize(20, 4));
        assert!(p.resize(30, 8));
        assert_eq!(p.grid_size(), (30, 8));
    }

    #[test]
    fn cells_for_px_clamps_and_divides() {
        assert_eq!(cells_for_px(800.0, 400.0, 8, 16), (100, 25));
        // Collapsed pane never yields a zero grid.
        assert_eq!(cells_for_px(0.0, 0.0, 8, 16), (2, 1));
    }

    // A pane whose surface is `cols`×`rows` logical px → exactly 1px per cell, so a hover at
    // `(col + 0.5, row + 0.5)` lands squarely on cell `(col, row)`.
    fn unit_pane(cols: usize, rows: usize) -> TerminalPane {
        TerminalPane::new(cols, rows, Box::new(SoftwareRenderer::new()))
    }

    #[test]
    fn link_at_hits_a_verified_path_and_misses_prose() {
        let dir = std::env::temp_dir().join(format!("hp_pane_link_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), b"hi").unwrap();

        let mut p = unit_pane(40, 3);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("see note.txt"); // "see " = cols 0..4, "note.txt" = cols 4..12
        let (w, h) = (40.0, 3.0); // 1px per cell

        // Over the path token → a hit pointing at the absolute file.
        let hit = p
            .link_at(6.5, 0.5, w, h)
            .expect("hover over note.txt should hit");
        assert!(hit.abs_path.replace('\\', "/").ends_with("note.txt"));
        // Underline spans exactly the token's columns (4..12) at 1px/col.
        assert_eq!(hit.x, 4.0);
        assert_eq!(hit.w, 8.0);

        // Over the bare word "see" (not path-shaped) → nothing.
        assert!(p.link_at(1.5, 0.5, w, h).is_none());
        // Over a blank cell past the text → nothing.
        assert!(p.link_at(30.5, 0.5, w, h).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reported failure, end to end: an agent announcing an artifact under
    /// `~/Library/Application Support`. The space is not a token boundary here, and only the
    /// disk can say so.
    #[test]
    fn a_path_with_a_space_in_it_links_as_one_path() {
        let dir = std::env::temp_dir().join(format!("hp_pane_space_{}", std::process::id()));
        let sub = dir.join("Application Support");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("lane-watch.html");
        std::fs::write(&file, b"x").unwrap();
        let abs = file.to_string_lossy().into_owned();

        let line = format!("Artifact({abs})");
        let cols = line.chars().count() + 8;
        let mut p = unit_pane(cols, 3);
        p.feed(&line);
        let (w, h) = (cols as f32, 3.0);

        // Hover just inside the '(' — over the half of the path the tokenizer kept.
        let hit = p
            .link_at(10.5, 0.5, w, h)
            .expect("the spaced path should be one link");
        assert_eq!(hit.abs_path.replace('\\', "/"), abs.replace('\\', "/"));
        // The underline covers the whole name: from just past the '(' to just before the ')'.
        assert_eq!(hit.x, 9.0);
        assert_eq!(hit.w, abs.chars().count() as f32);

        // ...and the second half of it is the same link, not a separate one.
        let tail = p
            .link_at((line.chars().count() - 3) as f32 + 0.5, 0.5, w, h)
            .expect("the far side of the space belongs to the same link");
        assert_eq!(tail.abs_path.replace('\\', "/"), abs.replace('\\', "/"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A path the printing program broke across lines itself leaves a prefix that can be a real
    /// directory. Offering it would open the wrong thing; the honest answer is no link.
    #[test]
    fn a_path_cut_off_by_a_hard_wrap_offers_no_link_at_all() {
        let dir = std::env::temp_dir().join(format!("hp_pane_cut_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Volumes").join("Data")).unwrap();

        // Room to spare: `./Volumes` ends mid-row, so it is plainly whole and links.
        let mut wide = unit_pane(20, 3);
        wide.set_cwd(Some(dir.to_string_lossy().into_owned()));
        wide.feed("xxx ./Volumes");
        assert!(wide.link_at(6.5, 0.5, 20.0, 3.0).is_some());

        // Same text in a pane exactly its width, with the rest on a line of its own: the run
        // does not wrap, so `./Volumes` is a fragment and must not stand in for `./Volumes/Data`.
        let mut tight = unit_pane(13, 3);
        tight.set_cwd(Some(dir.to_string_lossy().into_owned()));
        tight.feed("xxx ./Volumes\r\n/Data");
        assert!(tight.link_at(6.5, 0.5, 13.0, 3.0).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_at_ignores_a_nonexistent_path() {
        let dir = std::env::temp_dir().join(format!("hp_pane_nolink_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = unit_pane(40, 2);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("ghost.txt is gone"); // shape-passes but doesn't exist → no link
        assert!(p.link_at(2.5, 0.5, 40.0, 2.0).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctrl_click_copies_the_absolute_path() {
        let dir = std::env::temp_dir().join(format!("hp_pane_copy_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        let mut p = unit_pane(20, 2);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("a.txt"); // cols 0..5
        match p.activate_link(2.5, 0.5, 20.0, 2.0, true) {
            Some(LinkAction::Copy(path)) => {
                assert!(path.replace('\\', "/").ends_with("a.txt"));
            }
            other => panic!("ctrl+click should copy, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A plain click hands the path back for the left file tree rather than opening it: the
    /// human picked the file, not what should happen to it.
    #[test]
    fn plain_click_reveals_the_path_rather_than_opening_it() {
        let dir = std::env::temp_dir().join(format!("hp_pane_reveal_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        let mut p = unit_pane(20, 2);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("a.txt:12:4"); // cols 0..10
        match p.activate_link(2.5, 0.5, 20.0, 2.0, false) {
            Some(LinkAction::Reveal { path, line, col }) => {
                assert!(path.replace('\\', "/").ends_with("a.txt"));
                // The location rides along so whichever tool opens it lands on the right line.
                assert_eq!((line, col), (Some(12), Some(4)));
            }
            other => panic!("a plain click should reveal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The asymmetry D14 asks for: a path that isn't on disk still copies, but has nothing to
    /// reveal — and it never underlines, so it is only ever reached deliberately.
    #[test]
    fn a_missing_path_copies_but_neither_underlines_nor_reveals() {
        let dir = std::env::temp_dir().join(format!("hp_pane_missing_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = unit_pane(20, 2);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("gone.txt"); // never created

        assert!(
            p.link_at(2.5, 0.5, 20.0, 2.0).is_none(),
            "no hover underline"
        );
        let hit = p
            .link_target_at(2.5, 0.5, 20.0, 2.0)
            .expect("a deliberate lookup still finds it");
        assert!(!hit.exists);
        assert!(hit.abs_path.replace('\\', "/").ends_with("gone.txt"));

        match p.activate_link(2.5, 0.5, 20.0, 2.0, true) {
            Some(LinkAction::Copy(path)) => {
                assert!(path.replace('\\', "/").ends_with("gone.txt"));
            }
            other => panic!("ctrl+click should copy a missing path, got {other:?}"),
        }
        assert!(
            p.activate_link(2.5, 0.5, 20.0, 2.0, false).is_none(),
            "there is nothing to reveal"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drag_selection_suppresses_link_activation() {
        // After a drag-select, the same left-release must NOT also open/copy the link under it
        // (the widget fires selection-end then link-activated on one release).
        let dir = std::env::temp_dir().join(format!("hp_pane_seldrag_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        let mut p = unit_pane(20, 2);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("a.txt"); // cols 0..5, 1px/cell on a 20x2 surface
                         // A real drag past the threshold marks the selection dragged.
        p.selection_begin(0.5, 0.5, 20.0, 2.0);
        p.selection_update(5.5, 0.5, 20.0, 2.0); // 5px move > DRAG_THRESHOLD_PX
        assert!(p.selection_is_drag());
        // Activation over the path is suppressed while the drag selection stands — both a plain
        // click (would reveal) and Ctrl+click (would re-copy, clobbering the just-copied selection).
        assert!(p.activate_link(2.5, 0.5, 20.0, 2.0, false).is_none());
        assert!(p.activate_link(2.5, 0.5, 20.0, 2.0, true).is_none());
        // Once the selection is cleared, a click activates the link normally again.
        p.selection_clear();
        assert!(matches!(
            p.activate_link(2.5, 0.5, 20.0, 2.0, true),
            Some(LinkAction::Copy(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A throwaway repo with one commit, or `None` when this machine has no usable git — a
    /// skip, not a failure: this test is about the pane's plumbing, not about git being present.
    fn commit_fixture() -> Option<(std::path::PathBuf, String)> {
        let dir = std::env::temp_dir().join(format!(
            "hp_pane_commit_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        let run = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !run(&["init", "-q", "-b", "main"]) {
            return None;
        }
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "T"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("f.txt"), "one\n").ok()?;
        if !run(&["add", "-A"]) || !run(&["commit", "-q", "-m", "s"]) {
            return None;
        }
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()?;
        let short = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Some((dir, short))
    }

    #[test]
    fn a_hash_this_repository_knows_becomes_a_link_to_the_commit() {
        let Some((dir, short)) = commit_fixture() else {
            return;
        };
        let mut p = unit_pane(60, 3);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        let line = format!("pushed as {short}, signed");
        p.feed(&line);
        let (w, h) = (60.0, 3.0); // 1px per cell
        let start = "pushed as ".len();

        let hit = p
            .link_at(start as f32 + 0.5, 0.5, w, h)
            .expect("hover over the hash should hit");
        assert!(hit.is_commit);
        assert!(!hit.is_url);
        // The full object name rides out, not the abbreviation the screen showed.
        assert_eq!(hit.abs_path.len(), 40);
        assert!(hit.abs_path.starts_with(&short));
        assert_eq!(hit.commit_cwd, dir.to_string_lossy());
        // Underline spans exactly the hash's columns at 1px/col.
        assert_eq!(hit.x, start as f32);
        assert_eq!(hit.w, short.len() as f32);

        // A plain click asks the app to show it, in the repository it was resolved against.
        match p.activate_link(start as f32 + 0.5, 0.5, w, h, false) {
            Some(LinkAction::ShowCommit { cwd, hash }) => {
                assert_eq!(cwd, dir.to_string_lossy());
                assert_eq!(hash, hit.abs_path);
            }
            other => panic!("expected ShowCommit, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hex_word_naming_no_commit_stays_dark() {
        let Some((dir, _)) = commit_fixture() else {
            return;
        };
        let mut p = unit_pane(60, 3);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("saw deadbeef today"); // hex-shaped, but this repo has no such object
        assert!(p.link_at(6.5, 0.5, 60.0, 3.0).is_none());
        // Ctrl/Cmd-click has nothing to copy either: an unresolvable hash is not a target.
        assert!(p.link_target_at(6.5, 0.5, 60.0, 3.0).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bare_name_from_elsewhere_in_the_repository_still_linkifies() {
        let Some((dir, _)) = commit_fixture() else {
            return;
        };
        // The file the agent is talking about is nowhere near the pane's cwd — which is the
        // ordinary case when a session narrates its own work.
        std::fs::create_dir_all(dir.join("deep/er")).unwrap();
        std::fs::write(dir.join("deep/er/b99_price.py"), "x = 1\n").unwrap();
        // And two files that share a name, which is a question with no answer.
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::write(dir.join("a/dup.rs"), "").unwrap();
        std::fs::write(dir.join("b/dup.rs"), "").unwrap();

        let mut p = unit_pane(60, 3);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("in the b99_price.py shape, and dup.rs");
        let (w, h) = (60.0, 3.0); // 1px per cell

        let at = "in the ".len() as f32 + 0.5;
        let hit = p.link_at(at, 0.5, w, h).expect("the name should light up");
        assert!(hit.exists);
        assert!(!hit.is_commit);
        assert!(
            hit.abs_path.ends_with("deep/er/b99_price.py"),
            "{}",
            hit.abs_path
        );
        // Underline spans the name and nothing else.
        assert_eq!(hit.x, "in the ".len() as f32);
        assert_eq!(hit.w, "b99_price.py".len() as f32);

        let dup = "in the b99_price.py shape, and ".len() as f32 + 0.5;
        assert!(
            p.link_at(dup, 0.5, w, h).is_none(),
            "two files of that name: opening one of them would be a guess"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_at_hits_a_url_without_disk_verification() {
        let mut p = unit_pane(40, 3);
        // No cwd set, nothing on disk — a URL must still linkify on shape alone.
        p.feed("go https://a.com/x?q=1 now"); // "go " = cols 0..3, URL = cols 3..22
        let (w, h) = (40.0, 3.0); // 1px per cell

        let hit = p
            .link_at(10.5, 0.5, w, h)
            .expect("hover over the URL should hit");
        assert!(hit.is_url);
        assert_eq!(hit.abs_path, "https://a.com/x?q=1");
        assert_eq!(hit.tip, "https://a.com/x?q=1");
        assert_eq!((hit.line, hit.col), (None, None));
        // Underline spans exactly the URL's columns (3..22) at 1px/col.
        assert_eq!(hit.x, 3.0);
        assert_eq!(hit.w, 19.0);

        // Over the bare word "go" → nothing; past the URL → nothing.
        assert!(p.link_at(1.5, 0.5, w, h).is_none());
        assert!(p.link_at(30.5, 0.5, w, h).is_none());
    }

    #[test]
    fn a_url_wrapped_across_two_rows_stays_one_link() {
        // The pane is 20 cols; the URL is 29 chars, so the terminal soft-wraps it. Hit-testing
        // per visual row used to hand back only the half under the cursor -- a link reading
        // `http://127.0.0.1:51551/row18` opened `http://127.0.0.1:51` and hit nothing.
        let mut p = unit_pane(20, 4);
        let url = "https://a.com/abcdefghijklmno"; // 29 chars
        p.feed(url);
        let (w, h) = (20.0, 4.0); // 1px per cell

        // Hovering the first row: the whole URL, underlined across that row's 20 columns.
        let top = p.link_at(5.5, 0.5, w, h).expect("row 0 should hit");
        assert_eq!(top.abs_path, url);
        assert_eq!((top.x, top.w), (0.0, 20.0));

        // Hovering the continuation row: the same whole URL, underlined over its 9 columns.
        let cont = p.link_at(3.5, 1.5, w, h).expect("row 1 should hit");
        assert_eq!(cont.abs_path, url);
        assert_eq!((cont.x, cont.w), (0.0, 9.0));

        // Past the wrapped tail there is nothing to click.
        assert!(p.link_at(15.5, 1.5, w, h).is_none());
    }

    #[test]
    fn a_path_wrapped_across_two_rows_stays_one_link() {
        let dir = std::env::temp_dir().join(format!("hp_pane_wrap_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a-rather-long-file-name.txt");
        std::fs::write(&file, b"hi").unwrap();
        let abs = file.to_string_lossy().to_string();

        // Narrow enough that the absolute path cannot fit on one row.
        let cols = 24;
        let rows = abs.len().div_ceil(cols) + 2;
        let mut p = unit_pane(cols, rows);
        p.feed(&abs);
        let (w, h) = (cols as f32, rows as f32);

        // The last row of the wrap: still the full path, still verified on disk.
        let last = (abs.len() - 1) / cols;
        let hit = p
            .link_at(1.5, last as f32 + 0.5, w, h)
            .expect("the wrapped tail should hit");
        assert_eq!(hit.abs_path, abs);
        assert!(!hit.is_url);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctrl_click_copies_the_url() {
        let mut p = unit_pane(30, 2);
        p.feed("https://a.com/x"); // cols 0..15
        match p.activate_link(5.5, 0.5, 30.0, 2.0, true) {
            Some(LinkAction::Copy(url)) => assert_eq!(url, "https://a.com/x"),
            other => panic!("ctrl+click on a URL should copy it, got {other:?}"),
        }
    }

    /// A plain click on a URL hands it back rather than launching it: which browser gets the
    /// link is a Hyperpanes preference this crate can't see, and a widget that opened the OS
    /// default itself would silently bypass "ask each time".
    #[test]
    fn plain_click_hands_the_url_back_unopened() {
        let mut p = unit_pane(30, 2);
        p.feed("https://a.com/x");
        match p.activate_link(5.5, 0.5, 30.0, 2.0, false) {
            Some(LinkAction::OpenUrl(url)) => assert_eq!(url, "https://a.com/x"),
            other => panic!("a clicked URL should come back as OpenUrl, got {other:?}"),
        }
    }

    #[test]
    fn a_drag_selection_suppresses_url_activation_too() {
        // Same one-release rule as paths: a drag-select release must not also open/copy the URL.
        let mut p = unit_pane(30, 2);
        p.feed("https://a.com/x");
        p.selection_begin(0.5, 0.5, 30.0, 2.0);
        p.selection_update(8.5, 0.5, 30.0, 2.0); // 8px move > DRAG_THRESHOLD_PX
        assert!(p.selection_is_drag());
        assert!(p.activate_link(5.5, 0.5, 30.0, 2.0, false).is_none());
        assert!(p.activate_link(5.5, 0.5, 30.0, 2.0, true).is_none());
        p.selection_clear();
        assert!(matches!(
            p.activate_link(5.5, 0.5, 30.0, 2.0, true),
            Some(LinkAction::Copy(_))
        ));
    }

    #[test]
    fn prepare_paste_normalizes_newlines_to_cr() {
        // LF and CRLF both collapse to CR (the Enter the Windows console expects); a paste with
        // no bracketed-paste mode is sent bare.
        assert_eq!(prepare_paste("a\nb\r\nc", false), "a\rb\rc");
        // Trailing blank lines (e.g. a multi-row selection over empty rows) become trailing CRs.
        assert_eq!(prepare_paste("x\n\n", false), "x\r\r");
        // No newlines → unchanged.
        assert_eq!(prepare_paste("echo hello", false), "echo hello");
    }

    #[test]
    fn prepare_paste_wraps_in_brackets_only_when_mode_on() {
        assert_eq!(prepare_paste("a\nb", true), "\u{1b}[200~a\rb\u{1b}[201~");
        assert!(!prepare_paste("a\nb", false).contains("200~"));
    }

    #[test]
    fn a_click_twitch_within_threshold_is_not_a_drag() {
        // A press with a sub-threshold wobble (even one that crosses a cell boundary) must NOT
        // promote to a drag — otherwise it copies-on-select and clobbers the clipboard before a
        // paste. A 10×10 pane driven by a 100px surface → each cell is 10px wide: a 2px move from
        // x=19 (cell 1) to x=21 (cell 2) straddles a boundary but stays inside the 4px dead zone.
        let mut p = unit_pane(10, 10);
        p.selection_begin(19.0, 5.0, 100.0, 100.0);
        p.selection_update(21.0, 5.0, 100.0, 100.0); // 2px move, crosses cell 1→2
        assert!(
            !p.selection_is_drag(),
            "a sub-threshold twitch must not be a drag"
        );
        assert!(
            p.selection_text().is_none(),
            "a click must not yield copyable text"
        );
        assert!(
            p.selection_rects(100.0, 100.0).is_empty(),
            "a click leaves no highlight"
        );
    }

    #[test]
    fn a_real_drag_past_threshold_selects_and_copies() {
        let mut p = unit_pane(10, 10);
        p.feed("abcdefghij"); // row 0 cells 0..10
        p.selection_begin(5.0, 5.0, 100.0, 100.0); // cell 0
        p.selection_update(55.0, 5.0, 100.0, 100.0); // 50px move → well past threshold, cell 5
        assert!(
            p.selection_is_drag(),
            "a real drag past the threshold selects"
        );
        assert_eq!(p.selection_text().as_deref(), Some("abcdef"));
    }

    #[test]
    fn wheel_scrolls_scrollback_on_the_main_screen() {
        // Grow scrollback past the viewport, then wheel up: the main screen has no app mouse grab,
        // so the wheel moves OUR viewport (no bytes forwarded) and the display offset advances.
        let mut p = unit_pane(20, 3);
        for _ in 0..50 {
            p.feed("line\r\n");
        }
        assert_eq!(p.scroll_offset(), 0, "pinned to the live edge initially");
        assert!(
            p.wheel(3, 5.0, 5.0, 20.0, 3.0).is_none(),
            "no pty forward on the main screen"
        );
        assert!(
            p.scroll_offset() > 0,
            "wheel moved the scrollback viewport up"
        );
    }

    #[test]
    fn wheel_in_alt_screen_without_mouse_forwards_arrow_keys() {
        // Alt screen, no mouse mode → alternate scroll: wheel becomes Up/Down arrows so a pager
        // scrolls. This is the "can't scroll Claude" fix's no-mouse-grab leg.
        let mut p = unit_pane(20, 3);
        p.feed("\x1b[?1049h"); // enter alternate screen
        let up = p
            .wheel(3, 5.0, 5.0, 20.0, 3.0)
            .expect("alt screen forwards to the pty");
        assert_eq!(
            up, b"\x1b[A\x1b[A\x1b[A",
            "3 lines up → three normal Up arrows"
        );
        let down = p.wheel(-3, 5.0, 5.0, 20.0, 3.0).unwrap();
        assert_eq!(down, b"\x1b[B\x1b[B\x1b[B");
        // The viewport never moved (alt screen has no scrollback to scroll).
        assert_eq!(p.scroll_offset(), 0);
    }

    #[test]
    fn wheel_with_app_cursor_keys_uses_ss3_arrows() {
        let mut p = unit_pane(20, 3);
        p.feed("\x1b[?1049h\x1b[?1h"); // alt screen + DECCKM (application cursor keys)
        let up = p.wheel(3, 5.0, 5.0, 20.0, 3.0).unwrap();
        assert_eq!(up, b"\x1bOA\x1bOA\x1bOA", "DECCKM → SS3 Up arrows");
    }

    #[test]
    fn wheel_with_mouse_grab_forwards_sgr_wheel_report() {
        // A mouse-grabbing app (DECSET 1000) with SGR encoding (1006) gets a wheel report at the
        // pointer cell — one per notch — so it (Claude Code / vim / htop) scrolls its own view.
        let mut p = unit_pane(20, 3);
        p.feed("\x1b[?1000h\x1b[?1006h");
        // Pointer over cell (col 4, row 1) → 1-based (5, 2). One notch (3 lines) → one report.
        let up = p.wheel(3, 4.5, 1.5, 20.0, 3.0).unwrap();
        assert_eq!(up, b"\x1b[<64;5;2M", "wheel-up SGR report, button 64");
        let down = p.wheel(-3, 4.5, 1.5, 20.0, 3.0).unwrap();
        assert_eq!(down, b"\x1b[<65;5;2M", "wheel-down SGR report, button 65");
    }

    #[test]
    fn mouse_report_forwards_press_drag_release_to_a_grabbing_app() {
        // DECSET 1002 (button-event/drag tracking) + 1006 (SGR): forward the mouse so the app
        // (Claude) does its own selection. Pointer at cell (4,1) → 1-based (5,2).
        let mut p = unit_pane(20, 3);
        p.feed("\x1b[?1002h\x1b[?1006h");
        assert!(p.app_grabs_mouse());
        // Left press → button 0.
        assert_eq!(
            p.mouse_report(0, 0, 4.5, 1.5, 20.0, 3.0).unwrap(),
            b"\x1b[<0;5;2M"
        );
        // Drag (motion, left held) → +32 motion bit.
        assert_eq!(
            p.mouse_report(1, 0, 4.5, 1.5, 20.0, 3.0).unwrap(),
            b"\x1b[<32;5;2M"
        );
        // Release → SGR final 'm'.
        assert_eq!(
            p.mouse_report(2, 0, 4.5, 1.5, 20.0, 3.0).unwrap(),
            b"\x1b[<0;5;2m"
        );
        // A bare move (no button) under drag-only tracking (1002) is NOT reported.
        assert!(p.mouse_report(1, 3, 4.5, 1.5, 20.0, 3.0).is_none());
    }

    #[test]
    fn mouse_report_is_none_when_app_has_no_mouse_mode() {
        let mut p = unit_pane(20, 3);
        p.feed("hi"); // plain shell, no mouse mode
        assert!(!p.app_grabs_mouse());
        assert!(p.mouse_report(0, 0, 4.5, 1.5, 20.0, 3.0).is_none());
    }

    #[test]
    fn selection_text_follows_content_after_scrolling() {
        // Select a line, then scroll up: the selection is anchored to the absolute line, so its
        // text is unchanged even though the viewport now shows different rows. (Task: keep the
        // selection while scrolling.)
        let mut p = unit_pane(10, 3);
        p.feed("AAAAA\r\n");
        for _ in 0..20 {
            p.feed("xxxxx\r\n");
        }
        // Scroll up so the "AAAAA" line is back on screen, then select its first 5 cells.
        p.scroll_by(100); // clamps to the top of history
                          // Find AAAAA isn't necessary; just select viewport row 0 cols 0..5 and remember its text.
        p.selection_begin(0.5, 0.5, 10.0, 3.0);
        p.selection_update(4.5, 0.5, 10.0, 3.0);
        let before = p.selection_text();
        assert!(before.is_some());
        // Scroll down one line: the same content moves, but selection_text must be identical.
        p.scroll_by(-1);
        assert_eq!(
            p.selection_text(),
            before,
            "selection text is glued to its line"
        );
    }

    #[test]
    fn drag_at_top_edge_autoscrolls_into_history_and_grows_selection() {
        // 60 lines of history in a 20-row pane (so the 8px edge band leaves a middle zone).
        let mut p = unit_pane(10, 20);
        for i in 0..60 {
            p.feed(&format!("L{i}\r\n"));
        }
        // At the live edge; begin a drag at the bottom and pull it to the top row.
        p.selection_begin(0.5, 19.5, 10.0, 20.0);
        p.selection_update(0.5, 0.5, 10.0, 20.0); // y < edge(8) → top band
        assert!(p.selection_is_drag());
        let off0 = p.scroll_offset();
        let grew_before = p.selection_text();
        assert!(p.selection_autoscroll_tick(), "top-edge drag autoscrolls");
        assert!(p.scroll_offset() > off0, "scrolled up into history");
        // The head was re-mapped to the newly revealed top line → the selection text changed.
        assert_ne!(
            p.selection_text(),
            grew_before,
            "selection grew into scrollback"
        );
    }

    #[test]
    fn drag_at_bottom_edge_autoscrolls_toward_live_edge() {
        let mut p = unit_pane(10, 20);
        for i in 0..60 {
            p.feed(&format!("L{i}\r\n"));
        }
        p.scroll_by(30); // scroll up into history first
        let off0 = p.scroll_offset();
        assert!(off0 > 0);
        p.selection_begin(0.5, 0.5, 10.0, 20.0);
        p.selection_update(8.5, 19.5, 10.0, 20.0); // y > sh-edge → bottom band
        assert!(p.selection_is_drag());
        assert!(
            p.selection_autoscroll_tick(),
            "bottom-edge drag autoscrolls"
        );
        assert!(p.scroll_offset() < off0, "scrolled toward the live edge");
        // Releasing the button stops autoscroll even though the selection is kept.
        p.end_selection_drag();
        assert!(
            !p.selection_autoscroll_tick(),
            "no drag in flight → no autoscroll"
        );
    }

    #[test]
    fn no_autoscroll_when_pointer_is_in_the_middle() {
        let mut p = unit_pane(10, 20);
        for i in 0..60 {
            p.feed(&format!("L{i}\r\n"));
        }
        p.scroll_by(15);
        let off0 = p.scroll_offset();
        p.selection_begin(0.5, 9.5, 10.0, 20.0);
        p.selection_update(8.5, 10.5, 10.0, 20.0); // middle band (8..12) — no edge
        assert!(!p.selection_autoscroll_tick(), "middle drag doesn't scroll");
        assert_eq!(p.scroll_offset(), off0);
    }

    #[test]
    fn scrollbar_appears_after_a_scroll_and_is_hidden_at_the_bottom_with_no_history() {
        let mut p = unit_pane(20, 3);
        // No history yet → no scrollbar even right after a (clamped) scroll.
        assert!(p.scrollbar(60.0).is_none(), "no scrollback → no bar");
        for _ in 0..50 {
            p.feed("line\r\n");
        }
        // A fresh scroll shows the bar at full opacity with a thumb shorter than the track.
        p.scroll_by(5);
        let (thumb_y, thumb_h, op) = p.scrollbar(60.0).expect("bar shows right after a scroll");
        assert!(
            (op - 1.0).abs() < 1e-3,
            "fully opaque immediately after scrolling"
        );
        assert!((SCROLLBAR_MIN_THUMB_PX..60.0).contains(&thumb_h));
        assert!(thumb_y >= 0.0);
    }

    #[test]
    fn selection_on_cursor_row_is_prompt_line_only() {
        // 1px/cell on a 20x5 surface. Put the cursor (the "prompt") on row 2.
        let mut p = unit_pane(20, 5);
        p.feed("a\r\nb\r\nprompt"); // rows 0,1,2; cursor ends on row 2
        let (w, h) = (20.0, 5.0);
        assert!(!p.selection_on_cursor_row(), "no selection → false");
        // A single-row drag ON the cursor row is the editable prompt line.
        p.selection_begin(0.5, 2.5, w, h);
        p.selection_update(5.5, 2.5, w, h);
        assert!(p.selection_is_drag());
        assert!(
            p.selection_on_cursor_row(),
            "selection on the cursor row is the prompt line"
        );
        // A drag on a different row is not the prompt line.
        p.selection_begin(0.5, 0.5, w, h);
        p.selection_update(3.5, 0.5, w, h);
        assert!(
            !p.selection_on_cursor_row(),
            "an off-row selection is not the prompt line"
        );
        // A multi-row selection (even one touching the cursor row) is not a single prompt line.
        p.selection_begin(0.5, 0.5, w, h);
        p.selection_update(3.5, 2.5, w, h);
        assert!(
            !p.selection_on_cursor_row(),
            "a multi-row selection is not a prompt line"
        );
        // A stationary click (not dragged) is not a selection.
        p.selection_begin(0.5, 2.5, w, h);
        assert!(
            !p.selection_on_cursor_row(),
            "a click is not a drag-selection"
        );
    }

    /// 10px/cell on a 20x5 grid: drags between exact cells while clearing the 4px threshold.
    fn prompt_pane() -> (TerminalPane, f32, f32) {
        let mut p = unit_pane(20, 5);
        p.feed("a\r\nb\r\nprompt"); // cursor ends at row 2, col 6
        (p, 200.0, 50.0)
    }

    fn drag(p: &mut TerminalPane, c1: usize, c2: usize, row: usize, w: f32, h: f32) {
        let y = row as f32 * 10.0 + 5.0;
        p.selection_begin(c1 as f32 * 10.0 + 5.0, y, w, h);
        p.selection_update(c2 as f32 * 10.0 + 5.0, y, w, h);
        assert!(p.selection_is_drag());
    }

    /// A plain click: press and release on one cell, never crossing the drag threshold.
    fn click(p: &mut TerminalPane, col: usize, row: usize, w: f32, h: f32) {
        p.selection_begin(col as f32 * 10.0 + 5.0, row as f32 * 10.0 + 5.0, w, h);
        assert!(
            !p.selection_is_drag(),
            "a press with no motion is not a drag"
        );
    }

    #[test]
    fn a_click_left_of_the_caret_walks_it_back_with_left_arrows() {
        // "prompt" with the caret at col 6; click col 2 → 4 lefts.
        let (mut p, w, h) = prompt_pane();
        click(&mut p, 2, 2, w, h);
        assert_eq!(p.click_move_cursor(), Some(b"\x1b[D".repeat(4)));
    }

    #[test]
    fn a_click_right_of_the_caret_walks_it_forward_with_right_arrows() {
        // Past the end of the typed text: the editor clamps, so this is safe and still useful
        // (it lands the caret at the end of the input rather than in the blank cells).
        let (mut p, w, h) = prompt_pane();
        click(&mut p, 9, 2, w, h);
        assert_eq!(p.click_move_cursor(), Some(b"\x1b[C".repeat(3)));
    }

    #[test]
    fn a_click_on_the_caret_sends_nothing() {
        let (mut p, w, h) = prompt_pane();
        click(&mut p, 6, 2, w, h);
        assert_eq!(p.click_move_cursor(), None);
    }

    #[test]
    fn a_click_on_another_line_is_not_an_edit_position() {
        // Row 0 holds "a" — output, not the input line. Moving the caret there would be
        // fabricating an edit the human never asked for.
        let (mut p, w, h) = prompt_pane();
        click(&mut p, 0, 0, w, h);
        assert_eq!(p.click_move_cursor(), None);
    }

    #[test]
    fn a_click_walks_through_a_soft_wrap() {
        // One input longer than the 20-col grid: row 0 wraps into row 1, so the whole thing is
        // a single editable line and the arrows walk straight across the wrap.
        let mut p = unit_pane(20, 5);
        p.feed(&"x".repeat(25)); // caret lands on row 1, col 5
        click(&mut p, 3, 0, 200.0, 50.0);
        // 20 - 3 cells to the end of row 0, plus 5 more back across row 1 = 22 lefts.
        assert_eq!(p.click_move_cursor(), Some(b"\x1b[D".repeat(22)));
    }

    #[test]
    fn a_click_in_the_alternate_screen_moves_nothing() {
        // There the arrows are vim motions, not caret movement.
        let (mut p, w, h) = prompt_pane();
        p.feed("\x1b[?1049h");
        click(&mut p, 2, 2, w, h);
        assert_eq!(p.click_move_cursor(), None);
    }

    #[test]
    fn a_dragged_selection_is_not_a_click() {
        let (mut p, w, h) = prompt_pane();
        drag(&mut p, 1, 3, 2, w, h);
        assert_eq!(p.click_move_cursor(), None);
    }

    #[test]
    fn type_over_backspaces_a_selection_left_of_the_caret() {
        // Select all of "prompt" (cols 0..=5); caret at col 6, right after it → 6 backspaces.
        let (mut p, w, h) = prompt_pane();
        drag(&mut p, 0, 5, 2, w, h);
        assert_eq!(p.type_over_selection(), Some(vec![0x7f; 6]));
        assert!(!p.selection_is_drag(), "type-over consumes the selection");
    }

    #[test]
    fn type_over_steps_left_across_a_gap_before_backspacing() {
        // Select "ro" (cols 1..=2); caret at col 6 → 3 lefts to land after the selection, 2 BS.
        let (mut p, w, h) = prompt_pane();
        drag(&mut p, 1, 2, 2, w, h);
        assert_eq!(
            p.type_over_selection(),
            Some(b"\x1b[D\x1b[D\x1b[D\x7f\x7f".to_vec())
        );
    }

    #[test]
    fn type_over_forward_deletes_a_selection_right_of_the_caret() {
        // Select cols 8..=9 (right of the caret at col 6) → 2 rights + 2 forward-deletes.
        let (mut p, w, h) = prompt_pane();
        drag(&mut p, 8, 9, 2, w, h);
        assert_eq!(
            p.type_over_selection(),
            Some(b"\x1b[C\x1b[C\x1b[3~\x1b[3~".to_vec())
        );
    }

    #[test]
    fn type_over_splits_around_a_caret_inside_the_selection() {
        // Select cols 4..=8 with the caret at col 6 → 2 BS (cols 4-5) + 3 FDEL (cols 6-8).
        let (mut p, w, h) = prompt_pane();
        drag(&mut p, 4, 8, 2, w, h);
        assert_eq!(
            p.type_over_selection(),
            Some(b"\x7f\x7f\x1b[3~\x1b[3~\x1b[3~".to_vec())
        );
    }

    #[test]
    fn type_over_declines_off_row_and_keeps_the_selection() {
        // A selection on a non-cursor row is not editable text — no bytes, selection intact
        // (the caller just clears the highlight).
        let (mut p, w, h) = prompt_pane();
        drag(&mut p, 0, 3, 0, w, h);
        assert_eq!(p.type_over_selection(), None);
        assert!(
            p.selection_is_drag(),
            "declining must not consume the selection"
        );
    }

    #[test]
    fn type_over_spans_a_soft_wrapped_input_line() {
        // 30 chars on a 20-col grid soft-wrap onto row 1 (cursor row 1, col 10). A selection
        // up on row 0 is STILL the same editable line — erase distances go linear through the
        // wrap: caret lin=30, selection cols 5..=8 lin → 21 lefts + 4 backspaces.
        let mut p = unit_pane(20, 5);
        p.feed("abcdefghijklmnopqrstuvwxyz0123");
        let (w, h) = (200.0, 50.0);
        drag(&mut p, 5, 8, 0, w, h);
        let mut expect = b"\x1b[D".repeat(21);
        expect.extend_from_slice(&[0x7f; 4]);
        assert_eq!(p.type_over_selection(), Some(expect));
    }

    #[test]
    fn type_over_declines_on_the_alternate_screen() {
        // In a TUI (vim/htop) the erase bytes would be app commands, not line edits.
        let (mut p, w, h) = prompt_pane();
        p.feed("\x1b[?1049h"); // enter the alternate screen
        drag(&mut p, 0, 5, 2, w, h);
        assert_eq!(p.type_over_selection(), None);
    }

    #[test]
    fn changing_cwd_clears_the_verify_cache() {
        let mut p = unit_pane(20, 2);
        p.set_cwd(Some("/a".to_string()));
        // Prime the cache with a fake entry, then a cwd change must drop it.
        p.verified.insert(
            "x".to_string(),
            paths::ResolveResult {
                token: "t".into(),
                abs_path: "/a/t".into(),
                exists: true,
                is_dir: false,
                is_exe: false,
            },
        );
        assert!(!p.verified.is_empty());
        p.set_cwd(Some("/b".to_string()));
        assert!(
            p.verified.is_empty(),
            "a cwd change must clear stale resolutions"
        );
    }
}
