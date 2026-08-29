# hyperpanesd as the multiplexer — upgrade survival, mobile attach, panel, workspace sets

**Decision (2026-08-29):** no tmux. `hyperpanesd` *already is* a multiplexer server written in
Rust — it owns the PTYs, keeps a session registry, replay buffers and a headless screen mirror,
and does attach/detach over a framed UDS/named-pipe protocol. Rather than layer a second
multiplexer under it, we grow it into the one we need.

An earlier revision of this document planned to run tmux under the daemon. That was rejected
because:

* **psmux is Windows-only** ("Windows 10 or Windows 11" required) — it does not remove the tmux
  dependency on macOS/Linux, it only covers the Windows leg.
* **No cross-platform Rust tmux exists.** `tmux-rs` is ~81k lines of *unsafe* Rust, 5 months
  stale, hobby-status, non-standard license. Zellij is mature but not tmux-compatible and
  unix-only. WezTerm's mux is mature and cross-platform but speaks its own protocol and is an
  enormous dependency.
* Layering tmux cost us **double emulation** (two VTEs on every byte) and an **unresolved OSC
  7/133 forwarding risk** that threatened cwd tracking and the idle-agent glow. Owning the
  pipeline deletes both problems outright.

## Target architecture

```
GUI (Slint) ── SessionManager, API unchanged ──┐
                                                │ framed proto over UDS / named pipe
                                                ▼
                                          hyperpanesd
                    ┌───────────────────────────┼───────────────────────────┐
                    ▼                           ▼                           ▼
             SessionRegistry            SSH server (russh)          live-upgrade takeover
        PTYs · replay · screen · cwd     per-device keys              PTY fd handoff
              [exists today]             [new — M3]                  [new — M1]
                    │                           │
                    │                           ▼
                    └──────────────────> attach client  ──> any mobile SSH app
                                          [new — M2]        (Termux/Blink/ConnectBot/…)
```

One process owns every terminal. The GUI is a client. A phone is a client. An upgrade replaces
the process underneath both without the shells noticing.

### The two gaps we are actually filling

Persistence is already solved — the daemon survives a GUI crash today. Only two things were
missing, and neither needs tmux:

1. **Surviving a daemon upgrade.** Solved by handing the PTY master fds to the successor process
   (`SCM_RIGHTS` on unix — the nginx/systemd live-binary-upgrade trick). A shell dies from
   `SIGHUP` when its *pty closes*, not when its parent exits, so as long as the successor holds
   the master fd open, nothing downstream notices.
2. **Mobile attach.** Mobile "tmux apps" are SSH clients that will run any command. They need a
   terminal-drawing attach client, which is what tmux's client does and what M2 builds.

## Mobile: why an embedded SSH server

The requirement is to support mobile apps that **already exist** — not to ship another app. Every
free, open-source mobile terminal speaks SSH:

| App | Platform | License | Stars |
|---|---|---|---|
| Termux | Android | free / OSS | 60.0k |
| Blink Shell | iOS | GPL-3.0 | 6.9k |
| ConnectBot | Android | Apache-2.0 | 3.4k |
| Rootshell | iOS | MIT | — |

