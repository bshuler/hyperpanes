---
name: hyperpanes
description: Inspect and control the running Hyperpanes workspace — list windows, tabs and panes, read what any terminal is showing (even on a tab that is not on screen), type into or submit commands to any terminal, create/close/rename/recolor/re-layout panes, create/close/rename/reorder tabs, and read or change any application preference. Use whenever the user asks about "this window", "that pane", "the other tab", what a terminal is doing, or asks to change how Hyperpanes looks or behaves.
---

# Hyperpanes control

You are running inside the **Hyperpane** tab of a live Hyperpanes workspace. You can see and
change the whole thing through one command:

```
hyperpanes ctl <verb> [args]
```

That is the Hyperpanes binary itself. This pane's `PATH` carries the app's own directory, so the
bare name resolves here even when Hyperpanes isn't installed system-wide; `$HP_CTL` is the same
binary's absolute path, for the rare shell that loses the `PATH`.

Two reference files sit beside this one:

- `REFERENCE.md` — every verb, its arguments, and the HTTP route underneath it.
- `RECIPES.md` — worked examples for the things that come up most.

## Start here, every time

Ids change. Before acting on a pane or tab, get the current ones:

```
hyperpanes ctl tabs
```

That prints the whole workspace as an outline: one line per tab (with its id, title and layout,
`*` marking the active one) and one indented line per pane (id, label, status).

If it fails with "Start hyperpanes and enable Preferences → Control API", the control API is off.
Tell the user, and point them at **Preferences → Control API** — do not try to work around it.

## The ids

- A **pane id** is opaque and stable for the pane's life. Take it from `tabs` or `panes`.
- A **tab id** is `"{window}:{index}"` and is **positional**. Closing or reordering tabs renumbers
  the ones after it. Re-read `tabs` after any change before you use a tab id again.

## Reading a terminal

```
hyperpanes ctl read <pane> [--tail 60] [--screen] [--wait] [--raw]
```

The pane's text goes to stdout, ANSI already stripped; the pane id and status go to stderr. Use
`--tail N` for the last N lines, `--screen` for what is literally on screen right now (rather than
scrollback), and `--wait` to block until the pane goes idle — that last one is how you read the
result of a command you just sent instead of catching it mid-run.

You can read **any** pane, including panes on tabs that are not on screen. Nothing has to be
focused first.

## Writing to a terminal

```
hyperpanes ctl submit <pane> git status      # type it and press Enter
hyperpanes ctl send   <pane> some text       # type it, leave the line unsent
hyperpanes ctl keys   <pane> ctrl+c          # named keys: enter escape tab up down ctrl+c …
```

Rules that matter:

- **Say what you are about to run, and in which pane, before you run it.** A pane is somebody's
  live terminal; a command you send lands in their shell, not in a sandbox.
- Never submit into a pane you have not just read. Read it first — it may be at a prompt inside
  `vim`, a REPL, an interactive installer, or another agent's session.
- `submit` is `send` plus Enter, sent as a separate write so full-screen TUIs read it as Enter and
  not as pasted text. Prefer it over sending `\n` yourself.
- After submitting, `read --wait` rather than sleeping.

## Changing the workspace

Panes: `new-pane`, `close-pane`, `restart-pane`, `focus-pane`, `rename-pane`, `recolor-pane`,
`layout`. Tabs: `new-tab`, `close-tab`, `rename-tab`, `focus-tab`, `move-tab`.

Tab verbs answer `202 Accepted` with `{"queued": true}`: they are applied by the UI thread on its
next frame, not while your command is still running. Re-read `tabs` to confirm, don't assume.

`close-tab` parks the tab rather than destroying it — its sessions stay alive and the user can get
it back with "Reopen closed tab". `close-pane` is not undoable; confirm with the user first.

You cannot close the Hyperpane tab. It is the app's own tab and the close is refused.

## Preferences

```
hyperpanes ctl settings                 # everything, as JSON
hyperpanes ctl set fontPx 15
hyperpanes ctl set defaultShell zsh
hyperpanes ctl set-json '{"showFrame":true,"terminalTheme":3}'
```

Keys are camelCase and are validated against the live settings — an unknown key is rejected and
**nothing** in the patch is applied, so a typo can never leave you half-changed. Values that a
slider could not produce (a 400px font, a 3-second idle alert) are clamped rather than refused.
Changes take effect live and are saved.

Read `hyperpanes ctl settings` before writing: it is the authoritative list of what exists.

## Anything else

The control API has more surface than the verbs above — projects, work queues, tasks, paired
devices, tokens. Reach it directly:

```
hyperpanes ctl get /projects
hyperpanes ctl post /command '{"type":"setMeta","paneId":"…","meta":{"role":"reviewer"}}'
```

See `REFERENCE.md` for the routes.

## Working style

- Prefer reading over acting. Most questions ("what is that pane doing?", "did the build finish?")
  are answered by `tabs` + `read`.
- One change at a time, then verify by reading back.
- Destructive or visible actions — closing a pane, sending a command into someone's shell, changing
  a preference the user did not ask about — get confirmed first.
- Report what you actually observed, including the pane id, so the user can check you.
