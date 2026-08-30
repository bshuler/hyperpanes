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
| ~~macOS TCC grants invalidated every rebuild~~ | — | **Retired** by Q1: Developer ID available, bundle is signed |
| ~~Embedded webview contradicts the "no Electron, no browser" identity~~ | — | **Retired** by Q2: no internal browser is built at all |
| PATH shim surprises the user's shell/scripts | **Low** | Q3 — `BROWSER` is the default; the shim is opt-in and labelled |
| `state.rs` / `app.rs` contention across agents | **Medium** | Wave 0 pre-carves; one owner per file per wave |
| Slint layout-cycle assertion in the mode strip | **Medium** | Fixed-height strip; no geometry reads in bindings (commit `5c2eef5`) |
| Per-OS app-crate code not compiled by CI until tag time | **Medium** | D12 — matrix change in Wave 0 |
| Claude's on-disk transcript layout changes upstream | **Low** | Bounded prefix parse already tolerant; provider isolated behind the trait |
| A detected path that does not exist offers a dead click (D14) | **Medium** | Verify against the pane's live cwd before offering reveal; copy stays ungated |
| A repo-local project file drifts from the workspace envelope (D13) | **Medium** | Reuse `PaneSpec` verbatim — one format, one compat rule |
| No filesystem watcher in the tree (`notify` absent) | **Low** | Poll on panel open + mtime cache, matching today's `history_scan` behaviour |

---

## 8. Answered questions — the decisions they settle

All five questions below were put to the human on 2026-08-30 and answered the same
day. They are kept here rather than deleted because each one is load-bearing for a
decision above, and the *reason* a wave is shaped the way it is should not have to be
reconstructed from a commit message.

### Q1 — Code signing on macOS. **Answered: a Developer ID is available.**

macOS keys TCC grants to the code-signing identity, so an unsigned bundle loses every
Screen Recording / Full Disk Access grant on each rebuild. With a Developer ID this
stops being a reason to defer.

**Settles:** F8 (permissions) is fully in scope; `packaging/macos/bundle.sh` gains the
`NS*UsageDescription` keys, an entitlements plist, `codesign` with the Developer ID,
and notarization. The "grants invalidated every rebuild" risk is **retired**.

### Q2 — The internal browser. **Answered: skip it. The choice is the feature.**

**Settles:** no embedded webview, no `wry`/`WKWebView`/`WebView2` dependency, and
`PaneKind::Browser` does **not** get a renderer. What ships is the *routing* choice:
system default, any installed browser, or **Ask** (a chooser at launch time). This
also retires the "embedded webview contradicts the product identity" risk, and
`core::open::list_browsers` — already written and tested on all three OSes in Wave 0 —
is the whole of the enumeration work.

### Q3 — Intercepting a tool's attempt to open a URL. **Answered: `BROWSER` env var.**

**Settles:** the `BROWSER` environment variable, exported into each tool pane's PTY, is
the default and only enabled mechanism. The `PATH` shim survives as a clearly labelled
opt-in for tools that ignore `BROWSER`, never as a default — it is the option that can
surprise the user's own shell and scripts.

**New scope this answer brought with it** — recorded as D14/D15 below.

### Q4 — Codex. **Answered: install what is missing. Plus a correction.**

The human's note that "codex cli is called agent" turned out to be a name collision,
and the probe is worth writing down because the registry would otherwise carry a wrong
entry forever:

```
~/.local/bin/agent -> ~/.local/share/cursor-agent/versions/2026.08.25-3e8eec8/cursor-agent
```

`agent` is the **Cursor** CLI's installed name, not Codex's. Acted on both ways:

- `agent` was added to the `cursor-agent` entry's `alt_bins` — a *binary* name only,
  never a detect token, because `"agent"` in a pane title names no tool at all and is
  already in `GENERIC_AI_TOKENS` for exactly that reason.
- The real Codex CLI was installed (`npm install -g --prefix ~/.local @openai/codex`,
  `codex-cli 0.151.0`), so its history layout can be verified against a real install
  instead of implemented blind. `HistoryKind::None` on the `codex` entry stands only
  until that verification happens.

