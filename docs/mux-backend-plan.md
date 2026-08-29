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
        PTYs · replay · screen · cwd     per-device keys           unix: PTY fd handoff
              [exists today]             [new — M3]              Windows: pty-host stays put
                                                                        [M1 ✅]
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

### M1 — daemon live upgrade ✅ *built on every OS (unix: fd handoff; Windows: pty-host)*

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

**As built.** `PROTO_VER` is `2` and carries `ClientMsg::Takeover`. The successor daemon —
spawned from the new binary by `DaemonClient::new`'s mismatch branch — finds the salt's flock
held and, instead of bailing out, connects to the incumbent and asks for the handover. The
incumbent's `SessionRegistry::hand_off` snapshots every session (replay buffer, byte cursor,
grid size, cwd, pgrp), *relinquishes* rather than drops each pty (dropping one types EOT at the
shell it is meant to preserve), and `sendmsg`s the masters back as `SCM_RIGHTS` in
`HandoffPayload` chunks; then it unlinks its socket and exits, releasing the flock the successor
is waiting on. The successor's `adopt_all` rebuilds each session around its descriptor with
`SessionRegistry::adopt`. A pre-takeover incumbent cannot parse the request and drops the
connection — zero messages — which is why the incumbent always sends at least one chunk even
with no sessions: it makes "did not understand" distinguishable from "had nothing", and only the
former falls back to the old session-killing tear-down.

Tested by layer rather than end to end in one process: after `hand_off` the incumbent's reader
threads are still alive and steal bytes the successor never sees. Production closes that window
in microseconds by exiting; an in-process test daemon never exits, so the transfer is asserted at
the socket (`takeover_transfers_live_sessions_and_stands_the_incumbent_down` — the shell's process
group is still alive afterwards) and the re-creation in `session_manager`'s `adopt` tests.

**The Windows leg — a different answer to the same question.** A ConPTY is addressed by an
`HPCON`, an opaque pointer into the owning process's heap holding `hSignal`/`hPtyReference`/
`hConPtyProcess`. No documented API hands one to another process, and when the owner exits the OS
closes `hSignal`, at which point conhost kills the attached shell. So the fd-handoff shape is not
merely unproven on Windows — it is unavailable. The terminals cannot move.

The answer is that they never do: the ConPTYs live in a **pty-host** process that outlives daemon
upgrades, and the daemon in front of it is a thin proxy. A takeover then transfers *nothing* — the
successor re-attaches to the same running host, and the incumbent stands down without touching a
session (its `Takeover` arm must not `kill_all`, the one thing that would break this).

The host is deliberately **not** a new process mode. It is a daemon whose *salt* carries a
`\u{1}pty-host` marker suffix, so `pipe_name()` gives it a distinct endpoint for free and the
existing `--session-daemon <salt>` spawn path launches it. A daemon with a host salt runs
`SessionManager::InProcess`; any other salt proxies to the host via `SessionManager::Daemon`. No
new flag, no new protocol, no new spawn path.

Because the host is by design an *older build* than the daemon in front of it, that one link runs
`VersionPolicy::Tolerant` (the GUI→daemon link stays `LockStep`). What makes that safe is a written
contract — the **frozen host surface** documented on `VersionPolicy`: the host-facing messages may
gain optional fields and `SessionEvent` may gain variants, but no existing field may change name,
type or meaning.

One-daemon-per-salt on Windows is `first_pipe_instance(true)`, the race-free peer of the unix
flock: the OS grants the pipe name to exactly one server and returns `ERROR_ACCESS_DENIED` to the
rest, which `bind_first_instance` maps to `io::ErrorKind::AddrInUse` — the same kind the flock path
reports, so callers need no cfg. (This retired the old named-mutex plan: a mutex is only needed
when you detect by *connecting*, which races; we detect by *binding*, which does not.)

Tested in `windows.rs`'s own test module — the salt marker and its distinct endpoint, the
first-instance gate admitting one daemon per salt, a real ConPTY spawned and streamed
(`cmd.exe /c echo …`), and `takeover_stands_the_daemon_down_and_leaves_the_terminals_running`,
which asks the pty-host directly after the takeover and finds the session still there. Those run on
the `windows-latest` CI leg; they cannot run on a mac or Linux box, where `cargo xwin check --target
x86_64-pc-windows-msvc --all-targets` is the local gate.

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

