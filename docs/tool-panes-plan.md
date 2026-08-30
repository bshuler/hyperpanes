# Tool panes, left-panel modes, browser routing, and permissions — plan

Status: **proposal, awaiting sign-off on Q1–Q5 (§8).**
Written 2026-08-30. Grounded in a three-agent read of the tree at `cee964b`; every
claim below cites the file it came from, so a reviewer can check the premise before
checking the conclusion.

---

## 1. What was asked for

Seven features, one sentence each:

| # | Feature | Short name |
|---|---|---|
| F1 | The left panel gets **modes** — Workspace (today's view) plus one view per tool | `panel-modes` |
| F2 | A **settings page** listing every supported CLI AI tool, with favorites | `tool-prefs` |
| F3 | **Detect** each tool's binary, and let the human override the path | `tool-detect` |
| F4 | Panes **designated for a tool** — a Claude pane, a Cursor pane — plus non-terminal panes (file browser, web browser, file viewer, markdown preview) | `pane-kinds` |
| F5 | A terminal pane **upgrades itself** when it notices the tool is running | `pane-upgrade` |
| F6 | A **Claude view**: every locally resumable conversation, by project, newest first. Same for each favorited tool | `session-views` |
| F7 | **Browser routing** — internal / a specific installed browser / Ask — for URLs a tool opens | `url-routing` |
| F8 | An **on-demand permissions** path (screen recording, full disk access) | `permissions` |

Plus two cross-cutting constraints the user stated explicitly: **branded SVG icons**
so each tool feels like itself, and **every feature on every supported OS**.

---

## 2. The headline finding: most of the hard parts already exist

The single most important result of the survey is how much of this is already
built and simply not surfaced. Planning as if this were greenfield would have
produced roughly three times the work.

**Detection already runs on every PTY byte.**
`rs/crates/app/src/glow.rs:360` is `sniff_osc_title()`, a byte scanner for
`ESC ] (0|2) ;` titles. `glow.rs:349` is `is_ai_pane()`, token-matching against
`AI_NAMES` (`glow.rs:~330`) — thirteen entries that already include `claude`,
`codex`, `cursor-agent`, `copilot`, `aider`, `gemini`, `goose`. It is fed at
`app.rs:1078` into `PaneState.shell_title`. Today it only drives a glow
animation. **F5's signal is live; only the consequence is missing.**

**A deterministic Claude-identity marker already exists.**
`rs/crates/core/src/claude_panes.rs` — `resources/claude/hp-claude-session-hook.sh`
registers as a Claude Code `SessionStart` hook, reads `HYPERPANES_PANE_ID` from the
environment, and writes `<state dir>/claude-sessions/<pane-id>.json`; `SessionEnd`
deletes it. That is an *authoritative* "Claude is running in pane X", not a guess —
exactly the coded-deterministic-over-inferred preference we are asked to honour.

**The Claude session reader is done.**
`rs/crates/core/src/claude_history.rs` (≈470 lines) already has `ClaudeSession`
(`:62`), bounded-prefix parsing (`SUMMARY_SCAN_LINES = 60`, `FULL_TEXT_MAX = 32 KiB`),
substring search across the transcript body (`session_matches` `:93`), and
`SessionCache` (`:396`) which re-parses only transcripts whose mtime/size changed.
`rs/crates/app/src/history_scan.rs` runs it on a background thread with per-job
dedupe and drains into the UI each tick. `rs/crates/app/src/sidebar.rs:497` already
produces `claude --resume <id>`, and `state.rs:4959-5022` already knows the two
resume shapes (append `--resume` to the spawn argv vs. type `cd '<cwd>' && claude
--resume <id>` into a live pane).

Critically, `claude_history.rs:46` already declares
`pub enum HistorySource { Claude }` with a comment saying the enum exists *so other
harnesses can plug in later*. **F6's architecture was anticipated by whoever wrote
that file.** Our job is to widen it, not invent it.

**A full preferences UI exists.** `rs/crates/app/ui/overlays.slint:594` is overlay
`kind == 2`: an 820×600 card with a 168px category rail (`:611`) and five pages
(Appearance `:652`, Terminal `:784`, AI features `:862`, Keybindings `:1035`,
General `:1228`), with `pref-action(int,int)` / `pref-text(int,string)` callbacks
already plumbed to `Setting` variants. **F2 and F7's settings are new *pages*, not a
new settings system.**

**Persistence needs no format change.** `PaneSpec.meta: Option<BTreeMap<String,String>>`
(`rs/crates/core/src/workspace/model.rs`) already carries `claude.session`,
`claude.cwd`, `ai.subtitle`, `role`. A `"pane.kind"` key round-trips today on every
existing build, forward and backward, with zero risk.

**What genuinely does not exist:** enumerating *installed browsers*; launching a
*specific* browser; any interception of a tool's URL opens; any OS-permission code;
any native child view over the Slint window; any filesystem watcher (`notify` is
absent from all three lockfiles — everything is poll-based); and any non-terminal
pane content.

---

## 3. Architecture decisions

### D1 — A tool pane **is** a terminal pane. This is the load-bearing decision.

Claude, Cursor Agent, Codex, and Copilot CLI are all CLI programs running under a
PTY. A "Claude pane" is a terminal pane wearing Claude's identity: its icon, its
brand accent, its header badge, its context-menu verbs, its left-panel view. It is
**not** a different renderer.

That collapses F4 into two unrelated problems of very different size:

- **Family A — PTY-backed** (terminal + every tool pane). Zero rendering work.
  Everything is chrome and behaviour. This is F1–F3, F5, F6 — i.e. most of what
  was asked for.
- **Family B — non-PTY views** (file browser, file viewer, markdown preview).
  Real new content rendering, but modest and self-contained.
- **Family C — web browser pane.** A different animal entirely; see Q2.

Sequencing follows: Family A first, and it is independently shippable.

### D2 — `PaneKind` on `PaneState`, discriminator in `meta`

```rust
pub enum PaneKind {
    Terminal,              // default
    Tool(ToolId),          // PTY-backed, identified
    FileBrowser, FileViewer, Markdown,   // Family B
    Browser,                              // Family C, gated
}
```

`PaneState` (`state.rs:491`) gains `kind: PaneKind`. `PaneItem`
(`ui/types.slint`) gains `kind: int` plus `tool-icon: int` and `tool-name: string`.
Both are one-line additions; `pane_item()` (`paneview.rs:227`) is the single
projection site.

Persisted as `meta["pane.kind"] = "claude"` — no `PaneSpec` field, no
`ENVELOPE_VERSION` bump. The compat contract is already written down for us:
`rs/crates/core/tests/workspace_uid_compat.rs` locks backward-load, per-pane
resolution, **forward**-load on a build that predates the field, and round-trip.
Wave 0 clones it as `workspace_kind_compat.rs` with the same four directions.
Rule inherited from that suite: `Option<T>` + `skip_serializing_if`, and never
add `deny_unknown_fields` (the forward test exists precisely to break CI if
someone does).

**Known cost:** `PaneState` has four construction sites — `state.rs:1301`
(`make_pane`, canonical), `:1513` (`adopt_pane_at`), `:4033`, `:5074` (reminder
panes). All four must set `kind`. A `#[derive(Default)]`-backed field keeps that to
four one-word edits. There is also a third pane representation in
`core/src/control/readmodel.rs` (`PaneInfo`) that the external control surface
publishes; kind should reach it too.

### D3 — Do **not** mint session uids for non-PTY panes

`PaneState.uid` is doing four jobs at once: the `SessionManager` registry key, the
`HYPERPANES_PANE_ID` environment value, the Claude hook's marker filename, and the
`PaneSpec.uid` reattach key (`session_manager.rs:816`). A file-browser pane has no
PTY, so calling `SessionManager::fresh_uid()` for one would put a phantom entry in
front of `pane_load` (`session_manager.rs:873`), `has()`, and the multi-window
`claim_session`/`release_session` arbitration.

Decision: `uid` stays *pane* identity. Non-PTY panes mint `view-<uuid>` locally, and
every `mgr.*` call is gated on `kind.is_pty()`. `attach_panes_from_specs`
(`state.rs:4843`) skips `pane_load` for non-PTY specs. This is the smallest change
that keeps the invariant true; the alternative (splitting `uid` into
`pane_uid` + `session_uid`) touches far more and buys nothing yet.

### D4 — The tool registry is **data**, in core, and becomes the single source of truth

`rs/crates/core/src/tools/registry.rs`:

```rust
pub struct ToolDef {
    pub id:        &'static str,          // "claude"
    pub name:      &'static str,          // "Claude Code"
    pub bin:       &'static str,          // "claude"
    pub alt_bins:  &'static [&'static str],
    pub icon:      u32,                   // icon kind int, see D8
    pub brand:     (u8, u8, u8),          // accent for chrome
    pub detect:    &'static [&'static str],  // OSC-title tokens
    pub history:   HistoryKind,           // None | ClaudeJsonl | CursorSqlite | CopilotSqlite
}
pub static TOOLS: &[ToolDef] = &[ /* claude, cursor-agent, codex, copilot, aider, gemini, … */ ];
```

`glow::AI_NAMES` is then **derived from this table** rather than duplicated — a real
de-duplication with a trivial test (`every AI_NAMES token appears in some ToolDef`).
Adding a tool becomes a table row plus, optionally, a provider impl. No new Slint,
no new commands.

### D5 — Detection precedence: explicit → marker → sniff. Never the reverse.

1. **Explicit.** The pane was spawned as a tool (`Command::NewToolPane(ToolId)` or a
   workspace file carrying `meta["pane.kind"]`). Authoritative, no inference.
2. **Marker.** `<state dir>/claude-sessions/<pane-id>.json` exists for this pane
   (`claude_panes.rs`). Authoritative. Generalize the hook mechanism per tool where
   the tool supports hooks; Claude already does.
3. **Sniff.** `glow::sniff_osc_title` + `ToolDef::detect` tokens. Inferred, lowest
   priority. It may *upgrade* a pane's chrome; it must **never** rewrite
   `spawn_command`/`spawn_args`, because a wrong guess would then corrupt what gets
   saved and relaunched.

A downgrade path matters as much as the upgrade: when a tool exits, the pane returns
to `Terminal` unless it was explicit. The signal for that is already flowing and
already being thrown away — `app.rs:1110-1113` discards
`SessionEvent::CommandStart` / `CommandEnd` / `PromptReady` / `AgentState`, and
`AgentLiveness` (`session_manager.rs:116`) is exactly `Busy | AwaitingInput | Done |
Error`. Consuming those four arms is small, independently useful (a live "agent is
waiting for you" badge), and lands before any polymorphism.

*Optional hardening, Wave 3:* `SessionSnapshot.pgrp` (`session_manager.rs:146`) is
already carried daemon-side for the fd handoff. Asking the daemon for the pane's
foreground process name is the only fully deterministic answer to "what is running
*right now*" as opposed to "the title once said". No `/proc` walking or
`proc_pidinfo` exists in the tree today, so this is genuinely new per-OS code —
hence Wave 3, not Wave 1.

### D6 — Binary detection is a pure-Rust `PATH` scan, not a subprocess

Two probes exist today and neither is right for this. `core/src/paths.rs:356`
(`detect_vscode`) shells out to `which`/`where` — slow, and on Windows it needs
`CREATE_NO_WINDOW` to avoid flashing a console. `core/src/speech/engine.rs:44`
(`on_path`) is a clean in-process `$PATH` walk but is `#[cfg(unix)]` and lives in
the speech module.

Decision: promote `on_path` into `core/src/tools/detect.rs`, make it cross-platform
(honour `PATHEXT` and the `.exe`/`.cmd`/`.bat` shapes on Windows), and resolve each
tool in a fixed order with **no silent fallback**: user override → `PATH` →
well-known install locations (`~/.local/bin`, `/opt/homebrew/bin`, `%LOCALAPPDATA%\Programs`).
The settings page shows which of the three answered, so a surprising result is
explainable rather than mysterious.

*Ground truth on this machine, for the fixtures:* `claude` (`~/.local/bin`),
`cursor-agent`, `cursor`, `copilot`, `aider`, `gh` (`/opt/homebrew/bin`) are
present; `codex`, `gemini`, `opencode`, `amp`, `goose`, `q`, `crush` are not.

### D7 — Settings and the left panel extend existing mechanisms

**Settings** (`prefs/mod.rs:133`, 17 fields today) gains four, all
`#[serde(default)]` so every existing `native-settings.json` loads unchanged:

```rust
tool_favorites: Vec<String>,             // ordered ToolIds
tool_paths: BTreeMap<String, String>,    // id -> user-chosen absolute path
browser_mode: String,                    // "system" | "ask" | "app:<id>" | "internal"
browser_app: String,                     // resolved id/path for "app:"
```

Two new pages on the existing rail — `sel == 5` **Tools**, `sel == 6` **Browser** —
following the `sel == 2` AI-features idiom exactly (`PrefToggle` → `pref-action(id, v)`,
`TextInput` → `pref-text(id, s)`). No new overlay kind for settings.

**Left-panel modes**: `LeftPanelAdapter` gains `in property <int> mode`, and
`leftpanel.slint` grows a mode strip at the top (drawn-geometry icons — see D8).
Mode 0 wraps today's four sections verbatim. Modes 1..N are generated from the
favorites list and all render the **same** generic `SessionListView` over a generic
row model. Adding a tool's view is therefore registry data plus a provider, never
new Slint.

> **Hazard, learned the hard way (commit `5c2eef5`):** reading `absolute-position`
> or `height` inside a *reactive binding* in this panel trips Slint's
> `solve_box_layout` assertion during repeater init and silently empties the tree.
> All geometry reads live in event handlers. The mode strip must be a fixed-height
> row, not a computed one.

### D8 — Icons: original marks, drawn geometry, brand colour

Two hard constraints collide here.

*Repo rule:* **drawn geometry only, never font glyphs** — Slint on Windows silently
drops codepoints the resolved font lacks (documented at `leftpanel.slint:30-32`,
`types.slint:263-265`, `theme.rs:230-235`, root cause `df73005`). So every icon is a
`Path` with a `commands:` string, following the `SaveGlyph` idiom
(`leftpanel.slint:68`).

*Legal:* the user asked for icons that make the app "feel branded for the tools you
use". Redistributing Anthropic's, OpenAI's, GitHub's, or Cursor's actual logos in a
shipped binary is a trademark question, not a technical one. Decision: ship
**original geometric marks that carry each tool's brand colour and silhouette
family**, in a single swappable module (`ui/toolicons.slint`) with the reasoning in
its header, so replacing them later is one file. Icon kinds are ints allocated in
`theme.rs` from `TOOL_ICON_BASE = 40`, mirroring the existing `menu_icon` /
`MenuIcon` lock-step convention (`theme.rs:236-252`, `contextmenu.slint:158`).

*Note:* `resvg`/`usvg` are already linked transitively via Slint's image pipeline
(`app/Cargo.lock:5428`), so `Image::load_from_path` on a `.svg` works for free if we
ever want raster fallbacks. Not needed for chrome icons.

### D9 — `SessionProvider`, generalizing what `claude_history.rs` already does

`core/src/tools/history/mod.rs`:

```rust
pub struct ToolSession {         // ClaudeSession + provenance
    pub id: String, pub source: HistorySource,
    pub project: PathBuf, pub branch: Option<String>,
    pub started_at: Option<u64>, pub summary: String,
    pub first_user: String, pub message_count: usize, pub full_text: String,
}
pub trait SessionProvider {
    fn id(&self) -> &'static str;
    fn scan(&mut self) -> Vec<ToolSession>;     // ALL projects, cache-backed
    fn resume(&self, s: &ToolSession) -> ResumePlan;
}
```

`HistorySource` (`claude_history.rs:46`) is extended rather than duplicated — the
file's own comment invites this.

**The one real new problem in F6.** Today's reader is keyed by a *known* project
root: `encode_project_dir()` (`:119`) forward-encodes a path to its directory name.
The user asked for *every* locally resumable conversation, which means enumerating
directories we have no root for — and the encoding is **lossy and not reversible**.
`claude_history.rs:1-20` documents the rule (keep `[A-Za-z0-9]`, map everything else
to `-`, runs not collapsed) and I confirmed it empirically: `-Users-bshuler--pane`
is really `/Users/bshuler/.pane`; `-Users-bshuler-code-NoAds--claude-worktrees-daily-games`
is `/Users/bshuler/code/NoAds/.claude/worktrees/daily-games`. Both `/` and `.` become `-`.

The true path is recoverable, but only from *inside* the transcript: a verified
sample carried `cwd = /Users/bshuler/.pane` at line 6, alongside `gitBranch`,
`sessionId`, `timestamp`, and `version`. Line 1 is a summary record with no `cwd`.
So: add `cwd` and `git_branch` to the existing bounded prefix scan in
`read_session_file` (`:203`) — the fields land inside the first ~10 records, well
within the existing `SUMMARY_SCAN_LINES = 60` budget, so this costs nothing extra.
Cache by `(path, mtime, size)`, which `SessionCache` already does, making the whole
global scan O(new files) after the first index. There are 39 project directories on
this machine — a realistic fixture.

Provider status:
- **Claude** — wraps `SessionCache`. Ships Wave 1.
- **Cursor** — `~/.cursor/chats/<workspace-hash>/<uuid>/store.db` (SQLite) plus a
  plain `prompt_history.json`. `rusqlite 0.32` is **already a bundled core
  dependency** (`core/Cargo.toml:33`), so no new crate. Wave 2.
- **Copilot** — `~/.copilot/session-state/<uuid>/` plus a global
  `~/.copilot/session-store.db`. Wave 2.
- **Codex** — layout unverifiable; see Q4.

### D10 — URL routing, in three separable layers

**Layer 1 — consolidate three openers into one seam.** There are three
implementations of "open this thing" today: `core/src/paths.rs:399` (`os_open`, the
canonical one, whose own doc says it is public so the app need not grow its own),
`terminal-widget/src/pane.rs:136` (a second one using `raw_arg` on Windows so
query-string `&`/`?` cannot split the command — the comment acknowledges the
duplication), and `app/src/command.rs:425` (`RevealPaneCwd`). **The third has a real
bug**: it branches `#[cfg(windows)]` → `explorer` else `xdg-open`, and `xdg-open`
does not exist on macOS, so "Open Folder" in the pane context menu
(`contextmenu.rs:308`) is silently broken on macOS today. Fold all three into
`core/src/open/` with per-OS files per the `docs/ports-seams.md` convention, fixing
that bug on the way.

**Layer 2 — enumerate and launch a *specific* browser.** Genuinely new, and per-OS:
macOS `LSCopyApplicationURLsForURL` (this Mac has Chrome and Safari); Windows
`HKLM\SOFTWARE\Clients\StartMenuInternet`; Linux `.desktop` scan for
`x-scheme-handler/http` plus `xdg-settings get default-web-browser`. Feeds the
Browser settings page and the "Ask" chooser (a new overlay `kind == 6`, following the
`kind == 3/4/5` small-card idiom). Note `webbrowser 1.2.1` is already compiled in
transitively (`app/Cargo.lock:7555`) if we only ever wanted "the default browser" —
we want more than that, so we write layer 2 ourselves.

**Layer 3 — intercepting what a tool opens.** The hard part, and the one needing
sign-off: see **Q3**.

### D11 — Permissions broker, requested at point of use

`core/src/permissions/` as a new seam (`mod.rs` + `macos.rs` / `windows.rs` /
`linux.rs`), surface:

```rust
pub enum Permission { ScreenRecording, FullDisk, Accessibility, Microphone, Notifications }
pub enum Status { Granted, Denied, Undetermined, NotApplicable }
pub fn status(p: Permission) -> Status;
pub fn request(p: Permission);        // may be a no-op where the OS has no request API
pub fn open_settings(p: Permission);  // deep-link to the right settings pane
```

Never called at launch. The flow is: feature needs it → in-app explainer card saying
*what* and *why* → then the OS prompt. macOS screen recording has a real API pair
(`CGPreflightScreenCaptureAccess` / `CGRequestScreenCaptureAccess`); **Full Disk
Access has no request API at all** — the only honest implementation is a deep link
to `x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles` plus
instructions. Linux mostly reports `NotApplicable`, with the portal path for screen
capture. Windows has no analogue for either and reports `NotApplicable`.

**This is blocked on code signing.** `rs/packaging/macos/bundle.sh` writes
`Info.plist` inline (`:103-151`) with **zero `NS*UsageDescription` keys**, there is
no entitlements file anywhere in the tree, and the bundle is unsigned and
un-notarized — `bundle.sh:13-14` and `rs/packaging/macos/README.md:34-53` document
the `xattr -dr com.apple.quarantine` workaround users must run. macOS keys TCC
grants to the code-signing identity, so on an ad-hoc-signed bundle every grant the
user gives is invalidated on the next rebuild. `update/macos.rs:10-13` already flags
the same blocker for self-update. See **Q1**.

### D12 — Close the CI blind spot *before* writing per-OS code

`verify.yml:43` runs `cargo test` on ubuntu × windows × macos, but only for the `rs`
and `terminal-widget` workspaces. `verify.yml:133` compiles the **GUI app crate on
Linux only**, and `test.yml:49-51` says so on purpose: *the GUI app crate stays off
the per-push path on every OS.* Windows/macOS GUI code is only *built* — never
tested — at tag time (`release-rust.yml:115/142/187`).

This feature set adds macOS-only and Windows-only code to the app crate. Without a
matrix change we would not learn it is broken until someone cuts a tag. Wave 0 adds
`rs/crates/app` to a `cargo check` matrix on all three OSes. `docs/ports-seams.md:156`
already names this as the standing local bar; we are just making CI enforce it.

---

## 4. Track decomposition

| Track | Feature | Family | Depends on |
|---|---|---|---|
| **T1** Registry + detection | F3 | core | — |
| **T2** Tool prefs page | F2 | app/ui | T1 |
| **T3** Pane kind + chrome + upgrade | F4a, F5 | app | T1, W0 |
| **T4** Left-panel modes | F1 | app/ui | W0 |
| **T5** SessionProvider + Claude | F6 | core+app | T1, T4 |
| **T6** Icons | cross | ui | — |
| **T7** More providers (Cursor, Copilot) | F6 | core | T5 |
| **T8** Browser enumeration + routing settings | F7a | core+app | W0 (open seam) |
| **T9** Non-PTY view panes | F4b | app/ui | T3 |
| **T10** URL interception | F7b | core | Q3 |
| **T11** Permissions broker | F8 | core+app | Q1 |
| **T12** Internal browser | F7c | app | Q2 |

---

## 5. Wave plan for parallel agents

The governing rule is **contracts before fan-out**: one agent lands every shared
seam as a *compiling stub* while nobody else is running, and only then do the
parallel agents start, each owning a disjoint file set. The repo already works this
way — `docs/ports-seams.md:143-149` is an explicit Wave-1 file-ownership map marking
every `mod.rs`, every per-OS file, and **both `Cargo.toml`s** as *frozen, touch only
via orchestrator*. We inherit that convention verbatim.

### Wave 0 — contracts (ONE agent, serial, nothing else runs)

Everything here compiles and is inert. No behaviour changes.

- `PaneKind` enum; `PaneState.kind` (all four construction sites); `PaneItem.kind` /
  `tool-icon` / `tool-name`; `pane_item()` passes them through
- `meta["pane.kind"]` write/read + `core/tests/workspace_kind_compat.rs` cloned from
  `workspace_uid_compat.rs` in all four directions
- `core/src/tools/{mod,registry,detect}.rs` — the `TOOLS` table populated, `detect`
  returning `None`
- `Settings` + four fields, `Setting` variants, prefs pages 5 and 6 present but empty
- `LeftPanelAdapter.mode` + mode strip shell; only mode 0 wired
- `core/src/open/` seam — three openers consolidated, macOS `xdg-open` bug fixed
- `core/src/permissions/` seam — every impl returns `Undetermined` / `NotApplicable`
- `theme.rs` icon-kind constants from `TOOL_ICON_BASE = 40`
- **Cargo.toml edits (frozen files — orchestrator only)** and the CI matrix change (D12)

Gate: `cargo check --manifest-path rs/crates/app/Cargo.toml` green on all three OSes;
existing 248 unit tests still pass (`cargo test --bins`, **not** `--lib` — the
`hyperpanes` package is binary-only and `--lib` errors out while still exiting 0).

### Wave 1 — five agents in parallel

| Agent | Track | Owns (exclusive) |
|---|---|---|
| **A1** | T1+T2 | `core/src/tools/detect.rs`, `core/src/tools/registry.rs`, prefs page 5 region of `overlays.slint`, `prefs/mod.rs`, `prefs/platform_*.rs` |
| **A2** | T3+F5 | `app/src/glow.rs`, the pane-kind regions of `state.rs` / `app.rs`, `paneview.rs:227` projection, `control/readmodel.rs` |
| **A3** | T4 | `ui/leftpanel.slint` (exclusive), `app/src/leftpanel.rs` |
| **A4** | T5 | `core/src/claude_history.rs`, `core/src/tools/history/`, `app/src/history_scan.rs`, `app/src/sidebar.rs` |
| **A5** | T6 | `ui/toolicons.slint` (new), `ui/contextmenu.slint` icon dispatch, `theme.rs` icon block |

Contention is real on `state.rs`, `app.rs`, and `leftpanel.slint`; the split above
gives each exactly one owner per wave, which is why Wave 0 pre-carves the insertion
points. A3 and A5 have a seam (the mode strip renders A5's icons) — A5 lands the
icon kinds in Wave 0's `theme.rs` allocation so A3 codes against constants, not
against A5's schedule.

**Wave 1 alone is a shippable product increment**: tools detected and favourited,
panes that know what they are and say so, and a Claude view listing every resumable
conversation by project. That is the highest value-per-risk slice of the whole ask.

### Wave 2 — four agents in parallel

| Agent | Track | Owns |
|---|---|---|
| **B1** | T7 | Cursor provider (`core/src/tools/history/cursor.rs`) |
| **B2** | T7 | Copilot provider (`core/src/tools/history/copilot.rs`) |
| **B3** | T8 | `core/src/open/` per-OS browser enumeration, prefs page 6, Ask overlay `kind == 6` |
| **B4** | T9 | Family B panes — `ui/viewpanes.slint`, the view-model adapter |

### Wave 3 — gated, mostly serial

- **C1** T10 URL interception — blocked on **Q3**
- **C2** T11 permissions real impls — blocked on **Q1**
- **C3** T12 internal browser spike — blocked on **Q2**
- **C4** pgrp-based foreground-process detection (D5 hardening)

### Verification contract, every wave

1. `cargo test --manifest-path rs/crates/app/Cargo.toml --bins` (248 today, must not
   regress) and `cargo test --manifest-path rs/Cargo.toml`
2. `cargo check` for the app crate on macOS, Windows, Linux (new per D12)
3. Compat suite green: the cloned `workspace_kind_compat.rs` four directions
4. Empirical UI check on the isolated sandbox bundle (`/tmp/hphr/HP.app`, bundle id
   `com.hyperpanes.hotreload`, run under `HOME=/tmp/hphr`) — never the user's real
   install. Human-paced synthetic drags via the `slowdrag` CoreGraphics harness; the
   drag pump samples the cursor every 8 ms and refuses anything faster.

---

## 6. Assumptions I am proceeding under

- **Privacy.** Reading the user's own conversation transcripts stays local-only:
  listings, summaries, and the bounded search index never leave the machine and are
  never sent to any model or service. This is continuity — `sidebar.rs` +
  `claude_history.rs` already read these files — not a new posture.
- **No new top-level dependencies** without orchestrator sign-off; `core/Cargo.toml:9-11`
  states this as a rule. Everything planned above fits inside what is already linked
  (`rusqlite` bundled, `serde`, `tokio`, `rfd`, `objc2-app-kit`, `windows 0.62`).
  Any Windows-side addition must not drag in a `windows` crate major other than 0.62
  (`app/Cargo.toml:73`), and `accessibility` stays off (accesskit pins `windows 0.58`,
  which collides with wgpu-hal 29).
- **Codex, Gemini, Goose, and friends get registry entries** (detection, branded
  pane, favouritability) even where no history provider exists. A tool you can
  favourite and see identified is most of the value; the session view is the bonus.

---

## 7. Risk register

| Risk | Severity | Mitigation |
|---|---|---|
| macOS TCC grants invalidated every rebuild (unsigned bundle) | **High** | Q1 — signing decision, or defer F8 |
| Embedded webview contradicts the stated "no Electron, no browser" identity | **High** | Q2 — recommend external routing first |
| PATH shim surprises the user's shell/scripts | **Medium** | Q3 — recommend `BROWSER` env var instead |
| `state.rs` / `app.rs` contention across agents | **Medium** | Wave 0 pre-carves; one owner per file per wave |
| Slint layout-cycle assertion in the mode strip | **Medium** | Fixed-height strip; no geometry reads in bindings (commit `5c2eef5`) |
| Per-OS app-crate code not compiled by CI until tag time | **Medium** | D12 — matrix change in Wave 0 |
| Claude's on-disk transcript layout changes upstream | **Low** | Bounded prefix parse already tolerant; provider isolated behind the trait |
| No filesystem watcher in the tree (`notify` absent) | **Low** | Poll on panel open + mtime cache, matching today's `history_scan` behaviour |

---

## 8. Open questions — options and recommendations

### Q1. Code signing on macOS — do we get a Developer ID?

**Why it matters.** macOS keys TCC permission grants to the code-signing identity.
The bundle is unsigned (`bundle.sh:13-14`), so every grant a user gives is
invalidated on the next rebuild or update. F8 (screen recording, full disk access)
is not meaningfully implementable without this, and `update/macos.rs:10-13` already
flags the same blocker for self-update.

- **(a) Get an Apple Developer ID** ($99/yr), add codesign + notarization +
  `NS*UsageDescription` keys + an entitlements plist to `bundle.sh`. Unblocks F8
  *and* self-update *and* removes the `xattr -dr com.apple.quarantine` step users
  currently have to run.
- **(b) Ship F8 unsigned.** Permissions work until the next update, then silently
  stop. Actively bad UX — a permission that revokes itself reads as a bug.
- **(c) Defer F8 entirely** until signing happens; build the broker seam now
  (Wave 0 does anyway) and leave the impls returning `Undetermined`.

**Recommendation: (a) if F8 is wanted at all, otherwise (c).** (a) pays for three
separate blockers at once, which makes it the best value in this plan. (b) is the
one option I would argue against.

### Q2. The internal browser — how far do we go?

**Why it matters.** No native child view exists anywhere over the Slint window
(`grep` for `addSubview|SetParent|WS_CHILD|define_class` across `crates/` returns
nothing). Slint 1.17 has no embeddable web view. And the README takes visible pride
in *a single self-contained binary (no Electron, no browser)* (`README.md:19-20`) —
the same reasoning used to justify bundling SQLite (`core/Cargo.toml:35-37`). This
is a cultural constraint, not a documented non-goal, but it is real and it is yours
to weigh.

- **(a) True in-pane embed** — WKWebView / WebView2 / WebKitGTK as a child view
  clipped to the pane rect, z-ordered, resized with layout, hit-test-coordinated
  with Slint. Highest fidelity, by far the most work, and **Wayland is a hard case**:
  `drag/linux.rs:11-19` documents that a Wayland client cannot even position a window
  at the cursor, and `supports_cross_window()` returns false there.
- **(b) A separate lightweight window Hyperpanes owns.** Much more tractable, and
  there is precedent in-tree: `drag/macos.rs:83-110` already creates and drives a
  bare `NSWindow` entirely outside Slint's render path, and `drag/windows.rs:19`
  registers its own Win32 class with its own wndproc. Not in-pane, but ours.
- **(c) No internal browser.** Ship installed-browser routing + Ask only. Drops one
  of four `browser_mode` values and nothing else.

**Recommendation: (c) for v1, (b) as the v2 shape, (a) only if in-pane embedding
proves genuinely essential.** F7's actual user value — *"I control where Claude's
links open"* — is fully delivered by (c). The internal browser is the most expensive
item in the entire plan and the least load-bearing.

### Q3. How do we intercept a tool's attempt to open a URL?

**Why it matters.** Claude and its peers shell out to `open` / `xdg-open` / `start`.
Nothing in Hyperpanes sees that today, so "which browser should Claude's links open
in" has no hook without one.

- **(a) PATH shim.** Prepend a Hyperpanes-owned directory to each pane's `PATH`
  containing our own `open` / `xdg-open` / `start`, which post the URL to the
  daemon's control socket. Works for **every** tool without their cooperation, fully
  deterministic. But it shadows a system command inside the user's interactive
  shell — a script that expects real `open` semantics gets ours instead.
- **(b) `BROWSER` environment variable.** Set `BROWSER=<hyperpanes-url-helper>` in
  the pane environment. It is the long-standing convention, respected by a wide range
  of CLI tools, and it changes *nothing* about how normal commands behave. Weaker
  coverage: a tool that calls `open(1)` directly and ignores `BROWSER` slips through.
- **(c) Do nothing.** Let the OS default browser decide; Hyperpanes' setting only
  affects URLs the user clicks *inside* a pane (which `terminal-widget/pane.rs:118`
  already routes).

**Recommendation: (b) as the default, with (a) available as an explicit opt-in
"aggressive routing" toggle in the Browser settings page.** (b) is the least
invasive thing that works for most tools; making (a) opt-in and clearly labelled
means the one user who needs total coverage can have it, with informed consent, and
nobody else gets a shadowed `open` they did not ask for. (c) leaves the feature
unbuilt.

### Q4. Codex — implement blind, or wait?

Codex is **not installed on this machine**, so its session-store layout cannot be
verified. Claude's layout was only pinned down by reading real files; guessing
Codex's from documentation is exactly the "inferred solution" we are told to avoid.

- **(a) Implement from upstream docs**, untested against a real install.
- **(b) Ship the registry entry** — detection, branded pane, favouritable, resume by
  command line — but **no history provider** until a real install can be inspected.
- **(c) Omit Codex entirely.**

**Recommendation: (b).** It gives Codex users everything except the session list,
costs one table row, and does not put unverifiable path-parsing code into core. If
you install Codex, the provider becomes a half-day of work in Wave 2.

### Q5. Ship in waves, or hold until complete?

- **(a) Hold** until F1–F8 are all done. One big landing, longest time to any value.
- **(b) Ship per wave.** Wave 1 alone gives tool detection, favourites, tool-aware
  panes that identify themselves, and the Claude conversation view. Wave 2 adds more
  providers, browser routing, and the non-PTY view panes. Wave 3 is the gated,
  sign-off-dependent work.

**Recommendation: (b).** Wave 1 is independently coherent and independently useful,
and it is exactly the part with no open questions attached — it can start the moment
Wave 0's contracts land, regardless of how Q1–Q4 resolve.
