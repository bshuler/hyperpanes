# `hyperpanes ctl` reference

Every verb is a thin wrapper over one HTTP route on the loopback control API. Machine verbs print
JSON on stdout; `tabs`, `panes` and `read` print text, because their job is to be read. A non-2xx
response prints to stderr and exits 1; a usage error exits 2.

Call it as `hyperpanes ctl <verb>`.

## Discovery

| Verb | Route | Notes |
|---|---|---|
| `health` | `GET /health` | Is the server up, and does it allow input |
| `state` | `GET /state` | The whole windows→tabs→panes tree, as JSON |
| `tabs` | `GET /state` | The same tree as an outline; `*` marks the active tab |
| `panes` | `GET /state` | One tab-separated line per pane: `id · tab · status · label` |
| `settings` | `GET /settings` | Live preferences as camelCase JSON |

`/state` shape:

```json
{"windows":[{"windowId":1,"activeTabId":"1:0","tabs":[
  {"id":"1:0","title":"Tab 1","layout":"auto","panes":[
    {"id":"p1","sessionUid":"u1","label":"shell","color":"#3b82f6",
     "status":"running","activity":"busy"}]}]}]}
```

Optional pane fields are omitted when unset: `subtitle`, `command`, `args`, `cwd`, `shell`,
`exitCode`, `meta`, `talk`, `kind`. `status` is `running` or `exited`; `activity` is `busy`,
`idle` or `exited`, and a pane sitting at a prompt waiting for you reports `awaiting-input` in
`/panes/{id}/output`.

## Terminals

```
read <pane> [--tail N] [--raw] [--screen] [--wait]
```

`GET /panes/{id}/output`. Defaults to ANSI-stripped scrollback. `--raw` keeps the escape
sequences, `--screen` renders what is on screen right now instead of scrollback, `--tail N` keeps
the last N lines, `--wait` blocks until the pane settles before answering.

```
send   <pane> <text…>      # POST /panes/{id}/input  {"data": …}
submit <pane> <text…>      # the same, plus {"submit": true}
keys   <pane> <key>…       # POST /panes/{id}/input  {"keys": […]}
```

`submit` sends the carriage return as a separate write so bracketed-paste TUIs read it as Enter.
Key names are case-insensitive: `enter`/`return`, `escape`/`esc`, `tab`, `shift+tab`/`backtab`,
`up`, `down`, `left`, `right`, `home`, `end`, `pageup`/`pgup`, `pagedown`/`pgdn`, `insert`,
`delete`/`del`, `backspace`, `space`, and `ctrl+<a-z>` for any control code. An unknown name is
reported back — all of them at once, not just the first.

`403` means the control API's **allow input** permission is off — Preferences → Control API.
`423` means another client holds the pane's advisory lock.

## Panes

| Verb | `/command` body |
|---|---|
| `new-pane [--cwd D] [--cmd C] [--label L] [--color #rrggbb] [--shell S] [--project P] [--window N]` | `{"type":"newPane","pane":{…}}` |
| `close-pane <pane>` | `{"type":"closePane","paneId":…}` |
| `restart-pane <pane>` | `{"type":"restartPane","paneId":…}` |
| `focus-pane <pane>` | `{"type":"focusPane","paneId":…}` |
| `rename-pane <pane> <title>` | `{"type":"renamePane","paneId":…,"label":…}` |
| `recolor-pane <pane> <#rrggbb>` | `{"type":"recolorPane","paneId":…,"color":…}` |
| `layout <tab> <name>` | `{"type":"setLayout","tabId":…,"layout":…}` |

A new pane lands in the target window's **active** tab. The spawn spec must be nested under
`pane` — the server rejects a flat one rather than silently spawning a default shell, and `ctl`
nests it for you.

Layout names: `auto`, `single`, `columns`, `rows`, `grid`, `main-stack`, and `grid-<cols>x<rows>`
(e.g. `grid-2x3`). An unknown name falls back to `auto`.

`new-pane` returns the new pane's id as the command result.

