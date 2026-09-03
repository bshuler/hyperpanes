//! The left panel's **Files** mode: an IDE-style project explorer plus a fuzzy file finder.
//!
//! Two views over one root, because that is how every editor a developer already knows
//! behaves: a tree you expand a directory at a time, and — the moment you type — a flat
//! ranked list of everything under the root whose path matches what you typed.
//!
//! ## Why the rows are built here and not in the resync
//!
//! [`crate::paneview::resync`] runs every frame. A tree that re-listed its directories
//! there would `read_dir` the whole expanded tree at frame rate, which is both slow and
//! wrong (a listing would silently change under a click). So the row list is a *stored*
//! projection: `State` keeps `files_rows` and rebuilds it only on a real event — the root
//! changed, a directory was expanded or collapsed, the query changed, a reveal landed, or
//! the human asked for a refresh. The resync then only copies rows into the Slint model.
//!
//! ## The tree shows everything; the finder does not
//!
//! An explorer that hid `node_modules` would be lying about what is on disk, so the tree
//! lists every entry. The finder walks the same tree with a skip-list and a hard budget,
//! because indexing a 200k-file dependency directory to answer "where is main.rs" costs a
//! second and finds nothing anyone wanted.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A tree row that is a directory.
pub const KIND_DIR: i32 = 0;
/// A tree row that is a regular file.
pub const KIND_FILE: i32 = 1;
/// A row that is not a filesystem entry at all — an empty-directory note, a truncation
/// notice, "no matches". Inert: it has no path and cannot be activated.
pub const KIND_NOTE: i32 = 2;

/// Height of one explorer row, in logical pixels — the same 20px `FileRowView` lays itself
/// out at in `leftpanel.slint`. Duplicated here rather than queried because the scroll
/// offset for a reveal has to be computed in Rust, before the row exists in the UI: a
/// `ListView` cannot be asked where a row it has not yet been given would land.
pub const ROW_H: f32 = 20.0;

/// Extra height a row takes on when it carries a `detail` line (30px total). See [`ROW_H`]
/// — the two must be changed together with `FileRowView`.
pub const ROW_DETAIL_EXTRA: f32 = 10.0;

/// Distance from the top of the row list to the top of the row for `sel`, in logical px, or
/// `None` when `sel` is not among `rows`.
///
/// Selecting a row is only half of showing it: in a tree of any size the selected row is
/// below the fold, and a panel that opened scrolled to row 0 with the highlight off screen
/// answered "where is this file" with a blank list. Measured here, over the same flattened
/// rows the view is about to be given, because a `ListView` cannot be asked where a row it
/// has not yet received would land.
#[tracing::instrument(level = "debug", ret)]
pub fn scroll_offset_for(rows: &[FileRow], sel: &Path) -> Option<f32> {
    let mut y = 0.0f32;
    for r in rows {
        if r.path == sel {
            return Some(y);
        }
        y += ROW_H;
        if !r.detail.is_empty() {
            y += ROW_DETAIL_EXTRA;
        }
    }
    None
}

/// Most rows the flattened tree will hand to the model. A human who expands the whole of a
/// large monorepo gets a truncation note rather than a 200k-row `ListView`.
pub const MAX_ROWS: usize = 5_000;

/// Most entries listed from any one directory.
pub const MAX_ENTRIES: usize = 2_000;

/// Most results the finder returns. Past this, ranking stops mattering — nobody scrolls to
/// the 300th fuzzy match; they type another character.
pub const MAX_RESULTS: usize = 200;

/// Most filesystem entries the finder will look at for one query, across the whole walk.
pub const MAX_SCAN: usize = 40_000;

/// How deep the finder walks below the root. The tree itself has no depth limit — it only
/// ever lists what a human explicitly expanded.
pub const MAX_FIND_DEPTH: usize = 12;

/// Directories the *finder* walks past. Not hidden from the tree: an explorer that hid a
/// directory would be lying about what is on disk. These are simply never worth indexing —
/// they hold generated or vendored files nobody is searching for by name, and they are
/// where a fuzzy walk's entire budget goes if you let it.
pub const FINDER_SKIP: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".next",
    ".turbo",
    ".gradle",
    "DerivedData",
];

