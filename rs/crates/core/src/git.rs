//! Reading one commit out of a repository — the half of git that a *link* needs.
//!
//! [`crate::open`] and the app's `gitpanel` between them already cover "where is this file"
//! and "what has changed in the working tree". What neither could answer is the question a
//! hash printed in a pane asks: **which commit is `2d6909f1a`, and what did it touch?**
//!
//! Two entry points, split because they are asked at very different rates:
//!
//!   * [`resolve_commit`] runs on every hover over a hex-shaped token, so it is the cheapest
//!     question git can be asked (`rev-parse --verify`) and nothing else.
//!   * [`load_commit`] runs once, on the click that follows, and pays for the header and the
//!     file list.
//!
//! Everything here is best-effort: git missing, not a repo, a hash that names a blob rather
//! than a commit, a repo whose object is not fetched yet — all of it yields `None`. A link
//! that cannot resolve simply does not light up, which is the same contract a path that is
//! not on disk already has.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Spawn a child without flashing a console window on Windows — the same `CREATE_NO_WINDOW`
/// trick the app's git panel uses. Duplicated rather than shared because the two live in
/// different crates and the alternative is a public trait nobody else wants.
trait NoWindow {
    fn no_window(&mut self) -> &mut Self;
}
impl NoWindow for Command {
    #[cfg(windows)]
    #[tracing::instrument(level = "debug", ret)]
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

/// How much of a `git show` we are willing to read into memory. A commit that renamed a
/// vendored tree can list tens of thousands of files, and the panel draws a few dozen.
const MAX_OUTPUT_BYTES: usize = 1 << 20;

/// Run `git -C dir <args>` and return its stdout, or `None` for any failure at all — a
/// missing git, a non-zero exit, output that is not UTF-8, or output past [`MAX_OUTPUT_BYTES`].
#[tracing::instrument(level = "debug", ret)]
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        // A commit link must never be the thing that pops a credential or GPG prompt: this
        // reads local objects only, and `core.askPass` staying empty keeps it that way.
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .no_window()
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.len() > MAX_OUTPUT_BYTES {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// The work tree root containing `dir`, or `None` when it is in no repository.
#[tracing::instrument(level = "debug", ret)]
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"])?;
    let line = out.trim();
    if line.is_empty() {
        None
    } else {
        Some(PathBuf::from(line))
    }
}

/// True when `rev` is shaped like something we are willing to hand to git as a revision.
/// Deliberately narrow — a hex object name and nothing else. `rev-parse` happily accepts
/// `HEAD@{yesterday}`, `:/fix the thing` and other forms whose text comes from a pane's
/// output, and none of them is what a clicked hash means.
#[tracing::instrument(level = "debug", ret)]
pub fn is_hex_rev(rev: &str) -> bool {
    let n = rev.len();
    (7..=40).contains(&n)
        && rev
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Find the one file `name` refers to, when it is not where the pane is standing.
///
/// A coding session says `b99_price.py` and means a file three directories away; the pane's
/// cwd resolves it to nothing, and the name stays dark. The repository is the right place to
/// look for it, and `git ls-files` is the cheap way: one subprocess over the index (plus
/// untracked-but-not-ignored files, which a just-written file is), instead of a walk of a
/// tree whose `node_modules` alone would dwarf the answer.
///
/// **Ambiguity is a miss, deliberately.** Four files named `mod.rs` and a guess would open
/// the wrong one, and a link that opens the wrong file is worse than a name that stays dark.
///
/// `name` may carry directories (`src/b99_price.py`); it matches on a whole-segment suffix,
/// so `price.py` never answers for `b99_price.py`.
#[tracing::instrument(level = "debug", ret)]
pub fn find_in_repo(dir: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.starts_with('/') || name.contains('\\') || name.contains("..") {
        return None;
    }
    let root = repo_root(dir)?;
    // Two pathspecs: the name at the root, and the name anywhere under it. `*` in a git
    // pathspec crosses `/`, so `*b99_price.py` is the whole-tree search — done by git, so
    // the output that crosses the pipe is the handful of matches rather than the index.
    let anywhere = format!("*{name}");
    let out = git(
        &root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            name,
            &anywhere,
        ],
    )?;
    let tail = format!("/{name}");
    let mut hit: Option<&str> = None;
    for p in out.split('\0').filter(|p| !p.is_empty()) {
        // The pathspec glob matched on characters; this is the segment boundary it ignored.
        if p != name && !p.ends_with(&tail) {
            continue;
        }
        match hit {
            Some(prev) if prev != p => return None,
            _ => hit = Some(p),
        }
    }
    Some(root.join(hit?))
}

/// Resolve `rev` to the full hash of a **commit** in the repository containing `dir`, or
/// `None`. The `^{commit}` peel is what makes this an answer rather than a guess: a tree or
/// blob whose abbreviation happens to match is not something a commit link can show.
#[tracing::instrument(level = "debug", ret)]
pub fn resolve_commit(dir: &Path, rev: &str) -> Option<String> {
    if !is_hex_rev(rev) {
        return None;
    }
    let out = git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
    )?;
    let full = out.trim();
    (full.len() == 40 && is_hex_rev(full)).then(|| full.to_string())
}