## Tabs

| Verb | `/command` body |
|---|---|
| `new-tab [--window N] [--title T] [--cwd D]` | `{"type":"newTab",…}` |
| `close-tab <tab>` | `{"type":"closeTab","tabId":…}` |
| `rename-tab <tab> <title>` | `{"type":"renameTab","tabId":…,"title":…}` |
| `focus-tab <tab>` | `{"type":"focusTab","tabId":…}` |
| `move-tab <tab> <index>` | `{"type":"moveTab","tabId":…,"to":N}` |

All five answer **202** with `{"ok":true,"queued":true}` — they are applied by the UI thread on
its next frame. `newTab` also returns the id the new tab will have (`{window}:{count}`, appended
at the end). Re-read `tabs` to confirm anything else.

Tab ids are positional (`{window}:{index}`), so they shift when tabs close or move. A stale id
that no longer names a tab is refused with 404 rather than hitting whatever moved into that slot.

The Hyperpane tab is the app's own and refuses to close.

`move-tab`'s index is an insertion slot in the pre-move ordering, so `move-tab 1:3 0` puts the tab
first and an index equal to the tab count puts it last.

## Preferences

```
settings                 # GET /settings
set <key> <value>        # PATCH /settings {"<key>": <value>}
set-json <json>          # PATCH /settings <json>
```

`set` parses the value as JSON if it can and sends it as a string otherwise, so `set fontPx 15`
sends a number and `set defaultShell zsh` sends a string. Quote a value that is itself JSON-ish if
you mean it literally.

The patch is a shallow merge, validated key-by-key against the live settings. An unknown key
fails the whole patch — nothing is applied. Out-of-range numbers are clamped (font size to its
allowed range, the idle-alert threshold onto the dial's grid, palette indices into range) rather
than refused. Side effects run immediately: a font change reloads and re-bases every pane, a
palette change relabels every tab, a theme change repaints every pane.

`GET /settings` is the authoritative list of keys. As of writing it includes `fontFamily`,
`fontPx`, `framePalette`, `terminalTheme`, `defaultShell`, `showFrame`, `showDot`,
`clickablePaths`, `editorCommand`, `scrollback`, `showSidebar`, `idleAlert`, `idleEffect`,
`idleAlertSeconds`, `autoUpdate`, `copyOnSelect`, `keepAlive`, `toolFavorites`, `toolPaths`,
`browserMode`, `browserApp` and `confirmClose` — but read it rather than trusting this list.

`503` means no GUI is attached, so there are no live settings to serve.

## Raw passthroughs

```
get   <path>
post  <path> [json]
patch <path> [json]
command <json>            # sugar for: post /command <json>
```

Routes not covered by a named verb:

- `GET /projects`, `GET /projects/{id}` — the remembered git projects.
- `GET /queues`, `GET /queues/{queue}/tasks`, `POST /queues/{queue}/claim`,
  `POST /queues/{queue}/purge`, `GET /tasks/{id}`, `POST /tasks/{id}/ack|nack|extend` — the
  durable work queue.
- `GET /tokens`, `GET /devices` — scoped tokens and paired mobile clients.
- `GET /fs/read?path=…` — read a file through the app.
- `POST /panes/{id}/lock`, `GET /panes/{id}/messages` — the advisory pane lock and the pane
  message log.
- `/command` verbs with no `ctl` sugar: `attach` (restore a whole group), `recoverPane`,
  `setMeta` (orchestration metadata), `readScreen`, `setTalk` (speak a pane's replies aloud),
  `restartApp`.
- `GET /events` is a WebSocket, so `ctl` cannot speak it; poll `state` instead.

## Exit codes and errors

`0` success · `1` the server refused, or is not running · `2` bad usage.

"Start hyperpanes and enable Preferences → Control API" means the discovery file
(`control.json`) is missing — the API is off. That is a setting for the user to change; say so
rather than working around it.

Tab and settings writes require a **root** token; a scoped token gets 403.