/// One row of the Files view, in the order it is drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct FileRow {
    /// Indent level. `0` for a child of the root; always `0` in finder results, which are
    /// a flat ranked list rather than a tree.
    pub depth: i32,
    /// [`KIND_DIR`] · [`KIND_FILE`] · [`KIND_NOTE`].
    pub kind: i32,
    /// Whether a directory row is currently expanded (drives the twisty).
    pub expanded: bool,
    /// The entry's own file name — never the whole path, which does not fit in a 260px panel.
    pub label: String,
    /// The dim trailing column: the containing directory, relative to the root, for a finder
    /// result. Empty in the tree, where the indent already says where a row lives.
    pub detail: String,
    /// The absolute path this row acts on, or empty for a [`KIND_NOTE`].
    pub path: PathBuf,
}

impl FileRow {
    #[tracing::instrument(level = "debug", skip_all)]
    fn note(text: impl Into<String>) -> Self {
        FileRow {
            depth: 0,
            kind: KIND_NOTE,
            expanded: false,
            label: text.into(),
            detail: String::new(),
            path: PathBuf::new(),
        }
    }

    /// Whether clicking this row does anything — a [`KIND_NOTE`] ("no matches", "not a
    /// directory") is a message, not a target, and carries no path to act on.
    #[tracing::instrument(level = "debug", ret)]
    pub fn activatable(&self) -> bool {
        !self.path.as_os_str().is_empty()
    }
}

/// One directory's entries, directories first then files, each group sorted
/// case-insensitively — the ordering every file explorer uses, and the one a human
/// scanning for a name expects.
#[tracing::instrument(level = "debug", ret)]
fn read_children(dir: &Path) -> Result<Vec<(PathBuf, bool)>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut dirs: Vec<(PathBuf, bool)> = Vec::new();
    let mut files: Vec<(PathBuf, bool)> = Vec::new();
    for ent in rd.flatten() {
        // `file_type` on the DirEntry is the cheap answer (no extra stat on the platforms
        // that fill it in from the directory read); a broken symlink counts as a file.
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let p = ent.path();
        if is_dir {
            dirs.push((p, true));
        } else {
            files.push((p, false));
        }
        if dirs.len() + files.len() >= MAX_ENTRIES {
            break;
        }
    }
    let key = |p: &PathBuf| {
        p.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    dirs.sort_by_key(|(p, _)| key(p));
    files.sort_by_key(|(p, _)| key(p));
    dirs.append(&mut files);
    Ok(dirs)
}

#[tracing::instrument(level = "debug", ret)]
fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Flatten the tree under `root` into draw order, descending only into directories present
/// in `expanded`. This is the whole tree model: there is no node graph to keep in sync with
/// the disk, because the disk is read at the moment the rows are built.
#[tracing::instrument(level = "debug", ret)]
pub fn flatten(root: &Path, expanded: &BTreeSet<PathBuf>) -> Vec<FileRow> {
    let mut out = Vec::new();
    walk(root, expanded, 0, &mut out);
    if out.is_empty() {
        out.push(FileRow::note("Empty directory"));
    }
    out
}

#[tracing::instrument(level = "debug", ret)]
fn walk(dir: &Path, expanded: &BTreeSet<PathBuf>, depth: i32, out: &mut Vec<FileRow>) {
    if out.len() >= MAX_ROWS {
        return;
    }
    let children = match read_children(dir) {
        Ok(c) => c,
        Err(e) => {
            out.push(FileRow {
                depth,
                ..FileRow::note(format!("Cannot read: {e}"))
            });
            return;
        }
    };
    for (path, is_dir) in children {
        if out.len() >= MAX_ROWS {
            out.push(FileRow::note("… more entries not shown"));
            return;
        }
        let open = is_dir && expanded.contains(&path);
        out.push(FileRow {
            depth,
            kind: if is_dir { KIND_DIR } else { KIND_FILE },
            expanded: open,
            label: name_of(&path),
            detail: String::new(),
            path: path.clone(),
        });
        if open {
            walk(&path, expanded, depth + 1, out);
        }
    }
}

/// Every ancestor directory of `path` strictly below `root`, so revealing a deep file can
/// expand exactly the directories that lead to it and no others. Empty when `path` is not
/// under `root`.
#[tracing::instrument(level = "debug", ret)]
pub fn ancestors_within(root: &Path, path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = path.parent();
    while let Some(d) = cur {
        if d == root {
            break;
        }
        if !d.starts_with(root) {
            return Vec::new();
        }
        out.push(d.to_path_buf());
        cur = d.parent();
    }
    out
}

