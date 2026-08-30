//! Family B panes: the non-PTY views — the file browser, the file viewer and the
//! markdown preview.
//!
//! A Family A pane (terminal, and every tool pane) is backed by a pty and renders
//! itself; there is nothing for the app to project beyond the surface image. A
//! Family B pane has no pty at all (see D3: it mints a `view-N` uid and never
//! reaches the [`SessionManager`](hyperpanes_core::session_manager::SessionManager)),
//! so its content has to be *computed* — and that is what lives here.
//!
//! The design rule this module exists to enforce: **all of the parsing happens in
//! Rust**, never in `.slint`. Slint receives one flat row model whose `role` says
//! how to style each row, and styles it. That keeps the three views deterministic,
//! unit-testable, and immune to the layout-cycle hazards a smarter .slint would
//! invite — and it means one ~150-line `ViewPane` component serves all three kinds
//! instead of three separate renderers.
//!
//! The pane's target is its [`PaneState::cwd`](crate::state::PaneState) — a
//! *directory* for [`PaneKind::FileBrowser`], a *file* for the viewer and the
//! markdown preview. That reuse is deliberate: `PaneSpec.cwd` already round-trips
//! through persistence, so a view pane survives a restart with no format change.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use hyperpanes_core::tools::PaneKind;
use slint::{ModelRc, VecModel};

use crate::PaneViewRow;

/// How a row is drawn. The `.slint` side switches on this and nothing else — it
/// never re-parses `text`. Keep these in lock-step with `ViewPane` in
/// `ui/viewpanes.slint`; an unknown role there falls back to [`role::LINE`].
pub mod role {
    /// The synthetic ".." row at the top of a directory listing.
    pub const PARENT: i32 = 0;
    /// A subdirectory.
    pub const DIR: i32 = 1;
    /// A regular file.
    pub const FILE: i32 = 2;
    /// A plain line of a text file.
    pub const LINE: i32 = 3;
    /// Markdown `#`.
    pub const H1: i32 = 4;
    /// Markdown `##`.
    pub const H2: i32 = 5;
    /// Markdown `###` (and deeper — `####` renders as H3 rather than vanishing).
    pub const H3: i32 = 6;
    /// A line inside a fenced code block, or an indented code line.
    pub const CODE: i32 = 7;
    /// A `-` / `*` / `1.` list item.
    pub const BULLET: i32 = 8;
    /// A `>` block quote.
    pub const QUOTE: i32 = 9;
    /// A `---` horizontal rule (draws geometry; `text` is empty).
    pub const RULE: i32 = 10;
    /// An advisory row the app generated — truncation, "empty directory", an IO
    /// error. Never activatable.
    pub const NOTICE: i32 = 11;
}

/// One projected row. `path` is empty for everything that is not activatable, so
/// the click handler can refuse a row without consulting its role — the same
/// "decide it once, on the producing side" rule the left panel's `blocked` flag
/// follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewRow {
    pub role: i32,
    pub text: String,
    /// The dim trailing column: a size + age for a listing, a line number for the
    /// viewer, "" for markdown.
    pub detail: String,
    /// The absolute path this row activates, or "" when the row is inert.
    pub path: PathBuf,
}

impl ViewRow {
    /// A row with nothing to open.
    fn inert(role: i32, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
            detail: String::new(),
            path: PathBuf::new(),
        }
    }

    /// Whether clicking this row does anything (drives the pointer cursor).
    pub fn activatable(&self) -> bool {
        !self.path.as_os_str().is_empty()
    }
}

/// Most lines a file viewer will read. A view pane is a *preview*, not an editor:
/// past this the projection stops and appends a [`role::NOTICE`] saying so, rather
/// than pushing a million-row model into Slint.
pub const MAX_LINES: usize = 5_000;

/// Longest single line kept, in chars. A minified bundle is one 3 MB line; the
/// widget would try to lay all of it out.
const MAX_LINE_CHARS: usize = 2_000;

/// Largest file the viewer will read at all, in bytes (2 MiB).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Most entries a directory listing keeps. `node_modules` is the reason.
pub const MAX_ENTRIES: usize = 2_000;

// ---------------------------------------------------------------------------
// the three projections
// ---------------------------------------------------------------------------

