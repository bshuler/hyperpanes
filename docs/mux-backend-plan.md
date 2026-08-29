# Multiplexer-backed panes — tmux/psmux under every terminal

**Goal.** Every hyperpanes pane is a real tmux session. That single change buys four
things at once:

1. **Upgrade-in-place without dropping terminals.** The shells and agents are children of
   the *tmux server*, not of hyperpanes. The GUI and the session daemon can both be killed,
   replaced with a new binary, and relaunched; the client processes never see it — their
   controlling tty is the tmux pane pty, which never closes.
2. **A terminal on your phone, with apps that already exist.** A pane is just
   `tmux -L hyperpanes attach -t hp-<id>` over SSH, so every mobile SSH client works with
   no hyperpanes-specific code — and Blink/iTerm2 get native tabs via control mode.
3. **Saved workspaces become live things.** A saved workspace records its session names, so
   "load workspace" is *reattach-or-spawn*, not always-respawn.
4. **State discovery on launch.** Surviving sessions are enumerable (`list-sessions`), so a
   cold start can re-adopt whatever is still running, including sessions no snapshot knows about.

This plan supersedes nothing — it **layers under** `docs/session-daemon-plan.md`, which is
already built and default-on.

## Where this sits in what already exists

```
GUI (Slint)  ── unchanged ──>  SessionManager (API unchanged)
                                  │  framed proto over UDS / named pipe
                                  ▼
                              hyperpanesd            ← survives GUI crash (built today)
                                  │  SessionBackend trait  ← NEW seam (M0)
                    ┌─────────────┴─────────────┐
                    ▼                           ▼
              PtyBackend                   MuxBackend                ← NEW
        portable-pty → shell         portable-pty → `tmux attach`
        (today; the fallback)                 │
                                              ▼
                                    tmux/psmux server        ← survives EVERYTHING
                                    owns the real shells       but a reboot
```

The daemon keeps its protocol, its 128 KB replay buffer, its headless screen mirror and its
control-API wiring. All that changes is **what its PTY runs**: a mux client instead of a shell.
Nothing above `SessionManager` is touched.

### Why this is tractable here

The session-daemon work already did the hard architectural part. Its own plan says it: the GUI
grid is fed purely by `Data` events plus a `replay()` seed, and "a reconnecting *process* is the
same operation as a reconnecting *window*." Adopting a tmux session is that same operation a
third time. The seams are already load-bearing.

## Verified against real tmux (3.7b)

Six assumptions this design rests on, each probed rather than assumed:

| Assumption | Result |
|---|---|
| `window-size latest` keeps the desktop authoritative when a phone attaches at 80×24 | **Confirmed** — and it is already the default in 3.7 |
| `prefix` is settable **per session**, so desktop panes can have no prefix while mobile views keep `C-b` | **Confirmed** — `set -t pane1 prefix None` and `set -t tabview prefix C-b` coexist on one server |
| `new-session -e VAR=VAL` reaches the child (needed for `HYPERPANES_PANE_ID`, control file) | **Confirmed** — child's `printenv` returned the injected value |
| `link-window` can project a pane's window into a per-tab session for mobile | **Confirmed** |
| `capture-pane -p -e` returns SGR-preserved text, for seeding a grid on adopt | **Confirmed** — `^[[31mRED^[[39m plain` |
| One batched `list-panes -a -F '#{pane_current_path}'` recovers every pane's cwd in a single call | **Confirmed** |

Note `allow-passthrough` defaults to **off** — the OSC strategy below must turn it on explicitly.

### The prefix trick (the thing that makes this pleasant)

`prefix` being session-scoped is what resolves the obvious objection. hyperpanes owns
`Ctrl+Shift` chords and must not hand keys to tmux; a phone needs a working prefix to navigate.
So:

* pane sessions (`hp-<pane_id>`) — `prefix None`, `status off`. tmux is invisible plumbing.
* mobile tab views (`hpt-<tab_id>`) — `prefix C-b`, `status on`. tmux behaves like tmux.

Same server, same windows, two different personalities depending on who attached.

## Naming and identity

| Thing | Name | Notes |
|---|---|---|
| Server socket | `-L hyperpanes-<salt>` | salt = user-data dir, matching the existing single-instance salting so a dev instance never collides with the installed app |
| Pane session | `hp-<pane_id>` | **must be the durable pane id**, not the process-global `PANE_UID` counter — see M2 |
| Mobile tab view | `hpt-<tab_id>` | windows are `link-window`s of the member panes |
| Shipped config | `resources/tmux/hyperpanes.conf`, passed with `-f` | never reads the user's `~/.tmux.conf` for pane sessions |

## Milestones

Each is one branch and one PR, in the repo's existing fan-out style.

### M0 — backend seam + mux driver (headless)
`core::session::mux`: a `Mux` trait (`new_session`, `attach_argv`, `list_sessions`,
`capture_pane`, `kill_session`, `cwds`) plus a `TmuxCli` implementation, a version/capability
probe, session naming, and the shipped config. Extract today's PTY path behind the same trait so
both are selectable. Pure `core` — testable with a real tmux on a temp socket, no GUI.

### M1 — daemon runs the mux backend behind `HYPERPANES_MUX=1`
Daemon session creation spawns `tmux new-session -A -s hp-<id>` in its PTY; `kill` becomes
`kill-session`. Replay/screen/control paths unchanged. **Bench keystroke→echo against the
in-process path** — this adds a second terminal emulator to the pipeline and that cost must be
measured, not assumed.