/// Fuzzy-match `query` against `cand`, returning a score (higher is better) or `None` when
/// the query's characters do not appear in order.
///
/// The scoring is the small set of rules that make a subsequence match feel like an IDE's:
/// a run of adjacent characters is worth more than the same characters scattered, a
/// character starting a path segment or a word is worth more than one in the middle, and a
/// match inside the file name is worth more than one in the directories above it — typing
/// `stat` should find `state.rs` before `crates/statusline/thing.rs`.
#[tracing::instrument(level = "debug", ret)]
pub fn score(query: &str, cand: &str) -> Option<i32> {
    let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let c: Vec<char> = cand.chars().collect();
    // Everything below the last separator is the file name.
    let name_start = c
        .iter()
        .rposition(|&ch| ch == '/' || ch == '\\')
        .map_or(0, |i| i + 1);
    let mut total = 0i32;
    let mut ci = 0usize;
    let mut run = 0i32;
    for &qc in &q {
        let ql = qc.to_ascii_lowercase();
        let mut hit = None;
        while ci < c.len() {
            if c[ci].to_ascii_lowercase() == ql {
                hit = Some(ci);
                break;
            }
            ci += 1;
            run = 0;
        }
        let at = hit?;
        let mut s = 1;
        run += 1;
        s += run * 3;
        let prev = if at == 0 { None } else { Some(c[at - 1]) };
        let boundary = match prev {
            None => true,
            Some(p) => p == '/' || p == '\\' || p == '-' || p == '_' || p == '.' || p == ' ',
        };
        if boundary {
            s += 8;
        }
        if at >= name_start {
            s += 6;
        }
        total += s;
        ci = at + 1;
    }
    // A shorter candidate that matched the same query is the better answer.
    Some(total - (c.len() as i32) / 8)
}