This also motivated `registry::by_bin`: the executable the user asked to run is direct
evidence and needs no ambiguity rule, so it is consulted *before* the title-token match
that `for_command` used to rely on alone.

### Q5 — Ship in waves, or hold. **Answered: build every phase and wave autonomously.**

**Settles:** no gating on human review between waves. In exchange, the verification
contract tightens: every feature is end-to-end tested *by the implementer* — driving a
real PTY with `expect` or equivalent, the way a human would use it — **before** it is
handed over for manual testing. A feature that only passes unit tests is not done.

---

## 9. Scope added by the answers

### D13 — Repo-local project files: a repo describes its own windows

**Asked for:** opening `~/code/tplx` should find in-repo dot-files saying which windows
to open and what work is going on in each, so a window survives a reboot or a
multi-month pause.

This is the same durable-session problem the workspace persistence layer already
solves, pointed at a different home: today the layout lives in the user's app-support
directory and is keyed by workspace uid, which means it travels with the *machine* and
not with the *repo*. A repo-local file makes the layout a property of the checkout —
it clones, it branches, it can be committed or `.gitignore`d per the owner's taste.

- **Location:** `.hyperpanes/project.json` in the repo root, discovered by walking up
  from the opened directory to the first `.hyperpanes/` or `.git/` — the same ancestor
  walk shape the existing git-branch lookup uses.
- **Format:** the existing `PaneSpec`/workspace envelope, reused verbatim. Same
  `Option<T>` + `skip_serializing_if` compat rule, same `ENVELOPE_VERSION`. Writing a
  second serialization format for the same data is how the two drift.
- **Intent, not just geometry:** each pane carries a human-written `note` — "what work
  is going on in this window" — that survives independently of whether the process is
  still alive. This is the part that makes a multi-month pause recoverable, and it is
  the one genuinely new field.
- **Precedence:** a live session for the workspace wins; the repo file is the fallback
  used when there is no live session, and the seed used on first open. It never
  silently overwrites a running layout.
- **Never a secret store.** The file records commands and notes. Anything that would
  put a token in a git working tree is out — the existing secret rule applies without
  exception.

### D14 — Paths in pane output are clickable and copyable

**Asked for:** detect file names and paths in pane output; make them click-openable
*and* click-copyable. Clicking a filename in a chat pane opens it in a file-tree panel
on the left, from which the user chooses what to do with it.

The terminal widget already has the machinery: it detects URLs and hit-tests them for
click (`terminal-widget/src/pane.rs`), which is why this is an extension rather than a
new subsystem. What changes:

- A path detector beside the URL detector, scoring candidates (absolute paths,
  `./`-relative, `file:line:col` triples as emitted by every compiler and test runner)
  and **verifying existence against the pane's live cwd** before offering a click —
  inferring a path from shape alone is exactly the kind of guess that produces a
  dead link, and the cwd is already tracked by shell integration.
- Two affordances on one target: primary click **reveals in the left file tree**,
  modifier-click (or the context menu) **copies the path**. The copy path must not be
  gated on the file existing; the reveal must be.
- A `file:line` hit carries the line number through to whichever tool opens it.

### D15 — Editor tool panes: vim, emacs, edit

**Asked for:** "maybe we have tool panes for vim and emacs and edit, the most popular
of the editing tools."

These are registry entries like any other tool — `PaneKind::Tool("vim")` is a terminal
pane with an identity, which is D1 doing its job. They differ from the AI tools in one
respect only: they take a *target*. That makes them the natural consumers of D14 —
"open in a terminal with vi" is `NewPaneOpts { command: Some("vim <path>"), kind:
Tool("vim") }` and nothing more.

`HistoryKind::None` for all three: an editor's recent-files list is its own business
and not a resumable session in the sense the left panel means.

Probed on this machine: `vim -> /usr/bin/vim`, `nvim -> /opt/homebrew/bin/nvim`,
`nano -> /usr/bin/nano`. `emacs` is not installed; the registry entry does not require
it to be, which is the point of D4.

