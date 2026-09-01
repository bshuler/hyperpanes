# Dropping files in, and getting text out

Two ways text crosses the boundary between the desktop and a pane: **drag a file in from
Finder/Explorer** and it becomes a quoted path at the prompt, and **copy/paste** moves a
selection through the OS clipboard.

**Status: BUILT** — OS file drop on macOS, Windows and X11; clipboard copy/paste on all three.
Wayland file drop is the one gap, and it is winit's (see *Platforms* below).

## Dropping a file into a pane

Drag one or more files onto a pane. Their paths are inserted at the cursor, shell-quoted and
separated by spaces, with a trailing space so the next thing you type doesn't run into the last
path. **Nothing is submitted** — no Enter is ever sent. You look at what landed and press Return
yourself.

A dropped path is untrusted text on its way to a live shell, so a path containing a control
character (a newline in a filename *is* Enter) is **refused, not mangled**, and the pane's toast
says how many were skipped.

### How a drop finds its pane

winit's `DroppedFile` event is unhelpfully thin: it carries a path and a window, one event per
file, with **no cursor position** and **no end-of-batch marker**. So `src/filedrop.rs`:

- samples the global pointer *inside the event hook*, at drop time — by the time the pump drains
  the queue the mouse has usually moved;
- treats the **window** from the event as authoritative and uses the cursor only to pick the pane
  within it (`App::compute_hover`), falling back to that window's focused pane when there is no
  pointer to read — which is also the correct Wayland answer;
- holds the batch until its newest member has been still for 60 ms, so a five-file drag can never
  be split across two insertions at a tick boundary.

The text is inserted through `TerminalPane::prepare_insert`, which is `prepare_paste` — the same
path a clipboard paste takes. A TUI that distinguishes pasted content from typing therefore sees a
drop the way it sees a paste, rather than as a burst of hand-typed keys.

### The winit hook multiplexer

Slint keeps exactly **one** `on_winit_window_event` filter per window: a second registration
silently replaces the first. Linux already used the slot (frameless re-strip + pointer tracking).
`src/winit_hooks.rs` is a thread-local `window handle -> Vec<Hook>` fan-out that both features
register through, so neither has to know the other exists.

## Copy and paste

| Gesture | macOS | Linux / Windows | What it does |
|---|---|---|---|
| Copy selection | Cmd+C | Ctrl+Shift+C | selection → OS clipboard |
| Paste | Cmd+V | Ctrl+V | fresh clipboard read, bracketed |
| Paste an image | Alt+V | Alt+V | forwards a literal 0x16 so an in-pane TUI reads the image itself |
| Right-click | paste | paste | same as the paste chord |
| Interrupt | **Ctrl+C** | Ctrl+C | 0x03 to the pty |

Paste goes through `prepare_paste`: newlines normalize to CR and, when the program asked for it,
the text is wrapped in bracketed-paste markers — so pasting a multi-line block into an editor
doesn't execute each line.

### The macOS modifier swap

Slint swaps Command and Control on Apple platforms (Qt's convention), so a `KeyMsg` arriving from
a Mac has **Command in `control` and physical Control in `meta`**. App chords ride the swapped
slot on purpose — Cmd+Shift+P opens the palette, which is what a Mac user expects, and it's why
`CTRL_LABEL` renders as "Cmd" in Preferences.

A terminal is the one place the swap is wrong. `crate::pty_ctrl` therefore takes the pty's control
modifier from the *physical* Control key on macOS, which puts both halves where a Mac user reaches
for them: **Ctrl+C interrupts, Cmd+C copies.** Cmd is then free to be the app's modifier
throughout.

## Platforms

| | file drop | clipboard |
|---|---|---|
| macOS | ✅ `NSFilenamesPboardType`, registered by winit at window creation | ✅ |
| Windows | ✅ `drop_handler` | ✅ |
| X11 | ✅ XDND via winit's event processor | ✅ |
| Wayland | ❌ winit emits no `DroppedFile` there | ✅ |

The Wayland gap is the same shape as the Wayland global-pointer gap: the drop is inert rather than
wrong, and the moment winit grows the event the existing hook picks it up with no change here.