/// Walk `root` and return the best [`MAX_RESULTS`] matches for `query`, newest ranking
/// first. Bounded by [`MAX_SCAN`] entries and [`MAX_FIND_DEPTH`] levels, skipping
/// [`FINDER_SKIP`] — a finder that takes a second to answer is one nobody types into.
///
/// Directories match too: "where is the parser crate" is the same question as "where is
/// `parser.rs`", and an explorer that could only find leaf files would answer half of it.
#[tracing::instrument(level = "debug", ret)]
pub fn find(root: &Path, query: &str) -> Vec<FileRow> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let mut scanned = 0usize;
    let mut hits: Vec<(i32, PathBuf, bool)> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut truncated = false;
    while let Some((dir, depth)) = stack.pop() {
        let children = match read_children(&dir) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (path, is_dir) in children {
            scanned += 1;
            if scanned > MAX_SCAN {
                truncated = true;
                stack.clear();
                break;
            }
            let name = name_of(&path);
            if is_dir && FINDER_SKIP.contains(&name.as_str()) {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(s) = score(q, &rel) {
                hits.push((s, path.clone(), is_dir));
            }
            if is_dir && depth + 1 < MAX_FIND_DEPTH {
                stack.push((path, depth + 1));
            }
        }
    }
    // Ties break on the path itself, never on directory-read order: the same query must
    // produce the same list twice in a row, or a click races the list under the cursor.
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let shown = hits.len().min(MAX_RESULTS);
    let mut out: Vec<FileRow> = hits[..shown]
        .iter()
        .map(|(_, path, is_dir)| {
            let parent = path
                .parent()
                .and_then(|p| p.strip_prefix(root).ok())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            FileRow {
                depth: 0,
                kind: if *is_dir { KIND_DIR } else { KIND_FILE },
                expanded: false,
                label: name_of(path),
                detail: parent,
                path: path.clone(),
            }
        })
        .collect();
    if out.is_empty() {
        out.push(FileRow::note("No matching files"));
    } else if hits.len() > shown {
        out.push(FileRow::note(format!(
            "… {} more matches — keep typing",
            hits.len() - shown
        )));
    } else if truncated {
        out.push(FileRow::note("… search stopped at the scan limit"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, detail: &str) -> FileRow {
        FileRow {
            depth: 0,
            kind: KIND_FILE,
            expanded: false,
            label: name.into(),
            detail: detail.into(),
            path: PathBuf::from("/r").join(name),
        }
    }

    #[test]
    fn scroll_offset_sums_the_rows_above_the_selection() {
        let rows = vec![row("a", ""), row("b", ""), row("c", "")];
        assert_eq!(
            scroll_offset_for(&rows, Path::new("/r/c")),
            Some(2.0 * ROW_H)
        );
        // The first row needs no scroll at all — an offset it did not ask for would pull the
        // list down past a selection that was already in view.
        assert_eq!(scroll_offset_for(&rows, Path::new("/r/a")), Some(0.0));
    }

    #[test]
    fn scroll_offset_counts_the_taller_finder_rows() {
        // Finder results carry a `detail` line and are 10px taller. Measuring them as plain
        // rows put the viewport progressively short of the match — the deeper the result, the
        // further above the fold it landed.
        let rows = vec![row("a", "src"), row("b", "src/deep"), row("c", "")];
        assert_eq!(
            scroll_offset_for(&rows, Path::new("/r/c")),
            Some(2.0 * (ROW_H + ROW_DETAIL_EXTRA))
        );
    }

    #[test]
    fn scroll_offset_declines_a_row_that_is_not_there() {
        // A reveal whose target the flatten truncated must leave the viewport where it is,
        // not scroll to a guessed offset — the human's place in the list is worth more than
        // a wrong jump.
        let rows = vec![row("a", "")];
        assert_eq!(scroll_offset_for(&rows, Path::new("/r/zz")), None);
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hp-filetree-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn a_collapsed_root_lists_only_its_own_children() {
        let root = scratch("collapsed");
        touch(&root.join("b.txt"));
        touch(&root.join("sub/deep.txt"));
        let rows = flatten(&root, &BTreeSet::new());
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        // Directories first, then files — and `deep.txt` is not there, because `sub` is shut.
        assert_eq!(labels, vec!["sub", "b.txt"]);
        assert_eq!(rows[0].kind, KIND_DIR);
        assert!(!rows[0].expanded);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn expanding_a_directory_inlines_its_children_one_level_deeper() {
        let root = scratch("expanded");
        touch(&root.join("sub/deep.txt"));
        let mut open = BTreeSet::new();
        open.insert(root.join("sub"));
        let rows = flatten(&root, &open);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].expanded);
        assert_eq!(rows[1].label, "deep.txt");
        assert_eq!(rows[1].depth, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ancestors_within_names_exactly_the_directories_to_open() {
        let root = PathBuf::from("/a/b");
        let got = ancestors_within(&root, Path::new("/a/b/c/d/e.txt"));
        assert_eq!(
            got,
            vec![PathBuf::from("/a/b/c/d"), PathBuf::from("/a/b/c")]
        );
        // A path outside the root expands nothing rather than walking to `/`.
        assert!(ancestors_within(&root, Path::new("/x/y.txt")).is_empty());
    }

    #[test]
    fn the_finder_ranks_a_name_match_above_a_directory_match() {
        let name = score("state", "state.rs").unwrap();
        let dir = score("state", "state/lib/other.rs").unwrap();
        assert!(name > dir, "name {name} should beat directory {dir}");
    }

    #[test]
    fn a_query_whose_characters_are_out_of_order_does_not_match() {
        assert!(score("zq", "state.rs").is_none());
        assert!(score("ts", "state.rs").is_some());
    }

    #[test]
    fn the_finder_walks_the_tree_and_skips_vendored_directories() {
        let root = scratch("find");
        touch(&root.join("crates/core/src/state.rs"));
        touch(&root.join("node_modules/pkg/state.rs"));
        let rows = find(&root, "state.rs");
        let paths: Vec<String> = rows
            .iter()
            .filter(|r| r.activatable())
            .map(|r| r.path.display().to_string())
            .collect();
        assert_eq!(paths.len(), 1, "got {paths:?}");
        assert!(paths[0].ends_with("crates/core/src/state.rs"));
        // The result carries where it lives, since the flat list has no indent to say so.
        assert_eq!(rows[0].detail, "crates/core/src");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_query_finds_nothing_rather_than_everything() {
        let root = scratch("emptyq");
        touch(&root.join("a.txt"));
        assert!(find(&root, "   ").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_match_says_so_with_an_inert_row() {
        let root = scratch("nomatch");
        touch(&root.join("a.txt"));
        let rows = find(&root, "zzzzq");
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].activatable());
        let _ = std::fs::remove_dir_all(&root);
    }
}