/// Project `target` for `kind`. The single entry point: the caller never has to
/// know which of the three parsers applies.
///
/// Returns a single [`role::NOTICE`] row rather than an empty model on every
/// failure path — an empty pane looks broken, a pane that says *why* does not.
pub fn rows_for(kind: &PaneKind, target: Option<&str>) -> Vec<ViewRow> {
    let Some(t) = target.filter(|t| !t.is_empty()) else {
        return vec![ViewRow::inert(role::NOTICE, "No path set for this pane")];
    };
    let path = PathBuf::from(t);
    match kind {
        PaneKind::FileBrowser => list_dir(&path),
        PaneKind::FileViewer => read_lines(&path),
        PaneKind::Markdown => markdown_blocks(&path),
        // Family A, or a kind this build does not know: nothing to project.
        _ => Vec::new(),
    }
}

/// The header line for a view pane: the target's own name, which is all the
/// header has room for. "" when there is no target.
pub fn view_title(kind: &PaneKind, target: Option<&str>) -> String {
    if !matches!(
        kind,
        PaneKind::FileBrowser | PaneKind::FileViewer | PaneKind::Markdown
    ) {
        return String::new();
    }
    match target.filter(|t| !t.is_empty()) {
        // The full path is the honest title, but it is also 90 chars of noise in a
        // 26px header; the last two components read as a location without wrapping.
        Some(t) => tail_two(Path::new(t)),
        None => String::new(),
    }
}

/// The last two path components ("hyperpanes/src"), or the whole path when it is
/// already that short. Never empty for a non-empty path — a bare "/" returns "/".
fn tail_two(p: &Path) -> String {
    let comps: Vec<String> = p
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    match comps.len() {
        0 => String::new(),
        1 => comps[0].clone(),
        n => {
            let a = &comps[n - 2];
            let b = &comps[n - 1];
            // A root component already carries its separator ("/" or "C:\").
            if a.ends_with('/') || a.ends_with('\\') {
                format!("{a}{b}")
            } else {
                format!("{a}/{b}")
            }
        }
    }
}

/// A directory listing: the ".." row, then subdirectories, then files, each group
/// sorted by name. Dirs-before-files is the convention every file manager uses and
/// the one that makes a deep tree navigable by clicking down the top of the list.
///
/// Sorting is case-insensitive so `README` does not sort above `apps` the way a
/// raw byte comparison would.
pub fn list_dir(dir: &Path) -> Vec<ViewRow> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => return vec![ViewRow::inert(role::NOTICE, format!("Cannot read: {e}"))],
    };
    let now = now_secs();
    let mut dirs: Vec<ViewRow> = Vec::new();
    let mut files: Vec<ViewRow> = Vec::new();
    let mut skipped = 0usize;
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        // Dotfiles are shown — this is a developer tool and `.git` / `.hyperpanes`
        // are exactly what its user is looking for.
        let md = match ent.metadata() {
            Ok(md) => md,
            // A broken symlink still deserves a row; it just has no size or age.
            Err(_) => {
                files.push(ViewRow {
                    role: role::FILE,
                    text: name,
                    detail: "—".into(),
                    path: ent.path(),
                });
                continue;
            }
        };
        if dirs.len() + files.len() >= MAX_ENTRIES {
            skipped += 1;
            continue;
        }
        let age = md
            .modified()
            .ok()
            .map(|m| age_label(m, now))
            .unwrap_or_default();
        if md.is_dir() {
            dirs.push(ViewRow {
                role: role::DIR,
                text: name,
                detail: age,
                path: ent.path(),
            });
        } else {
            let detail = if age.is_empty() {
                size_label(md.len())
            } else {
                format!("{} · {age}", size_label(md.len()))
            };
            files.push(ViewRow {
                role: role::FILE,
                text: name,
                detail,
                path: ent.path(),
            });
        }
    }
    let key = |r: &ViewRow| r.text.to_lowercase();
    dirs.sort_by_key(key);
    files.sort_by_key(key);

    let mut rows = Vec::with_capacity(dirs.len() + files.len() + 2);
    if let Some(parent) = dir.parent() {
        rows.push(ViewRow {
            role: role::PARENT,
            text: "..".into(),
            detail: String::new(),
            path: parent.to_path_buf(),
        });
    }
    let empty = dirs.is_empty() && files.is_empty();
    rows.append(&mut dirs);
    rows.append(&mut files);
    if empty {
        rows.push(ViewRow::inert(role::NOTICE, "Empty directory"));
    }
    if skipped > 0 {
        rows.push(ViewRow::inert(
            role::NOTICE,
            format!("… {skipped} more entries not shown"),
        ));
    }
    rows
}