There is no Rust mobile terminal app — nobody has written one. But [russh](https://github.com/Eugeny/russh)
(Apache-2.0, 1.8k stars, 30 contributors, actively maintained, forked by Microsoft for VS Code) is
a mature Rust SSH **server** library. So the free/open-source/Rust trifecta lands on *our* side of
the wire: we implement the protocol, and every one of those apps connects unmodified.

Embedding it rather than leaning on the system sshd means no external dependency (consistent with
the pure-Rust decision), no sshd config, and — because we own the channel — a connection can drop
**straight into the attach client with no shell in between**.

## Milestones

### M0 — durable pane ids ✅ *already built*
An earlier draft listed this as prep work. It is done: `SessionManager::fresh_uid`
(`session_manager.rs:605`) mints a `pane-<uuid>` on the daemon backend precisely so a session can
outlive the GUI run and be re-attached by uid, and `daemon_client.rs:1291`
(`daemon_fresh_uid_is_unique_across_runs`) asserts cross-run uniqueness. `PaneSpec.uid` carries it
through the relaunch snapshot. Nothing to do.

### M1 — daemon live upgrade via PTY fd handoff  ← *the headline, and the whole gap*

**The exact failure today.** Sessions already survive a *GUI* upgrade: quit with keep-alive on
leaves the daemon running (`main.rs:575`) and a relaunch re-attaches. What they do not survive is a
*daemon* upgrade. `DaemonClient::new` (`daemon_client.rs:147`) does a `Hello` probe and, when the
running daemon's `PROTO_VER` differs from the new binary's, calls `tear_down_stale_daemon` — which
sends `Shutdown`, **killing every session** — then respawns. The doc comment states the intent
plainly: *"lock-step upgrades — no third-party compat burden."* So an upgrade drops every terminal
the moment it bumps `PROTO_VER` (`proto.rs:41`, currently `1`).

**The fix, at that same call site.** Replace the tear-down with a takeover: the incumbent daemon
hands every PTY master fd plus its session metadata (replay buffer, screen state, cwd, uid) to the
freshly spawned daemon over `SCM_RIGHTS`, then exits. A shell dies from `SIGHUP` when its *pty
closes*, not when its parent exits, so nothing downstream notices. One seam, one replaced function.

**Consequence to handle:** after handoff the children are reparented to init and are no longer
waitable, so **exit detection must move from `waitpid` to pty EOF**. Needs a test covering exit
codes.

Headless-testable end to end: start daemon, run a session, take over, assert the session still
streams.

### M2 — `hyperpanes attach` terminal client
A CLI that renders a pane into whatever terminal it is running in: seed from the replay buffer,
stream live output, forward stdin, handle `SIGWINCH`, and a detach key. This is the tmux-client
equivalent and the piece every mobile path depends on. Usable over a stock system sshd immediately,
before M3 exists.

### M3 — embedded SSH server (russh)
Listener inside the daemon; per-device public keys reusing the existing `device-tokens.json` +
`hyperpanes pair` QR flow. An opened channel runs the M2 client directly — no shell. A pane chooser
when none is named. **Binds loopback by default**; remote exposure follows the Tailscale-first model
already written down in `docs/mobile-client-plan.md`.

### M4 — tmux control-mode surface *(optional)*
Speak `-CC` on the SSH channel so Blink and iTerm2 render panes as **native tabs** rather than a
full-screen terminal. It is a simple line-based text protocol — the one place literal tmux
compatibility is cheap and buys something real.

### M5 — left slide-out panel
`ui/leftpanel.slint` + `app/src/leftpanel.rs`, mounted as a **sibling** of the pane area like the
existing right-hand rail (not the overlay slot — no scrim, never dims the panes). Toggled by a
`Command` (Seam #2), projected in `paneview::resync` (Seam #1). Three sections: the workspace tree
(tabs → panes, click-to-focus, drag between tabs, liveness dots from existing activity data), the
saved-workspace library, and **detached sessions** — live sessions no window is showing, one click
to adopt.

### M6 — workspace library and sets
A `WorkspaceSet` model (`sets/*.json`: a name plus member workspace references) on top of the
existing `WorkspaceFile`. `SaveWorkspaceAs` / `SaveSet` / `OpenSet`. Loading becomes
**reattach-or-spawn** per pane, using the durable ids from M0.

### M7 — discovery and adopt *(mostly built)*
Launch-time discovery and re-adoption already exist — `ListSessions` / `Attach` are in the protocol
(`proto.rs:146,149`), `App::attach_panes_from_specs` (`app.rs:1451`) rebinds snapshot panes to
surviving sessions, and the daemon backend is default-on. What remains is small: surface the
sessions the snapshot does *not* claim in M5's detached list so an orphan can be adopted with one
click.

## Risks and open questions

* **ConPTY fd handoff on Windows is UNVERIFIED.** ConPTY's pipe handles are duplicable via
  `DuplicateHandle`, but whether pseudoconsole (`HPCON`) *ownership* transfers cleanly across
  processes is the open question. M1 must prove it on Windows or fall back to "keep the old daemon
  running for old sessions" there.
* **An embedded SSH server is a real security surface** — it owns every terminal and every
  credential typed into one. Loopback-only default, explicit opt-in to any wider bind, per-device
  revocable keys, and no password auth.
* **Exit detection changes semantics** after handoff (pty EOF, not `waitpid`). Zombie/exit-code
  reporting needs a test.
* **Mobile resize contention.** An attach client must not reflow the desktop; decide the policy
  (attach at the desktop's grid and letterbox, vs. explicit resize request) before M2 ships.
* **russh pulls in a crypto backend** (`aws-lc-rs` or `ring`) — a new dependency class for this repo.
* **No Rust mosh implementation exists**, so mobile roaming/high-latency behaviour will be worse
  than mosh until someone writes one. Future item.
* **What we give up by dropping tmux:** literal `tmux attach`, users' own `.tmux.conf`, and tmux
  muscle memory against hyperpanes panes. Accepted — M4 recovers the part that matters for Blink
  and iTerm2.

## Fan-out

| Track | Branch | Depends on |
|---|---|---|
| M1 live upgrade | `mux/m1-takeover` | — |
| M2 attach client | `mux/m2-attach` | — |
| M3 embedded SSH | `mux/m3-ssh` | M2 |
| M4 control mode | `mux/m4-ccmode` | M2 |
| M5 left panel | `mux/m5-panel` | — (adoption list needs M7) |
| M6 workspace sets | `mux/m6-sets` | — |
| M7 orphan adoption | `mux/m7-adopt` | M5 |

**Wave 1 (parallel now):** M1 ‖ M5 ‖ M6 — no shared files.
**Wave 2:** M2 ‖ M7. **Wave 3:** M3 ‖ M4.