---

## 10. Build log

A running record of what has actually landed, so a fresh session can pick the work up
without re-deriving it from the diff. Newest section last.

### Wave 0 — 2026-08-30

**Status: code complete, green, awaiting the two frozen-file edits.**

Landed:

- `core/src/tools/{mod,registry,detect,kind}.rs` — the `TOOLS` table (16 entries incl.
  the D15 editors), `by_bin`/`by_id`/`by_title`, `PaneKind`, binary-first `for_command`,
  and a `resolve`/`resolve_all` `PATH` + well-known-dirs scan with user overrides.
- `core/src/open/` — the three duplicate openers consolidated behind one seam, the macOS
  `xdg-open` bug fixed, a `http`/`https`/`mailto` allow-list plus a command-splitting
  guard, and browser enumeration for all three OSes.
- `core/src/permissions/` — six `Right`s, a four-state `Grant`, per-OS deep links; every
  impl still answers `Undetermined`/`NotApplicable`, which is the Wave-0 contract.
- `PaneKind` threaded end-to-end: `PaneState` / `DetachedPane` / `NewPaneOpts`,
  `meta["pane.kind"]` at both spec-write sites, recorded-outranks-derived on read in BOTH
  `app::make_pane_from_spec` and `core::spawn_seed_pane`, and `kind` on the control
  read-model (`PaneInfo` → `PaneOut`, omitted for a plain terminal — the additive-optional
  rule `talk` set).
- Branded marks: 16 drawn-geometry tool glyphs plus the `ToolIcon` dispatcher in
  `widget.slint` (kinds 40–55), consumed by the pane header, the left panel's mode strip
  and the Preferences → Tools list. `theme::menu_icon::TOOL_BASE` is locked to the
  registry's `TOOL_ICON_BASE` by a cross-crate test.
- Settings: `tool_favorites`, `tool_paths`, `browser_mode`, `browser_app` (+ helpers),
  four new `Setting` variants, and Preferences pages 5 (Tools) and 6 (Browser) — fully
  wired, not shells: star/unstar, per-tool path override, and the three browser modes.
- Left panel: `LeftPanelAdapter.mode` / `modes` / `sessions` / `resume-session`, the mode
  strip, and the `mode != 0` session list. Mode 0 is the existing tree.

Two decisions worth carrying forward:

- **The mode strip gates the four workspace sections INDIVIDUALLY** (11 `mode == 0`
  conditions) rather than wrapping them in one container. Wrapping would add a nesting
  level and change this layout's `vertical-stretch` maths, and this panel has broken
  twice on exactly that (`b0c6637`, `5c2eef5`).
- **`pref-action` code 21 was double-booked** — the General page's keep-alive toggle and
  the Terminal page's copy-on-select both sent 21, and the `if kind == 21` early return
  shadowed the `21 => CopyOnSelect` match arm, so copy-on-select silently flipped
  keep-alive. Keep-alive moved to 22 (the match arm's claim on 21 is the older, declared
  intent) and an authoritative code table now sits above `on_pref_action`. `pref_text`
  has its own separate code space, documented the same way.

**D12 (CI blind spot) closed.** `verify.yml`'s `build-gui` is now a three-OS matrix:
Linux keeps the full `cargo build --locked` (it also proves the link step and is the
cheapest runner), macOS and Windows run `cargo check --locked --bins`, which is what
actually type-checks the per-OS `cfg` blocks without paying for codegen of the whole GPU
stack on the slowest runners. `fail-fast: false`, so one OS breaking does not hide the
other two. `test.yml`'s two comments claiming the app crate is tag-time-only on every OS
were stale the moment this landed and now point here.

**No `Cargo.toml` edits were needed after all**, so the frozen files stay untouched and
Wave 1 inherits them clean. Everything Wave 0 added is std-only or uses deps already
declared: `core` already has `serde`/`serde_json` (the session providers in T5 need
nothing more), and the macOS permission *status* probes that Wave 2+ will want
(`CGPreflightScreenCaptureAccess` and friends) are reachable through a raw
`#[link(name = "CoreGraphics", kind = "framework")] extern "C"` block — `core` does not
need `objc2` for them. If a later wave does need a crate, that is still an orchestrator
checkpoint, not a unilateral edit.