/// A file's lines, numbered, bounded by [`MAX_LINES`] / [`MAX_LINE_CHARS`].
pub fn read_lines(file: &Path) -> Vec<ViewRow> {
    let text = match read_text(file) {
        Ok(t) => t,
        Err(row) => return vec![row],
    };
    let mut rows: Vec<ViewRow> = Vec::new();
    let mut total = 0usize;
    for (i, line) in text.lines().enumerate() {
        total = i + 1;
        if i >= MAX_LINES {
            continue;
        }
        rows.push(ViewRow {
            role: role::LINE,
            text: clip(line),
            detail: (i + 1).to_string(),
            path: PathBuf::new(),
        });
    }
    if rows.is_empty() {
        rows.push(ViewRow::inert(role::NOTICE, "Empty file"));
    } else if total > MAX_LINES {
        rows.push(ViewRow::inert(
            role::NOTICE,
            format!("… {} more lines not shown", total - MAX_LINES),
        ));
    }
    rows
}

/// A markdown file as styled blocks. Line-based on purpose: a real markdown parser
/// would pull in a dependency and an inline-span model the row projection cannot
/// express anyway, and a preview only needs the block level to read like a
/// document. Fences win over every other rule while open, so a `# comment` inside
/// a shell snippet stays code.
pub fn markdown_blocks(file: &Path) -> Vec<ViewRow> {
    let text = match read_text(file) {
        Ok(t) => t,
        Err(row) => return vec![row],
    };
    let mut rows: Vec<ViewRow> = Vec::new();
    let mut fenced = false;
    let mut total = 0usize;
    for (i, raw) in text.lines().enumerate() {
        total = i + 1;
        if i >= MAX_LINES {
            continue;
        }
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            // The fence line itself is chrome, not content: toggle and drop it.
            fenced = !fenced;
            continue;
        }
        if fenced {
            rows.push(ViewRow::inert(role::CODE, clip(raw)));
            continue;
        }
        let row = if let Some(rest) = heading(trimmed) {
            rest
        } else if is_rule(trimmed) {
            ViewRow::inert(role::RULE, "")
        } else if let Some(rest) = bullet(trimmed) {
            ViewRow::inert(role::BULLET, clip(rest))
        } else if let Some(rest) = trimmed.strip_prefix('>') {
            ViewRow::inert(role::QUOTE, clip(rest.trim_start()))
        } else if raw.starts_with("    ") || raw.starts_with('\t') {
            ViewRow::inert(role::CODE, clip(raw.trim_end()))
        } else {
            ViewRow::inert(role::LINE, clip(trimmed))
        };
        rows.push(row);
    }
    if rows.is_empty() {
        rows.push(ViewRow::inert(role::NOTICE, "Empty file"));
    } else if total > MAX_LINES {
        rows.push(ViewRow::inert(
            role::NOTICE,
            format!("… {} more lines not shown", total - MAX_LINES),
        ));
    }
    rows
}

/// `#`/`##`/`###+` → the matching heading row. `None` when the line is not a
/// heading — including `#hashtag`, which needs the space ATX requires.
fn heading(trimmed: &str) -> Option<ViewRow> {
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    let body = rest.strip_prefix(' ')?;
    let r = match hashes {
        1 => role::H1,
        2 => role::H2,
        // `####` and deeper share H3's styling rather than disappearing.
        _ => role::H3,
    };
    Some(ViewRow::inert(r, clip(body.trim())))
}

/// `- x` / `* x` / `+ x` / `1. x` → the item text.
fn bullet(trimmed: &str) -> Option<&str> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(m) {
            return Some(rest);
        }
    }
    // An ordered item: digits, then `.` or `)`, then a space.
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &trimmed[digits..];
        for m in [". ", ") "] {
            if let Some(r) = rest.strip_prefix(m) {
                return Some(r);
            }
        }
    }
    None
}