### M2 — durable pane ids + reattach on launch
Pane ids today are a process-global counter (`state.rs::PANE_UID`); they must become stable
across GUI restarts or nothing can be re-adopted. Persist id + mux session name in the snapshot
(`PaneSpec` already carries a `uid` field as precedent — add `mux`). On start: `list-sessions` →
attach survivors → seed the grid from `capture-pane`. **This is the upgrade-survival demo.**

### M3 — shell integration through tmux
The open risk (below). Wrap hyperpanes' own OSC emissions in tmux passthrough inside
`resources/shell-integration/hp-init.sh` / `hp-init.ps1` when `$TMUX` is set, turn on
`allow-passthrough`, and keep the batched `list-panes -a` cwd poll as the fallback for shells
without integration. Parity tests for cwd, the idle-agent glow, and `awaitingInput`.

### M4 — mobile attach
Per-tab `link-window` sessions, the session-scoped prefix/status options, and a
`hyperpanes mobile` CLI extending the existing `pair` QR path — emitting SSH, `tmux attach`, and
`tmux -CC` forms. Per-app docs: Blink, Termius, Prompt, iSH, a-Shell, Termux, ConnectBot,
JuiceSSH. Tailscale-first security guidance, reusing the model already written down in
`docs/mobile-client-plan.md`.

### M5 — left slide-out panel
`ui/leftpanel.slint` + `app/src/leftpanel.rs`, mounted as a **sibling** of the pane area like the
existing right-hand rail (not the overlay slot — no scrim, never dims the panes). Toggled by a
`Command` (Seam #2), projected in `paneview::resync` (Seam #1). Three sections: the
workspace tree (tabs → panes, click-to-focus, drag to move between tabs, liveness dots from the
existing activity data), the saved-workspace library, and **detached sessions** — live mux
sessions no window is showing, one click to adopt.

### M6 — workspace library and sets
A `WorkspaceSet` model (`sets/*.json`: a name plus member workspace references) on top of the
existing `WorkspaceFile`. `SaveWorkspaceAs` / `SaveSet` / `OpenSet`. Loading becomes
reattach-or-spawn per pane, using the `mux` field from M2.

### M7 — upgrade flow, default on
Updater pre-flight snapshot → kill daemon → install → relaunch → re-adopt. The daemon's existing
proto-version-mismatch path (kill and respawn the daemon) becomes *safe* for the first time,
because the sessions are no longer inside it. Flip `HYPERPANES_MUX` on by default, keep
`--no-mux`.

### M8 — Windows via psmux
[psmux](https://github.com/psmux/psmux) is a Rust, ConPTY-native, tmux-command-compatible
multiplexer (MIT) with a client/server model and control-mode support. It plugs into the M0
`Mux` trait as a second implementation. **Its per-command compatibility is unverified** — M8
starts with a capability probe that falls back to today's PTY backend when a required command is
missing, so an incompatibility degrades to current behaviour instead of breaking Windows.

## Risks and open questions

* **OSC 7 / OSC 133 forwarding through tmux is UNVERIFIED.** The probe that would have settled
  it needs a pty that the sandbox refused to allocate. This matters: cwd tracking
  (`session/cwd.rs`) and the shell-integration marks that feed the idle-agent glow and
  `awaitingInput` are OSC-based, and tmux is known to consume OSC 7 for its own
  `pane_current_path` rather than pass it on. M3 must settle it in a real terminal. The
  fallback is proven — one batched `list-panes -a` call returns every cwd — so cwd survives
  either way; the prompt marks are the part genuinely at risk.
* **Double emulation.** tmux parses the shell's output, then hyperpanes parses tmux's. Two VTEs
  where there was one, on every byte. M1 benches it.
* **Scrollback gets two owners.** tmux keeps `history-limit` lines; hyperpanes keeps a 128 KB
  replay buffer. Pick one as authoritative for the widget or they will disagree after a reattach.
* **Version floor.** `-e` env injection needs tmux ≥3.2. The probe in M0 must refuse politely,
  not misbehave, below it.
* **`capture-pane` fidelity for alternate-screen apps.** Seeding a grid mid-vim is the case to
  test.
* **tmux must be installed.** Consider bundling on macOS rather than a hard runtime error.
* **psmux is an unproven external dependency** on the Windows leg — hence the probe-and-fallback
  posture in M8.

## Fan-out

| Track | Branch | Depends on |
|---|---|---|
| M0 mux driver | `mux/m0-driver` | — |
| M2-prep durable pane ids | `mux/prep-pane-ids` | — |
| M1 daemon backend | `mux/m1-daemon` | M0 |
| M2 reattach | `mux/m2-reattach` | M1, prep |
| M3 shell integration | `mux/m3-osc` | M1 |
| M4 mobile | `mux/m4-mobile` | M1 |
| M5 left panel | `mux/m5-panel` | — (UI only; adoption list needs M2) |
| M6 workspace sets | `mux/m6-sets` | M2 |
| M7 upgrade + default on | `mux/m7-default` | M2, M3 |
| M8 Windows psmux | `mux/m8-psmux` | M0 |

**Wave 1 (parallel now):** M0 ‖ prep ‖ M5 — no shared files.
**Wave 2:** M1. **Wave 3:** M2 ‖ M3 ‖ M4 ‖ M8. **Wave 4:** M6, M7.
