//! The left panel's GIT mode: a read-only working-tree view of the repo the panel is
//! pointed at — branch, upstream divergence, and the staged / changed / untracked files.
//!
//! Read-only on purpose. The panel *shows* the working tree and lets you open a file from
//! it (the same reveal the FILES mode uses); staging, committing and discarding are
//! destructive and are not wired to a one-click row.
//!
//! Everything here comes from ONE `git status --porcelain=v2 --branch -z` run, parsed
//! deterministically rather than sniffed:
//!
//!   * `-z` makes records NUL-terminated, so a path containing a space, a quote or a
//!     newline arrives verbatim — the default porcelain output C-quotes those, and
//!     un-quoting it correctly is a parser we would rather not own.
//!   * `--porcelain=v2` splits the index state (X) from the working-tree state (Y), which
//!     is exactly the "Staged Changes" / "Changes" split the panel draws. v1 collapses
//!     rename information the same way but reports no branch divergence.
//!
//! Any failure (git missing, not a repo, a broken index) yields [`GitStatus::none`]: the
//! panel says it has nothing rather than surfacing a subprocess error to a sidebar.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Spawn a child without flashing a console window on Windows — same `CREATE_NO_WINDOW`
/// trick `sidebar` uses for its `git worktree` calls (its trait is private to that module).
trait NoWindow {
    fn no_window(&mut self) -> &mut Self;
}
impl NoWindow for Command {
    #[cfg(windows)]
    fn no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        self.creation_flags(CREATE_NO_WINDOW)
    }
    #[cfg(not(windows))]
    fn no_window(&mut self) -> &mut Self {
        self
    }
}

/// Which of the three sections a row belongs to. The panel draws them in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// Index differs from HEAD — `git commit` would include this.
    Staged,
    /// Working tree differs from the index, or the merge is unresolved.
    Changed,
    /// Not tracked and not ignored.
    Untracked,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::Staged => "Staged Changes",
            Section::Changed => "Changes",
            Section::Untracked => "Untracked",
        }
    }
}

/// One file in the working-tree view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitRow {
    /// Repo-relative path exactly as git reported it — the row's identity.
    pub path: String,
    /// File name, the part the 260px-wide panel actually shows.
    pub label: String,
    /// Parent directory (empty at the repo root), drawn dimmed after the label.
    pub detail: String,
    /// The single-letter status badge: `A` `M` `D` `R` `C` `T` `U` `?`.
    pub code: char,
    pub section: Section,
}

impl GitRow {
    fn new(path: String, code: char, section: Section) -> Self {
        // Split on `/`: git reports repo-relative paths with forward slashes on every
        // platform, so this is correct on Windows too and needs no `Path` round-trip.
        let (detail, label) = match path.rsplit_once('/') {
            Some((dir, name)) => (dir.to_string(), name.to_string()),
            None => (String::new(), path.clone()),
        };
        Self {
            path,
            label,
            detail,
            code,
            section,
        }
    }
}

/// The whole view: one repo's head and its working tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitStatus {
    /// The repo root the rows are relative to; `None` when there is no repo.
    pub root: Option<PathBuf>,
    /// Short branch name, or `HEAD detached` when there is no branch.
    pub branch: String,
    /// Configured upstream (`origin/main`), when there is one.
    pub upstream: Option<String>,
    /// Commits ahead of / behind the upstream. Both 0 with no upstream.
    pub ahead: u32,
    pub behind: u32,
    pub rows: Vec<GitRow>,
}

impl GitStatus {
    /// "There is nothing to show" — no repo, no git, or a git that failed.
    pub fn none() -> Self {
        Self::default()
    }

    pub fn is_repo(&self) -> bool {
        self.root.is_some()
    }

    /// Rows of one section, in git's own (path-sorted) order.
    pub fn section(&self, section: Section) -> impl Iterator<Item = &GitRow> {
        self.rows.iter().filter(move |r| r.section == section)
    }

    /// `main ↑2 ↓1` — the header line, already assembled so the UI does no formatting.
    pub fn head_summary(&self) -> String {
        let mut s = self.branch.clone();
        if self.ahead > 0 {
            s.push_str(&format!("  ↑{}", self.ahead));
        }
        if self.behind > 0 {
            s.push_str(&format!("  ↓{}", self.behind));
        }
        s
    }
}

/// The status letter for a porcelain v2 `XY` half. `.` (and a stray space, which v1 uses
/// in the same position) mean "unchanged in this half" and produce no row.
fn code_of(c: u8) -> Option<char> {
    match c {
        b'.' | b' ' => None,
        b'A' | b'M' | b'D' | b'R' | b'C' | b'T' => Some(c as char),
        // Anything else is a status this build doesn't name; show it verbatim rather than
        // dropping the file — a file silently missing from the panel is the worse failure.
        other => Some(other as char),
    }
}