/// One file a commit touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitFile {
    /// Repo-relative path, exactly as git reported it (forward slashes on every platform).
    pub path: String,
    /// The file name — the part a 260px panel actually shows.
    pub label: String,
    /// The parent directory, drawn dimmed after the label. Empty at the repo root.
    pub detail: String,
    /// git's status letter: `A` `M` `D` `R` `C` `T`.
    pub code: char,
}

impl CommitFile {
    #[tracing::instrument(level = "debug", ret)]
    fn new(path: String, code: char) -> Self {
        let (detail, label) = match path.rsplit_once('/') {
            Some((dir, name)) => (dir.to_string(), name.to_string()),
            None => (String::new(), path.clone()),
        };
        Self {
            path,
            label,
            detail,
            code,
        }
    }
}

/// A commit as a link target: enough to caption the view, plus what it touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    /// The work tree the paths in [`files`](Self::files) are relative to.
    pub root: PathBuf,
    /// Full 40-character hash — the identity, and what every follow-up command speaks.
    pub hash: String,
    /// git's own abbreviation, which is what the panel shows.
    pub short: String,
    pub subject: String,
    pub author: String,
    /// Author date, `YYYY-MM-DD`.
    pub date: String,
    pub files: Vec<CommitFile>,
}

/// The separator between header fields. A record separator rather than a newline because a
/// commit subject can (and in this repo does) contain almost anything else.
const FIELD: &str = "\u{1f}";

/// Load the commit `rev` names, as seen from `dir`. `None` when `dir` is in no repository or
/// `rev` is not a commit in it.
#[tracing::instrument(level = "debug", ret)]
pub fn load_commit(dir: &Path, rev: &str) -> Option<Commit> {
    let root = repo_root(dir)?;
    let hash = resolve_commit(&root, rev)?;

    let fmt = format!("--format=%h{FIELD}%an{FIELD}%ad{FIELD}%s");
    let head = git(&root, &["show", "--no-patch", "--date=short", &fmt, &hash])?;
    let mut parts = head.trim_end_matches('\n').splitn(4, FIELD);
    let short = parts.next().unwrap_or_default().to_string();
    let author = parts.next().unwrap_or_default().to_string();
    let date = parts.next().unwrap_or_default().to_string();
    let subject = parts.next().unwrap_or_default().to_string();

    Some(Commit {
        files: files_of(&root, &hash),
        root,
        hash,
        short,
        author,
        date,
        subject,
    })
}