**Wave 0 gate: MET.** `cargo check --manifest-path rs/crates/app/Cargo.toml --bins`
green; `cargo test --bins` 249 passed / 0 failed; `cargo test` on `rs` 883 passed / 0
failed / 5 ignored. Landed as `fc067e3`.

### Wave 1 · A2 (T3 + F5) — 2026-08-30

**Status: landed. A terminal pane that starts running `claude` now wears the Claude mark
and drops it again at the next prompt.**

The whole of T3 hangs on one structural choice: **the sniff never touches `PaneState`.**
Detection lives in two runtime-only side maps on `State` — `sniffed_tool: HashMap<uid,
tool-id>` and `agent_live: HashMap<uid, AgentLiveness>` — which are not fields of a pane,
are not serialized, and cannot round-trip through the session store. The two identities
meet in exactly one place, `State::effective_kind(&PaneState)`, which the UI projection
reads. That makes D5's rule ("a sniff may upgrade the chrome; it must never rewrite
`spawn_command`/`spawn_args` or the persisted `PaneKind`") true *by construction* rather
than by everyone remembering it — there is no code path from a title frame to anything
that survives a restart.

Landed:

- `state.rs` — the side maps plus `note_pane_title`, `note_agent_state`, `note_agent_idle`,
  `forget_pane_runtime`, `effective_kind`, `liveness_ui`. `note_pane_title` is a no-op on a
  pane whose `kind` is already explicit, so a pane *spawned* as a tool can never be
  re-labelled by whatever it prints.
- `app.rs` — `route_event` feeds the OSC title it was already sniffing for the glow to
  `note_pane_title` as well (hoisted out of the pane borrow), and the four
  previously-discarded arms now land: `CommandStart` → `Busy`, `CommandEnd`/`PromptReady` →
  idle (the downgrade signal — the mark is dropped, not kept forever), `AgentState` → the
  reported state.
- `paneview.rs` — both projection sites precompute `Vec<(PaneKind, i32)>` *before* taking
  `&mut state.tabs[..]`, because the side maps are unreachable once that borrow is held.
  Moving the data onto `PaneState` would have solved the borrow and re-opened the
  persistence risk; the vec was the cheaper trade.
- `types.slint` / `theme.slint` / `paneview.slint` — `agent-live: int` on `PaneItem`, the
  `ok`/`warn` tokens, and a 6px liveness dot in the pane header. The dot is gated on
  `kind != 0`, not on `tool-icon`, so a tool with no glyph still reports and a plain shell
  running `make` never grows one.
- The idle glow now accepts *either* signal — the merged identity or the title sniff.
  Gating on the title alone meant a pane spawned as `claude` that never printed an OSC
  title sat there quiet and unglowing.

Map growth is handled at the three exits, not sprinkled: `take_pane_in` was split around
`take_pane_inner` so every removal path drops the entries, and the two removals that do
*not* flow through it — the pane-restart site (which mints a new uid, stranding the old
key) and the parked-reminder `Exit` path — forget explicitly.

`control/readmodel.rs` needed nothing: Wave 0 already put `kind` on `PaneOut` and its
tests already assert both halves (omitted for a shell, `"claude"` for a tool pane).

Worth carrying forward: `registry::by_title` returns `None` when two *different* tools
match, and `vim` is itself a registered tool (D15). So `"vim claude-vs-codex.md"` resolves
to `Tool("vim")` — the tokenizer keeps `-`, making `claude-vs-codex` one token that matches
nothing. The ambiguity test needs two tokens naming two different tools (`"claude · codex"`).

**Gate: MET.** `cargo check --manifest-path rs/crates/app/Cargo.toml --bins` green;
`cargo test --bins` 258 passed / 0 failed (T3 added 8, plus the glow widening). Landed as
`5222ed6` and the glow follow-up.