/// Parse `git status --porcelain=v2 --branch -z` output.
///
/// Records are NUL-terminated. A `2` (rename/copy) record carries its ORIGINAL path in the
/// following NUL-field, so the iterator has to consume two fields for one row — the reason
/// this is a hand-rolled loop and not a `split('\0').map(..)`.
pub fn parse_status_v2(out: &str) -> GitStatus {
    let mut st = GitStatus {
        branch: "HEAD detached".to_string(),
        ..Default::default()
    };
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    while let Some(rec) = fields.next() {
        let Some((tag, rest)) = rec.split_once(' ') else {
            continue;
        };
        match tag {
            "#" => {
                let (key, val) = rest.split_once(' ').unwrap_or((rest, ""));
                match key {
                    // `(detached)` is git's own literal for "no branch"; keep the default.
                    "branch.head" if val != "(detached)" => st.branch = val.to_string(),
                    "branch.upstream" => st.upstream = Some(val.to_string()),
                    "branch.ab" => {
                        // `+N -M`, always both, always in that order.
                        for part in val.split_whitespace() {
                            let (sign, n) = part.split_at(1);
                            let n: u32 = n.parse().unwrap_or(0);
                            match sign {
                                "+" => st.ahead = n,
                                "-" => st.behind = n,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            "?" => st
                .rows
                .push(GitRow::new(rest.to_string(), '?', Section::Untracked)),
            // Ignored entries are only emitted with `--ignored`, which we never pass.
            "!" => {}
            "1" | "2" => {
                // `<XY> <sub> <mH> <mI> <mW> <hH> <hI> [<Xscore> ]<path>`
                let mut it = rest.splitn(if tag == "2" { 9 } else { 8 }, ' ');
                let Some(xy) = it.next() else { continue };
                let path = match it.clone().last() {
                    Some(p) => p.to_string(),
                    None => continue,
                };
                if tag == "2" {
                    // Consume the original path so it is not mistaken for the next record.
                    let _ = fields.next();
                }
                let xy = xy.as_bytes();
                if xy.len() < 2 {
                    continue;
                }
                if let Some(c) = code_of(xy[0]) {
                    st.rows.push(GitRow::new(path.clone(), c, Section::Staged));
                }
                if let Some(c) = code_of(xy[1]) {
                    st.rows.push(GitRow::new(path, c, Section::Changed));
                }
            }
            "u" => {
                // Unmerged: both halves describe the conflict, and it is one row, not two.
                let path = rest.rsplit(' ').next().unwrap_or_default().to_string();
                if !path.is_empty() {
                    st.rows.push(GitRow::new(path, 'U', Section::Changed));
                }
            }
            _ => {}
        }
    }
    st
}

/// Run the status query in `root`. `None` on any failure — see the module note.
pub fn status_in(root: &Path) -> Option<GitStatus> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            // Every untracked file, not just the containing directory: the panel lists
            // files, and `normal` would collapse a new directory into one unopenable row.
            "--untracked-files=all",
            "-z",
        ])
        .no_window()
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut st = parse_status_v2(&String::from_utf8_lossy(&out.stdout));
    st.root = Some(root.to_path_buf());
    Some(st)
}

/// The whole read: find the repo enclosing `cwd`, then describe it. `None` when `cwd` is
/// not inside a repo.
pub fn status_for(cwd: &str) -> Option<GitStatus> {
    let root = crate::sidebar::git_root_of(cwd)?;
    status_in(&root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records are NUL-*terminated*, so the fixture ends with one too.
    fn z(lines: &[&str]) -> String {
        lines.iter().map(|l| format!("{l}\0")).collect::<String>()
    }

    #[test]
    fn reads_branch_upstream_and_divergence() {
        let st = parse_status_v2(&z(&[
            "# branch.oid 1111111111111111111111111111111111111111",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
        ]));
        assert_eq!(st.branch, "main");
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        assert_eq!(st.head_summary(), "main  ↑2  ↓1");
        assert!(st.rows.is_empty());
    }

    #[test]
    fn a_detached_head_is_named_not_left_blank() {
        let st = parse_status_v2(&z(&["# branch.head (detached)", "# branch.ab +0 -0"]));
        assert_eq!(st.branch, "HEAD detached");
        assert_eq!(st.head_summary(), "HEAD detached");
    }

    /// The XY split IS the section split: a file staged *and* then edited again shows up
    /// in both sections, which is what git itself reports and what the user has to see to
    /// understand why a commit would not include their latest edit.
    #[test]
    fn the_index_half_and_the_worktree_half_are_separate_rows() {
        let st = parse_status_v2(&z(&[
            "# branch.head main",
            "1 MM N... 100644 100644 100644 aaa bbb src/state.rs",
        ]));
        let staged: Vec<_> = st.section(Section::Staged).collect();
        let changed: Vec<_> = st.section(Section::Changed).collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(changed.len(), 1);
        assert_eq!(staged[0].code, 'M');
        assert_eq!(staged[0].label, "state.rs");
        assert_eq!(staged[0].detail, "src");
        assert_eq!(staged[0].path, "src/state.rs");
    }

    #[test]
    fn an_unchanged_half_produces_no_row() {
        let st = parse_status_v2(&z(&[
            "# branch.head main",
            "1 .M N... 100644 100644 100644 aaa bbb only-in-worktree.txt",
            "1 A. N... 000000 100644 100644 000 bbb only-in-index.txt",
        ]));
        assert_eq!(st.section(Section::Staged).count(), 1);
        assert_eq!(st.section(Section::Changed).count(), 1);
        assert_eq!(
            st.section(Section::Staged).next().unwrap().path,
            "only-in-index.txt"
        );
        assert_eq!(
            st.section(Section::Changed).next().unwrap().path,
            "only-in-worktree.txt"
        );
    }

    /// A `2` record's original path is a field of its own. Miss it and the very next
    /// record is read as a path — the bug this test exists to pin.
    #[test]
    fn a_rename_consumes_its_original_path_field() {
        let st = parse_status_v2(&z(&[
            "# branch.head main",
            "2 R. N... 100644 100644 100644 aaa bbb R100 new/name.rs",
            "old/name.rs",
            "? untracked.txt",
        ]));
        let staged: Vec<_> = st.section(Section::Staged).collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].path, "new/name.rs");
        assert_eq!(staged[0].code, 'R');
        let untracked: Vec<_> = st.section(Section::Untracked).collect();
        assert_eq!(
            untracked.len(),
            1,
            "the record after a rename is not a path"
        );
        assert_eq!(untracked[0].path, "untracked.txt");
    }

    #[test]
    fn untracked_and_conflicted_land_in_their_own_sections() {
        let st = parse_status_v2(&z(&[
            "# branch.head main",
            "? new file.txt",
            "u UU N... 100644 100644 100644 100644 a b c conflict.rs",
        ]));
        let un: Vec<_> = st.section(Section::Untracked).collect();
        assert_eq!(un.len(), 1);
        // A space in the name survives because `-z` never quotes.
        assert_eq!(un[0].path, "new file.txt");
        assert_eq!(un[0].code, '?');
        let ch: Vec<_> = st.section(Section::Changed).collect();
        assert_eq!(ch.len(), 1, "a conflict is ONE row, not one per half");
        assert_eq!(ch[0].code, 'U');
        assert_eq!(ch[0].path, "conflict.rs");
    }

    #[test]
    fn a_root_level_file_has_no_detail() {
        let st = parse_status_v2(&z(&["# branch.head main", "? README.md"]));
        let r = st.section(Section::Untracked).next().unwrap();
        assert_eq!(r.label, "README.md");
        assert_eq!(r.detail, "");
    }

    #[test]
    fn empty_output_is_a_clean_tree_not_a_panic() {
        let st = parse_status_v2("");
        assert!(st.rows.is_empty());
        assert!(!st.is_repo());
        assert_eq!(GitStatus::none(), GitStatus::default());
    }

    /// End to end against a real repo — the parser and the flags have to agree with the
    /// git that is actually installed, not with the fixture I wrote.
    #[test]
    fn a_real_repo_reports_its_branch_and_a_new_file() {
        let root = std::env::temp_dir().join(format!("hp-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .expect("git runs")
        };
        if !git(&["init", "-b", "trunk"]).status.success() {
            return; // no git on this machine — nothing to assert against
        }
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(root.join("sub/added.txt"), "hi").unwrap();
        std::fs::write(root.join("loose.txt"), "hi").unwrap();
        git(&["add", "sub/added.txt"]);

        let st = status_in(&root).expect("status");
        assert_eq!(st.branch, "trunk");
        assert_eq!(st.root.as_deref(), Some(root.as_path()));
        let staged: Vec<_> = st.section(Section::Staged).collect();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].path, "sub/added.txt");
        assert_eq!(staged[0].code, 'A');
        let un: Vec<_> = st.section(Section::Untracked).map(|r| &r.path).collect();
        assert_eq!(un, vec!["loose.txt"]);

        let _ = std::fs::remove_dir_all(&root);
    }
}