### M7 — discovery and adopt ✅ *(built on `mux/m7-adopt`)*
Launch-time discovery and re-adoption already existed — `ListSessions` / `Attach` are in the
protocol, `App::attach_panes_from_specs` rebinds snapshot panes to surviving sessions, and the
daemon backend is default-on. M7 added the part that makes M5's DETACHED list safe with more than
one hyperpanes process running: **a cross-process claim registry**.

**The daemon is the registry** (`session/claims.rs`), not a file under the runtime dir. A file
registry would force every reader to judge staleness, and the only honest judgement is pid *plus*
process start time (bare pids are recycled) — per-OS code — with an `flock` protocol layered on top
for mutual exclusion, and crash safety left as every future reader's problem. The daemon is
already the single process that knows every session and that every hyperpanes process connects to,
so the claim map is ordinary in-memory state behind a `Mutex`: a real compare-and-set with exactly
one winner and no protocol to get wrong.

* **Wire (`PROTO_VER` → 3, additive):** `Claim{uid}` / `Release{uid}` / `ListClaims` client-side;
  `ClaimResult{uid,granted,owner}`, `Claims(Vec<ClaimInfo>)` and `SessionsChanged(Vec<SessionMeta>)`
  daemon-side, plus a `conn_id` on `Hello`. `Claims` and `SessionsChanged` are pushed as **full
  snapshots**, never deltas, so they are idempotent and a dropped or reordered one cannot desync a
  client. Pushes are gated on the peer having said `Hello`, and the connect-time seed is sent by
  *broadcasting* on the same ordered channel as later changes — so a snapshot can never overwrite a
  newer one, and the raw `Takeover` exchange (which never says `Hello`) is never interrupted.
* **Crash safety is connection scope, not a lease.** A claim belongs to a daemon-assigned
  `ConnId`, and the per-connection teardown releases every claim that connection holds. That
  teardown runs on socket EOF — which the kernel delivers when the owning process dies, however it
  dies. No heartbeat to miss, no expiry to tune, no pid to mistrust. Proved by a test that
  `SIGKILL`s a real child process and watches its claim drain.
* **No double adoption.** `adopt_detached_session` claims before it attaches and obeys the answer.
  Proved by a test that spawns **six real OS processes** (re-executions of the test binary), fires
  them at one wall-clock instant against one orphan, and asserts one `granted` and five `denied`
  — with all six connections still open, so the winner cannot have released and re-opened the race.
* **Shadow staleness fixed.** The client's shadow map used to be seeded by `ListSessions` at
  connect and then maintained only from the `Exit` stream plus local creates, so a session another
  client made after we connected stayed invisible until reconnect. It now reconciles against each
  pushed `SessionsChanged` snapshot (a `pending` flag protects a local create still in flight).
* `leftpanel::claimed_by_other_processes` — the M5 stub that returned an empty set — now answers
  from the pushed claim snapshot: a lock and a set filter, no I/O on the paint path.

Windows (`session/windows.rs`) mirrors all of this, but could not be compiled locally (the
`ring` build script fails under cross-compilation from macOS) and is unverified on that OS.

## Risks and open questions

* ~~**ConPTY fd handoff on Windows is UNVERIFIED.**~~ **Answered (M1): it is impossible, and the
  design routes around it.** `HPCON` ownership cannot cross a process boundary with documented
  APIs, so Windows keeps the ConPTYs in a pty-host process that upgrades never replace — see M1.
  The residual risk moves to the *frozen host surface* contract on `VersionPolicy`: break it and a
  new daemon mis-talks to an old host. Changes to host-facing messages must stay additive.
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
| M1 live upgrade ✅ | `mux/m1-takeover` | — |
| M2 attach client | `mux/m2-attach` | — |
| M3 embedded SSH | `mux/m3-ssh` | M2 |
| M4 control mode | `mux/m4-ccmode` | M2 |
| M5 left panel | `mux/m5-panel` | — (adoption list landed with M7) |
| M6 workspace sets | `mux/m6-sets` | — |
| M7 orphan adoption ✅ | `mux/m7-adopt` | M5 |

**Wave 1 (parallel now):** M5 ‖ M6 — no shared files (M1 landed).
**Wave 2:** M2 ‖ M7. **Wave 3:** M3 ‖ M4.