/// `---`, `***`, `___` (three or more, nothing else on the line).
fn is_rule(trimmed: &str) -> bool {
    let t = trimmed.trim_end();
    for c in ['-', '*', '_'] {
        if t.len() >= 3 && t.chars().all(|ch| ch == c) {
            return true;
        }
    }
    false
}

/// Read a file as text, refusing what a preview has no business loading. The error
/// arm is a ready-made [`role::NOTICE`] row so every caller reports failure the
/// same way.
fn read_text(file: &Path) -> Result<String, ViewRow> {
    let md = fs::metadata(file)
        .map_err(|e| ViewRow::inert(role::NOTICE, format!("Cannot read: {e}")))?;
    if md.is_dir() {
        return Err(ViewRow::inert(role::NOTICE, "That is a directory"));
    }
    if md.len() > MAX_FILE_BYTES {
        return Err(ViewRow::inert(
            role::NOTICE,
            format!("File is {} — too large to preview", size_label(md.len())),
        ));
    }
    let bytes =
        fs::read(file).map_err(|e| ViewRow::inert(role::NOTICE, format!("Cannot read: {e}")))?;
    // A NUL in the first block is the same heuristic `grep` uses for "binary".
    if bytes.iter().take(8_000).any(|b| *b == 0) {
        return Err(ViewRow::inert(role::NOTICE, "Binary file — not previewed"));
    }
    // Lossy on purpose: a stray invalid byte should cost one replacement char, not
    // the whole preview.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Truncate to [`MAX_LINE_CHARS`] *chars* (never bytes — a byte slice can split a
/// UTF-8 sequence) and drop the trailing `\r` a CRLF file leaves on every line.
fn clip(s: &str) -> String {
    let s = s.strip_suffix('\r').unwrap_or(s);
    if s.chars().count() <= MAX_LINE_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_LINE_CHARS).collect();
    out.push('…');
    out
}

/// "1.2 KB" — binary units, one decimal past KB, so a listing column stays narrow.
fn size_label(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    for (i, unit) in ["KB", "MB", "GB", "TB"].iter().enumerate() {
        let div = K.powi(i as i32 + 1);
        if b < div * K || *unit == "TB" {
            return format!("{:.1} {unit}", b / div);
        }
    }
    unreachable!("the TB arm is unconditional")
}

/// "3m", "5h", "2d", "Aug 30" — the same shape the left panel's session rows use,
/// so the two lists read as one product.
fn age_label(t: SystemTime, now: u64) -> String {
    let then = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if then == 0 || then > now {
        return String::new();
    }
    let d = now - then;
    match d {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", d / 60),
        3600..=86_399 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86_400),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// the per-pane model cache
// ---------------------------------------------------------------------------

/// What a projection was computed from. Re-projecting is a disk walk, so the pump
/// must not do it per tick — but it must notice an edit. The fingerprint is the
/// target's path plus its mtime and length, which is exactly what changes when the
/// answer would change.
#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    kind: i32,
    target: String,
    mtime: u64,
    len: u64,
}

fn fingerprint(kind: &PaneKind, target: Option<&str>) -> Fingerprint {
    let target = target.unwrap_or_default().to_string();
    let (mtime, len) = fs::metadata(&target)
        .ok()
        .map(|md| {
            let m = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (m, md.len())
        })
        // A missing target still needs a stable fingerprint, so the "Cannot read"
        // notice is computed once rather than every tick.
        .unwrap_or((0, 0));
    Fingerprint {
        kind: kind.ui_kind(),
        target,
        mtime,
        len,
    }
}

/// One cached projection.
struct Cached {
    fp: Fingerprint,
    /// Bumped every time this pane is re-projected. The pump has no other way to
    /// ask "did anything change?" — `ModelRc` has no identity comparison — and the
    /// tests assert on it rather than on wall-clock behaviour.
    /// Bumped on every reprojection. Read only by the tests — this slint revision has
    /// no `ModelRc::ptr_eq`, so a generation counter is how "did the model change?"
    /// is asserted at all.
    #[cfg_attr(not(test), allow(dead_code))]
    gen: u64,
    rows: Vec<ViewRow>,
    model: ModelRc<PaneViewRow>,
}