/// The files `hash` touched. `-z` so a path containing a space, a quote or a newline arrives
/// verbatim — the same reason the working-tree view uses it. A merge commit legitimately
/// reports nothing here (`git show` diffs it against no parent), and that is not an error.
#[tracing::instrument(level = "debug", ret)]
fn files_of(root: &Path, hash: &str) -> Vec<CommitFile> {
    let Some(out) = git(root, &["show", "--name-status", "--format=", "-z", hash]) else {
        return Vec::new();
    };
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    let mut files = Vec::new();
    while let Some(status) = fields.next() {
        let code = status.chars().next().unwrap_or('M').to_ascii_uppercase();
        // A rename or copy spends TWO path fields — old then new — and the new one is the
        // file that now exists, so it is the only one a click could open.
        let path = if matches!(code, 'R' | 'C') {
            fields.next();
            fields.next()
        } else {
            fields.next()
        };
        let Some(path) = path else { break };
        files.push(CommitFile::new(path.to_string(), code));
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// A throwaway repo with one commit, or `None` when this machine has no usable git —
    /// which is a skip, not a failure, for tests that are about git's output format.
    fn fixture() -> Option<(tempdir::Dir, String)> {
        let dir = tempdir::Dir::new()?;
        let run = |args: &[&str]| -> bool {
            Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
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
        std::fs::create_dir_all(dir.path().join("sub")).ok()?;
        std::fs::write(dir.path().join("sub/a b.txt"), "one\n").ok()?;
        std::fs::write(dir.path().join("top.txt"), "two\n").ok()?;
        if !run(&["add", "-A"]) || !run(&["commit", "-q", "-m", "a subject: with punctuation"]) {
            return None;
        }
        let hash = git(dir.path(), &["rev-parse", "HEAD"])?.trim().to_string();
        Some((dir, hash))
    }

    /// A minimal scratch directory that removes itself — the crate has no dev-dependency on
    /// a tempdir crate and one test does not justify adding one.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Option<Self> {
                let base = std::env::temp_dir().join(format!(
                    "hyperpanes-git-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&base);
                std::fs::create_dir_all(&base).ok()?;
                Some(Self(base))
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn only_a_lowercase_hex_abbreviation_is_offered_to_git() {
        assert!(is_hex_rev("2d6909f1a"));
        assert!(is_hex_rev(&"a".repeat(40)));
        assert!(
            !is_hex_rev("2d6909"),
            "six characters is too ambiguous to link"
        );
        assert!(!is_hex_rev(&"a".repeat(41)));
        assert!(
            !is_hex_rev("2D6909F1A"),
            "uppercase is not how git prints one"
        );
        assert!(
            !is_hex_rev("HEAD~1"),
            "a revision expression is not a clicked hash"
        );
        assert!(!is_hex_rev(":/fix the thing"));
    }

    #[test]
    fn a_hash_printed_by_git_resolves_back_to_the_commit_it_names() {
        let Some((dir, hash)) = fixture() else { return };
        let short = &hash[..9];
        assert_eq!(
            resolve_commit(dir.path(), short).as_deref(),
            Some(&hash[..])
        );
        assert_eq!(
            resolve_commit(dir.path(), &hash).as_deref(),
            Some(&hash[..])
        );
    }

    #[test]
    fn a_hex_word_that_names_nothing_does_not_light_up() {
        let Some((dir, _)) = fixture() else { return };
        assert_eq!(resolve_commit(dir.path(), "defaced"), None);
        assert_eq!(resolve_commit(dir.path(), &"f".repeat(40)), None);
    }

    #[test]
    fn a_commit_reports_its_caption_and_every_file_it_touched() {
        let Some((dir, hash)) = fixture() else { return };
        let c = load_commit(dir.path(), &hash[..9]).expect("the commit we just made");
        assert_eq!(c.hash, hash);
        assert_eq!(c.subject, "a subject: with punctuation");
        assert_eq!(c.author, "T");
        assert_eq!(
            c.date.len(),
            10,
            "%ad --date=short is YYYY-MM-DD: {}",
            c.date
        );

        let mut paths: Vec<_> = c.files.iter().map(|f| f.path.clone()).collect();
        paths.sort();
        assert_eq!(
            paths,
            ["sub/a b.txt", "top.txt"],
            "a space in a path survives -z"
        );
        assert!(
            c.files.iter().all(|f| f.code == 'A'),
            "a first commit adds everything"
        );

        let deep = c.files.iter().find(|f| f.path.starts_with("sub/")).unwrap();
        assert_eq!(
            (deep.label.as_str(), deep.detail.as_str()),
            ("a b.txt", "sub")
        );
    }

    #[test]
    fn a_bare_name_finds_its_one_file_anywhere_in_the_repository() {
        let Some((dir, _)) = fixture() else { return };
        let root = dir.path();
        // What comes back is rooted at git's own `--show-toplevel`, which on macOS has
        // already walked the `/var` → `/private/var` symlink the temp dir sits behind.
        let real = repo_root(root).expect("the fixture is a repository");

        // The name is three directories from where we are standing, and still resolves.
        assert_eq!(
            find_in_repo(root, "a b.txt").as_deref(),
            Some(real.join("sub/a b.txt").as_path()),
            "a nested file answers to its bare name"
        );
        assert_eq!(
            find_in_repo(&root.join("sub"), "top.txt").as_deref(),
            Some(real.join("top.txt").as_path()),
            "the search is the repository, not the directory we asked from"
        );
        assert_eq!(
            find_in_repo(root, "sub/a b.txt").as_deref(),
            Some(real.join("sub/a b.txt").as_path()),
            "a partial path is a name too"
        );

        assert_eq!(
            find_in_repo(root, "op.txt"),
            None,
            "the match is by whole segment: `op.txt` is not `top.txt`"
        );
        assert_eq!(find_in_repo(root, "nothing-like-this.txt"), None);

        // A second `top.txt` — untracked, but not ignored, so the search sees it and now
        // cannot say which one was meant. Silence beats opening the wrong file.
        std::fs::write(root.join("sub/top.txt"), "three\n").unwrap();
        assert_eq!(
            find_in_repo(root, "top.txt"),
            None,
            "two files of that name is an ambiguity, not a pick"
        );
    }

    #[test]
    fn a_directory_outside_any_repository_has_no_commits_to_show() {
        let tmp = std::env::temp_dir();
        // Not asserting on `tmp` itself being repo-less would make this test lie on a
        // machine whose temp dir is inside a checkout; skip there rather than fail.
        if repo_root(&tmp).is_some() {
            return;
        }
        assert_eq!(load_commit(&tmp, &"a".repeat(40)), None);
    }
}
