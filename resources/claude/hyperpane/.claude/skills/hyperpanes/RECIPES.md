# Recipes

Worked examples for the things that come up most. Every one starts from `tabs`, because ids move.

## "What is that pane doing?"

```
hyperpanes ctl tabs
hyperpanes ctl read p7 --tail 40
```

`tabs` gives the outline; the pane id is the first field on the indented lines. `--tail 40` is
almost always the right amount — enough to see the last command and its output, short enough to
read. Reach for `--screen` instead when the pane is running a full-screen program (an editor, a
pager, `top`): scrollback of a TUI is a mess of repaints, but the screen is what the user sees.

## "Did the build finish?"

```
hyperpanes ctl read p7 --wait --tail 20
```

`--wait` blocks until the pane goes idle, so this answers with the finished output instead of
catching the build mid-run. If the pane never settles, it is still building.

## Run a command in an existing terminal

```
hyperpanes ctl read   p7 --screen --tail 5     # 1. look before you type
hyperpanes ctl submit p7 cargo test            # 2. say what you are running, then run it
hyperpanes ctl read   p7 --wait --tail 40      # 3. read the result
```

Step 1 is not optional. A pane at a shell prompt takes `cargo test`; a pane sitting in `vim`, a
REPL, or another agent's session takes it as keystrokes into whatever that is.

## Interrupt something

```
hyperpanes ctl keys p7 ctrl+c
hyperpanes ctl read p7 --screen --tail 10
```

Answer a prompt the same way — `keys p7 enter`, `keys p7 escape`, `keys p7 down down enter`.

## Give a job its own pane instead

When the work is yours rather than the user's, don't borrow their terminal:

```
hyperpanes ctl new-pane --cwd ~/code/hyperpanes --cmd "cargo test" --label tests --color '#3b82f6'
```

The reply is JSON carrying the new pane's id. The pane lands in the window's **active** tab;
`--window N` picks a different window. Then `read <id> --wait`, and `close-pane <id>` when the user is done with it —
that one is not undoable, so ask first.

## Set up a tab for a task

```
hyperpanes ctl new-tab --title "release" --cwd ~/code/hyperpanes   # 202; reply carries the id
hyperpanes ctl tabs                                                 # confirm it exists
hyperpanes ctl focus-tab 1:4
hyperpanes ctl new-pane --cwd ~/code/hyperpanes --label build
hyperpanes ctl new-pane --cwd ~/code/hyperpanes --label logs
hyperpanes ctl layout 1:4 columns
```

Two things to be careful about. Tab writes return **202** — queued for the UI thread, not done —
so `tabs` between the create and the first use, not because it usually fails but because the id
you were handed is positional. And `new-pane` has no tab argument: it targets the window's active
tab, which is why `focus-tab` comes before the panes.

## Find the pane the user means

They will say "the one running the server", not "p7".

```
hyperpanes ctl panes
```

One line per pane — `id · tab · status · label` — across every window and tab, on screen or not.
Grep it for the label, then confirm by reading a few lines before you act. When two panes match,
ask; acting on the wrong terminal is not recoverable by re-reading it.

## Change a preference

```
hyperpanes ctl settings                  # the authoritative key list
hyperpanes ctl set fontPx 15
```

Read first. The patch is validated key-by-key and one unknown key fails the whole thing, so a
guessed name changes nothing — which is the good failure, but it is still a wasted turn. Values a
slider could not produce are clamped, not refused, so `set fontPx 400` silently lands at the
maximum: read back if the exact value matters.

## Reach something with no verb

```
hyperpanes ctl get /projects
hyperpanes ctl command '{"type":"setMeta","paneId":"p7","meta":{"role":"reviewer"}}'
```

`get`, `post`, `patch` and `command` are the raw passthroughs; `REFERENCE.md` lists the routes.