thread_local! {
    /// Per-pane projections, keyed by pane uid. Holds the *Slint* model, not just
    /// the rows: `pane_item` runs every tick and must be able to hand the model
    /// over with a refcount bump instead of rebuilding a 5,000-row `VecModel`.
    static VIEW_CACHE: RefCell<HashMap<String, Cached>> = RefCell::new(HashMap::new());
    /// Window-wide projection counter (see [`Cached::gen`]).
    static VIEW_GEN: RefCell<u64> = const { RefCell::new(0) };
}

/// The Slint row model for pane `uid`, recomputed only when the target changed on
/// disk. Cheap enough to call from the per-frame pump — the steady state is one
/// `stat` and a refcount bump.
pub fn model_for(uid: &str, kind: &PaneKind, target: Option<&str>) -> ModelRc<PaneViewRow> {
    let fp = fingerprint(kind, target);
    VIEW_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(have) = c.get(uid) {
            if have.fp == fp {
                return have.model.clone();
            }
        }
        let rows = rows_for(kind, target);
        let model: ModelRc<PaneViewRow> = ModelRc::from(Rc::new(VecModel::from(
            rows.iter()
                .map(|r| PaneViewRow {
                    role: r.role,
                    text: r.text.as_str().into(),
                    detail: r.detail.as_str().into(),
                    activatable: r.activatable(),
                })
                .collect::<Vec<_>>(),
        )));
        let gen = VIEW_GEN.with(|g| {
            let mut g = g.borrow_mut();
            *g += 1;
            *g
        });
        c.insert(
            uid.to_string(),
            Cached {
                fp,
                gen,
                rows,
                model: model.clone(),
            },
        );
        model
    })
}

/// Which projection pane `uid` is currently showing. Unequal values mean the rows
/// were rebuilt between the two calls; `None` means the pane has no projection.
#[cfg(test)]
pub fn generation(uid: &str) -> Option<u64> {
    VIEW_CACHE.with(|c| c.borrow().get(uid).map(|e| e.gen))
}

/// The row a click landed on, by index into the model that was on screen. Index is
/// safe here (unlike the session list, which is keyed by id) because the model and
/// the rows are stored together and replaced together — a stale index can only
/// miss, never resolve to a different row's path.
pub fn row_at(uid: &str, index: usize) -> Option<ViewRow> {
    VIEW_CACHE.with(|c| c.borrow().get(uid).and_then(|e| e.rows.get(index).cloned()))
}

/// Drop a pane's projection. Called when the pane closes so a long-lived window
/// does not accumulate the row vectors of every view pane it ever had.
pub fn forget(uid: &str) {
    VIEW_CACHE.with(|c| {
        c.borrow_mut().remove(uid);
    });
}

