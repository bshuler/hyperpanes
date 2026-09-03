//! Files dragged from the OS file manager (Finder / Explorer / Nautilus) onto a pane.
//!
//! A drop inserts the paths into that pane's program as shell-quoted words — and stops
//! there. It never presses Enter: what the user dropped is an *argument* they are still
//! composing (`claude`'s prompt, an `open` you were about to type), and a file manager
//! drag is far too easy to fumble to have it run a line.
//!
//! ## Why the cursor is read in the hook and not in the pump
//!
//! winit's `WindowEvent::DroppedFile` carries a path and nothing else — no position, and
//! no "that was the last file" marker. So this module does two things the pump cannot:
//!
//!  * it samples the **global cursor** ([`crate::drag::global_pointer`]) at the instant
//!    each file lands, because by the time the 8 ms pump drains the queue the pointer has
//!    moved (often out of the window entirely — the user drops and lets go);
//!  * it holds the queue until it stops growing ([`SETTLE`]), so a five-file drag arrives
//!    at the pane as one insertion rather than as five, whichever tick boundary the five
//!    events happen to straddle.
//!
//! Platform notes: macOS and Windows deliver drops through winit's own registration
//! (`registerForDraggedTypes` / `IDropTarget`) and X11 speaks XDND. Wayland has no
//! `DroppedFile` in winit 0.30, so a drop there is inert — the same shape of gap as the
//! Wayland global-pointer fallback in `drag/`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long the queue must stop growing before it is delivered. Long enough to swallow the
/// per-file event burst of one drag, short enough to feel instant.
pub const SETTLE: Duration = Duration::from_millis(60);

/// How to quote a path so the program in the pane reads it back as one word.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quoting {
    /// `sh`-family: single quotes, `'\''` for an embedded quote.
    Posix,
    /// `cmd`/PowerShell: double quotes (a Windows path cannot contain one).
    Windows,
}

/// The convention of the shell this build's panes run.
#[tracing::instrument(level = "debug", ret)]
pub fn native_quoting() -> Quoting {
    if cfg!(windows) {
        Quoting::Windows
    } else {
        Quoting::Posix
    }
}

/// One file, as it landed.
#[derive(Debug, Clone)]
pub struct Dropped {
    /// Native handle of the window the OS delivered the drop to — authoritative, since the
    /// event itself is what the window received.
    pub win: isize,
    /// Global cursor (physical px, top-left origin) at the moment of the drop, or `None`
    /// where the platform cannot read one. Picks the *pane* within `win`.
    pub at: Option<(i32, i32)>,
    pub path: PathBuf,
    pub when: Instant,
}

thread_local! {
    static QUEUE: std::cell::RefCell<Vec<Dropped>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Start listening for drops on the window identified by `key` (its native handle).
/// Idempotent per window as long as the caller only calls it once the handle resolves.
#[tracing::instrument(level = "debug", ret, skip(win))]
pub fn install(win: &slint::Window, key: isize) {
    crate::winit_hooks::add(
        win,
        key,
        Box::new(move |_, ev| {
            if let slint::winit_030::winit::event::WindowEvent::DroppedFile(p) = ev {
                record(key, p.clone());
            }
            slint::winit_030::EventResult::Propagate
        }),
    );
}

/// Queue one dropped file, stamping it with where the pointer is *right now*.
#[tracing::instrument(level = "debug", ret)]
fn record(win: isize, path: PathBuf) {
    let at = crate::drag::global_pointer()
        .poll()
        .map(|(p, _)| (p.x, p.y));
    QUEUE.with(|q| {
        q.borrow_mut().push(Dropped {
            win,
            at,
            path,
            when: Instant::now(),
        })
    });
}

/// Drain the queue if it has settled; otherwise leave it to grow for another tick.
#[tracing::instrument(level = "debug", ret)]
pub fn take_settled() -> Vec<Dropped> {
    QUEUE.with(|q| split_settled(&mut q.borrow_mut(), Instant::now(), SETTLE))
}

/// All-or-nothing: the batch is only released once its newest member is older than
/// `settle`, so the files of one drag are never split across two insertions.
#[tracing::instrument(level = "debug", ret)]
fn split_settled(queue: &mut Vec<Dropped>, now: Instant, settle: Duration) -> Vec<Dropped> {
    let ready = queue
        .iter()
        .map(|d| d.when)
        .max()
        .is_some_and(|newest| now.duration_since(newest) >= settle);
    if ready {
        std::mem::take(queue)
    } else {
        Vec::new()
    }
}

/// Quote one path. `None` when the name carries a control character.
///
/// A newline in a filename is legal on Unix and is the one thing that must never reach a
/// pty: it *is* Enter, so a dropped file could run the half-typed line it was dropped into.
/// Rather than mangle the name into something that no longer opens, such a path is refused
/// and reported — the same call the transcript sanitizer makes, in the other direction.
#[tracing::instrument(level = "debug", ret)]
pub fn quote(path: &str, style: Quoting) -> Option<String> {
    if path.is_empty() || path.chars().any(|c| c.is_control() || c == '\u{7f}') {
        return None;
    }
    match style {
        Quoting::Posix => {
            if path.chars().all(posix_bare) {
                Some(path.to_string())
            } else {
                Some(format!("'{}'", path.replace('\'', r"'\''")))
            }
        }
        Quoting::Windows => {
            if path.chars().all(windows_bare) {
                Some(path.to_string())
            } else {
                Some(format!("\"{path}\""))
            }
        }
    }
}

/// Characters a POSIX shell reads literally, so a tidy path stays readable and editable
/// rather than being wrapped in quotes it never needed. Deliberately conservative: `~`
/// and `-` are bare only because they are not leading (a bare path always starts with `/`
/// or a name), and everything non-ASCII is literal to `sh`.
#[tracing::instrument(level = "debug", ret)]
fn posix_bare(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || !c.is_ascii()
        || matches!(c, '_' | '-' | '.' | '/' | ',' | ':' | '@' | '+' | '=' | '%')
}

/// Same idea for `cmd`, which additionally treats `%` and `!` as expansion.
#[tracing::instrument(level = "debug", ret)]
fn windows_bare(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || !c.is_ascii()
        || matches!(
            c,
            '_' | '-' | '.' | '/' | '\\' | ':' | '@' | '+' | '=' | ','
        )
}

/// Render a batch as the text to insert: quoted words, space separated, with a trailing
/// space so whatever the user types next is a new word. Returns `(text, refused)`.
#[tracing::instrument(level = "debug", ret)]
pub fn format_paths(paths: &[String], style: Quoting) -> (String, usize) {
    let mut out = String::new();
    let mut refused = 0usize;
    for p in paths {
        match quote(p, style) {
            Some(q) => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&q);
            }
            None => refused += 1,
        }
    }
    if !out.is_empty() {
        out.push(' ');
    }
    (out, refused)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tidy_path_is_inserted_bare() {
        // The commonest drop by far. Quoting it would work but would be noise the user then
        // has to edit around.
        assert_eq!(
            quote("/Users/me/code/main.rs", Quoting::Posix).unwrap(),
            "/Users/me/code/main.rs"
        );
        assert_eq!(
            quote(r"C:\Users\me\notes.md", Quoting::Windows).unwrap(),
            r"C:\Users\me\notes.md"
        );
    }

    #[test]
    fn a_space_or_a_shell_metacharacter_forces_quotes() {
        assert_eq!(
            quote("/tmp/my file.txt", Quoting::Posix).unwrap(),
            "'/tmp/my file.txt'"
        );
        assert_eq!(
            quote("/tmp/a;rm -rf ~", Quoting::Posix).unwrap(),
            "'/tmp/a;rm -rf ~'"
        );
        assert_eq!(
            quote("/tmp/$(whoami)", Quoting::Posix).unwrap(),
            "'/tmp/$(whoami)'"
        );
        assert_eq!(
            quote(r"C:\Program Files\x.exe", Quoting::Windows).unwrap(),
            "\"C:\\Program Files\\x.exe\""
        );
    }

    #[test]
    fn an_embedded_single_quote_is_closed_escaped_and_reopened() {
        // The classic sh idiom; anything less lets the rest of the name out of the quotes.
        assert_eq!(
            quote("/tmp/it's here", Quoting::Posix).unwrap(),
            r"'/tmp/it'\''s here'"
        );
    }

    #[test]
    fn a_newline_in_a_filename_is_refused_not_mangled() {
        // Legal on Unix, and the one character that would submit the line it lands in.
        assert!(quote("/tmp/a\nb", Quoting::Posix).is_none());
        assert!(quote("/tmp/a\rb", Quoting::Posix).is_none());
        assert!(quote("/tmp/esc\u{1b}[2J", Quoting::Posix).is_none());
        assert!(quote("", Quoting::Posix).is_none());
    }

    #[test]
    fn non_ascii_names_stay_bare() {
        assert_eq!(
            quote("/tmp/café/日本語.txt", Quoting::Posix).unwrap(),
            "/tmp/café/日本語.txt"
        );
    }

    #[test]
    fn a_batch_joins_with_spaces_and_ends_with_one() {
        let (text, refused) = format_paths(&["/a/b".into(), "/c d".into()], Quoting::Posix);
        assert_eq!(text, "/a/b '/c d' ");
        assert_eq!(refused, 0);
    }

    #[test]
    fn refused_paths_are_counted_and_the_rest_still_land() {
        let (text, refused) = format_paths(&["/a/b".into(), "/bad\nname".into()], Quoting::Posix);
        assert_eq!(text, "/a/b ");
        assert_eq!(refused, 1);
    }

    #[test]
    fn a_batch_of_only_refused_paths_inserts_nothing() {
        let (text, refused) = format_paths(&["/x\ny".into()], Quoting::Posix);
        assert!(text.is_empty(), "no trailing space with nothing to trail");
        assert_eq!(refused, 1);
    }

    // ---- batching ----

    fn at(secs_ago: u64) -> Dropped {
        Dropped {
            win: 1,
            at: Some((0, 0)),
            path: PathBuf::from("/x"),
            when: Instant::now() - Duration::from_millis(secs_ago),
        }
    }

    #[test]
    fn a_batch_still_arriving_is_held_whole() {
        // One drag reports one event per file. Delivering the first three because a tick
        // boundary fell mid-burst would type the path list twice.
        let mut q = vec![at(500), at(500), at(0)];
        assert!(split_settled(&mut q, Instant::now(), SETTLE).is_empty());
        assert_eq!(q.len(), 3, "nothing is consumed while it is still growing");
    }

    #[test]
    fn a_settled_batch_is_released_in_one_piece() {
        let mut q = vec![at(500), at(490), at(480)];
        assert_eq!(split_settled(&mut q, Instant::now(), SETTLE).len(), 3);
        assert!(q.is_empty());
    }

    #[test]
    fn an_empty_queue_is_never_a_delivery() {
        let mut q: Vec<Dropped> = Vec::new();
        assert!(split_settled(&mut q, Instant::now(), SETTLE).is_empty());
    }
}