/// The kind a file should open as when it is activated from a browser row: a
/// markdown file gets the preview, everything else the plain viewer. Deterministic
/// and extension-driven — the alternative (sniffing content) would make the same
/// click do different things on different days.
pub fn kind_for_file(path: &Path) -> PaneKind {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "markdown" | "mdown" | "mkd" => PaneKind::Markdown,
        _ => PaneKind::FileViewer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;
    use std::io::Write;

    /// A scratch directory unique to the calling test, removed and recreated so a
    /// rerun never sees the previous run's files.
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hp-viewpane-{name}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = fs::File::create(&p).expect("create");
        f.write_all(body.as_bytes()).expect("write");
        p
    }

    #[test]
    fn a_listing_puts_the_parent_first_then_dirs_then_files() {
        let d = scratch("listing");
        fs::create_dir(d.join("zeta")).unwrap();
        fs::create_dir(d.join("Alpha")).unwrap();
        write(&d, "b.txt", "hi");
        write(&d, "A.txt", "hi");

        let rows = list_dir(&d);
        let shape: Vec<(i32, &str)> = rows.iter().map(|r| (r.role, r.text.as_str())).collect();
        assert_eq!(
            shape,
            vec![
                (role::PARENT, ".."),
                // case-insensitive within each group, dirs before files
                (role::DIR, "Alpha"),
                (role::DIR, "zeta"),
                (role::FILE, "A.txt"),
                (role::FILE, "b.txt"),
            ]
        );
        // The parent row navigates; it is not decoration.
        assert_eq!(rows[0].path, d.parent().unwrap());
        assert!(rows.iter().all(|r| r.activatable()));
    }

    #[test]
    fn an_unreadable_target_reports_why_instead_of_going_blank() {
        let missing = std::env::temp_dir().join("hp-viewpane-nope-does-not-exist");
        let _ = fs::remove_dir_all(&missing);

        for rows in [
            list_dir(&missing),
            read_lines(&missing),
            markdown_blocks(&missing),
        ] {
            assert_eq!(rows.len(), 1, "one notice row, not an empty pane");
            assert_eq!(rows[0].role, role::NOTICE);
            assert!(rows[0].text.starts_with("Cannot read:"), "{}", rows[0].text);
            // A notice is never clickable.
            assert!(!rows[0].activatable());
        }
    }

    #[test]
    fn the_viewer_numbers_lines_and_says_when_it_stopped() {
        let d = scratch("viewer");
        let body: String = (1..=MAX_LINES + 7).map(|i| format!("line {i}\n")).collect();
        let f = write(&d, "long.txt", &body);

        let rows = read_lines(&f);
        assert_eq!(rows.len(), MAX_LINES + 1, "the cap plus one notice");
        assert_eq!(rows[0].detail, "1");
        assert_eq!(rows[0].text, "line 1");
        assert_eq!(rows[MAX_LINES - 1].detail, MAX_LINES.to_string());
        let last = rows.last().unwrap();
        assert_eq!(last.role, role::NOTICE);
        // The count is what was DROPPED, not the file length.
        assert_eq!(last.text, "… 7 more lines not shown");
    }

    #[test]
    fn a_binary_file_is_refused_rather_than_rendered_as_mojibake() {
        let d = scratch("binary");
        let p = d.join("a.bin");
        fs::write(&p, [0x7f, 0x45, 0x4c, 0x46, 0x00, 0x01, 0x02]).unwrap();

        let rows = read_lines(&p);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "Binary file — not previewed");
    }

    #[test]
    fn markdown_maps_each_block_to_its_role() {
        let d = scratch("md");
        let f = write(
            &d,
            "doc.md",
            "# Title\n\
             ## Sub\n\
             #### Deep\n\
             #nothashtag\n\
             plain text\n\
             - first\n\
             2. second\n\
             > quoted\n\
             ---\n",
        );

        let blocks = markdown_blocks(&f);
        let got: Vec<(i32, &str)> = blocks.iter().map(|r| (r.role, r.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (role::H1, "Title"),
                (role::H2, "Sub"),
                // `####` shares H3 rather than vanishing.
                (role::H3, "Deep"),
                // ATX needs the space, so this is prose.
                (role::LINE, "#nothashtag"),
                (role::LINE, "plain text"),
                (role::BULLET, "first"),
                (role::BULLET, "second"),
                (role::QUOTE, "quoted"),
                (role::RULE, ""),
            ]
        );
    }

    #[test]
    fn a_fence_wins_over_every_other_markdown_rule() {
        let d = scratch("fence");
        let f = write(
            &d,
            "doc.md",
            "before\n```sh\n# not a heading\n- not a bullet\n```\nafter\n",
        );

        let rows = markdown_blocks(&f);
        let got: Vec<(i32, String)> = rows.iter().map(|r| (r.role, r.text.clone())).collect();
        assert_eq!(
            got,
            vec![
                (role::LINE, "before".into()),
                // the fence lines themselves are dropped, their contents stay verbatim
                (role::CODE, "# not a heading".into()),
                (role::CODE, "- not a bullet".into()),
                (role::LINE, "after".into()),
            ]
        );
    }

    #[test]
    fn the_cache_reprojects_only_when_the_file_actually_changed() {
        let d = scratch("cache");
        let f = write(&d, "a.txt", "one\n");
        let target = f.display().to_string();
        let uid = "view-1";

        let first = model_for(uid, &PaneKind::FileViewer, Some(&target));
        assert_eq!(first.row_count(), 1);
        let g1 = generation(uid).expect("projected");

        // Same fingerprint → no rebuild at all, which is what makes this safe to
        // call from the per-frame pump.
        let again = model_for(uid, &PaneKind::FileViewer, Some(&target));
        assert_eq!(again.row_count(), 1);
        assert_eq!(
            generation(uid),
            Some(g1),
            "an unchanged target must not reproject"
        );

        // Rewrite with a different length: the fingerprint moves even if the
        // filesystem's mtime resolution would not have caught the edit.
        fs::write(&f, "one\ntwo\n").unwrap();
        let third = model_for(uid, &PaneKind::FileViewer, Some(&target));
        assert_eq!(third.row_count(), 2);
        assert_ne!(generation(uid), Some(g1), "an edit must reproject");

        // row_at reads the rows stored alongside that model.
        assert_eq!(row_at(uid, 1).unwrap().text, "two");
        assert!(row_at(uid, 99).is_none());
        forget(uid);
        assert!(row_at(uid, 0).is_none());
        assert_eq!(generation(uid), None);
    }

    #[test]
    fn only_family_b_kinds_project_rows() {
        let d = scratch("kinds");
        let f = write(&d, "a.txt", "x\n");
        let t = f.display().to_string();
        assert!(rows_for(&PaneKind::Terminal, Some(&t)).is_empty());
        assert!(rows_for(&PaneKind::Tool("claude".into()), Some(&t)).is_empty());
        assert!(!rows_for(&PaneKind::FileViewer, Some(&t)).is_empty());
        // No target is a notice, not a panic and not an empty pane.
        let none = rows_for(&PaneKind::FileBrowser, None);
        assert_eq!(none[0].role, role::NOTICE);
        assert_eq!(none[0].text, "No path set for this pane");
    }

    #[test]
    fn a_file_opens_as_markdown_only_for_a_markdown_extension() {
        assert_eq!(kind_for_file(Path::new("/a/README.md")), PaneKind::Markdown);
        assert_eq!(kind_for_file(Path::new("/a/README.MD")), PaneKind::Markdown);
        assert_eq!(kind_for_file(Path::new("/a/main.rs")), PaneKind::FileViewer);
        assert_eq!(kind_for_file(Path::new("/a/LICENSE")), PaneKind::FileViewer);
    }

    #[test]
    fn labels_stay_narrow_enough_for_the_detail_column() {
        assert_eq!(size_label(0), "0 B");
        assert_eq!(size_label(999), "999 B");
        assert_eq!(size_label(1024), "1.0 KB");
        assert_eq!(size_label(1536), "1.5 KB");
        assert_eq!(size_label(2 * 1024 * 1024), "2.0 MB");
        assert_eq!(size_label(3 * 1024 * 1024 * 1024), "3.0 GB");

        let now = 1_000_000u64;
        assert_eq!(
            age_label(UNIX_EPOCH + std::time::Duration::from_secs(now), now),
            "now"
        );
        assert_eq!(
            age_label(UNIX_EPOCH + std::time::Duration::from_secs(now - 300), now),
            "5m"
        );
        assert_eq!(
            age_label(UNIX_EPOCH + std::time::Duration::from_secs(now - 7200), now),
            "2h"
        );
        assert_eq!(
            age_label(
                UNIX_EPOCH + std::time::Duration::from_secs(now - 3 * 86_400),
                now
            ),
            "3d"
        );
        // A clock skewed into the future must not underflow into a huge age.
        assert_eq!(
            age_label(UNIX_EPOCH + std::time::Duration::from_secs(now + 60), now),
            ""
        );
    }

    #[test]
    fn a_long_line_is_clipped_on_char_boundaries() {
        let wide = "é".repeat(MAX_LINE_CHARS + 50);
        let out = clip(&wide);
        assert_eq!(
            out.chars().count(),
            MAX_LINE_CHARS + 1,
            "the ellipsis is the +1"
        );
        assert!(out.ends_with('…'));
        // CRLF must not leave a visible carriage return.
        assert_eq!(clip("hi\r"), "hi");
    }

    #[test]
    fn the_title_is_the_last_two_components() {
        assert_eq!(
            view_title(&PaneKind::FileBrowser, Some("/Users/me/code/app")),
            "code/app"
        );
        assert_eq!(view_title(&PaneKind::FileViewer, Some("/etc")), "/etc");
        assert_eq!(view_title(&PaneKind::Markdown, Some("")), "");
        // Family A panes have no view title at all.
        assert_eq!(view_title(&PaneKind::Terminal, Some("/Users/me")), "");
    }
}
