//! Command dispatch — Wave-2 **Seam #2**.
//!
//! Every user action — a top-bar click, a key shortcut, and (in Wave 2) a command
//! palette entry or a keybinding — is expressed as a [`Command`] and run through
//! [`dispatch`]. `dispatch` mutates the central [`State`] and returns an
//! [`Effect`] for the thin set of concerns that live outside the state (quitting,
//! OS fullscreen). Wave-2 features add variants here and emit them; they never
//! reach into the UI or the window glue themselves.

use hyperpanes_core::layout::navigate::Direction;
use hyperpanes_core::layout::presets::{DividerKind, Layout};
use hyperpanes_core::session_manager::SessionManager;

use crate::state::{DetachedPane, DetachedTab, NewPaneOpts, ReminderOffset, Setting, State};
use crate::theme;

/// The `--model` ids for the goals-system model pickers, indexed by the New-goal dialog's model
/// options. ORDER MUST MATCH [`GOAL_MODEL_LABELS`]. Defaults per tier: orchestrator/spec =
/// index 0 (opus), implementation = 1 (sonnet).
pub const GOAL_MODELS: [&str; 4] = [
    "claude-opus-5[1m]",
    "claude-sonnet-5[1m]",
    "claude-fable-5[1m]",
    "claude-haiku-4-5",
];

/// Display labels for [`GOAL_MODELS`] (the New-goal dialog's chips + option rows).
pub const GOAL_MODEL_LABELS: [&str; 4] = ["opus[1m]", "sonnet[1m]", "fable[1m]", "haiku"];

/// An action against the workspace. Construct these from any input source.
#[derive(Debug, Clone)]
pub enum Command {
    // panes
    /// Immediately spawn a default pane (the plain ＋ click / palette "New pane").
    NewPane,
    /// Open the "New pane" options dialog (Shift+＋ / the menus' "New pane…").
    OpenNewPane,
    /// Submit the New Pane dialog: spawn a pane from the configured options + close the dialog.
    SubmitNewPane(NewPaneOpts),
    /// Open the "New goal" box (command palette → "New goal…").
    OpenNewGoal,
    /// New-goal box: set the goal text — the mirror of the box's TextInput (pushed on `edited`).
    GoalQuery(String),
    /// New-goal box: Tab / Shift+Tab — reveal the option chips, then cycle focus among them.
    GoalNav(i32),
    /// New-goal box: reveal / hide the option chips (Ctrl+O).
    GoalToggleOptions,
    /// New-goal box: hide the option chips + any open list, back to the text field (Esc).
    GoalCollapse,
    /// New-goal box: open (`true`, ↓) / close (`false`) the focused field's option list
    /// (goal field = history; project field = projects; model fields = tiers).
    GoalMenu(bool),
    /// New-goal box: move the open option list's selection by ±1 (chip fields apply live).
    GoalMenuNav(i32),
    /// New-goal box: apply the option list's selected row to the focused field.
    GoalMenuPick,
    /// New-goal box (mouse): focus field `i` and open its option list.
    GoalFieldClick(usize),
    /// New-goal box (mouse): apply option row `i`.
    GoalMenuClick(usize),
    /// Submit the New-goal box (Enter / the submit icon). Routes to [`State::goal_submit`].
    GoalSubmit,
    /// New-goal box: paste the clipboard into the goal — an image becomes an attachment,
    /// text is appended to the goal text (Ctrl+V).
    GoalPasteClipboard,
    /// New-goal box: attach image(s) via the OS file picker.
    GoalAttachImage,
    /// New-goal box: capture the clipboard image (if any) as an attachment.
    GoalPasteImage,
    /// New-goal box: remove attached image `i`.
    GoalRemoveImage(usize),
    CloseFocused,
    ClosePane(usize),
    /// A directory row of a Family B file-browser pane was activated: point pane
    /// `usize` at the new directory. The pane keeps its kind and its uid — a browser
    /// navigating is not a new pane — so nothing about the session model moves.
    ViewNavigate(usize, String),
    /// Open a Family B file-browser pane rooted at pane `usize`'s live cwd. The in-app
    /// twin of [`Command::RevealPaneCwd`]: same starting directory, but the listing lands
    /// in a pane instead of the OS file explorer. This is the only way a human can reach
    /// a non-PTY view pane, so it is deliberately next to "Open Folder" in the pane menu.
    OpenFileBrowser(usize),

    // ---- left panel: Files mode (D14/D15) ----
    //
    // Every row of the explorer dispatches one of these. They carry a **path**, never a row
    // index, because the row list is rebuilt from disk on each of these commands and an index
    // captured before the rebuild would name a different file after it.
    /// Show `path` in the left panel's Files tree: switch the panel to Files mode, re-root if
    /// the path lies outside the current root, open the directories leading to it, and select
    /// it. `line`/`col` come from a `file:line:col` hit in a pane's output and are held for
    /// whichever tool opens the file next.
    ///
    /// This is where a plain click on a filename in a pane lands. It deliberately opens
    /// nothing: the human picks the tool from the row menu, which is the whole reason a
    /// clicked path goes to the panel instead of straight to the OS handler.
    RevealInFiles {
        path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
    /// Single click on an explorer row: a directory opens/shuts, a file is selected.
    FilesClick(String),
    OpenFileContext(String, f32, f32),
    /// Open or shut an explorer directory without selecting anything (the chevron).
    /// Double click on an explorer file: open it in a read-only view pane — the Markdown
    /// renderer for `.md`, the plain file viewer otherwise.
    FilesOpen(String),
    /// The finder box changed. A non-empty query replaces the tree with ranked matches.
    FilesSetQuery(String),
    /// Re-root the explorer at `0` (the row menu's "Browse Containing Folder", and a
    /// double-click on a directory).
    FilesSetRoot(String),
    /// Re-root one directory higher — the way out when the derived root guessed too narrowly.
    FilesUp,
    /// Re-read the tree from disk. Nothing watches the filesystem, so this is how a human
    /// says "I changed something outside the app".
    FilesRefresh,
    // ---- left panel: Git mode (J) ----
    //
    // Read-only: there is deliberately no Stage / Unstage / Discard here. The rows carry
    // git's own REPO-RELATIVE path, which `State::git_abs` resolves against the repo root —
    // the panel speaks git's identity for a file, and everything downstream speaks the
    // filesystem's.
    /// Single click on a git row: select it.
    GitClick(String),
    /// Double click on a git row: open it in a read-only view pane, exactly as the explorer
    /// does — one behaviour for "open this file", reached from two lists.
    GitOpen(String),
    /// Right click on a git row: the explorer's own file menu, anchored at `1`, `2`.
    GitContext(String, f32, f32),
    /// Re-run `git status`. Nothing watches the repository, so this is how a human says "I
    /// just committed something in a pane".
    GitRefresh,
    /// Open `path` in a terminal pane running tool `tool` — "open in a terminal with vi".
    /// The pane is a `Tool` pane, so it gets the tool's brand and icon like any other.
    OpenPathWith {
        path: String,
        tool: String,
    },
    /// Copy an arbitrary path to the clipboard (the row menu). Goes through the focused
    /// pane's clipboard so it raises the same "Copied …" toast as a Ctrl+click does.
    CopyPathText(String),
    /// Show `path` in the OS file explorer (the row menu's "Reveal in Finder").
    RevealPath(String),
    FocusPane(usize),
    FocusDir(Direction),
    // layout
    SetLayout(Layout),
    CycleLayout,
    ToggleZoom,
    ToggleFullscreen,
    // font zoom (Ctrl+= / Ctrl+- / Ctrl+0)
    /// Nudge the global terminal font size by `0` px (clamped), re-gridding every pane.
    FontZoom(i32),
    /// Reset the global terminal font size to its default.
    FontReset,
    ResizeDivider {
        kind: DividerKind,
        index: i32,
        delta: f64,
    },
    // tabs
    NewTab,
    CloseTab(usize),
    SwitchTab(usize),
    /// Switch to the next tab, wrapping around (Ctrl+Tab).
    NextTab,
    /// Switch to the previous tab, wrapping around (Ctrl+Shift+Tab).
    PrevTab,
    BeginRename(i32),
    RenameTab(i32, String),
    /// Begin editing pane `0`'s label inline (double-click on its header).
    BeginRenamePane(i32),
    /// Commit pane `0`'s label to `1` (blank keeps the prior label).
    RenamePane(i32, String),
    // ---- pane context-menu actions (target a specific pane by active-tab index) ----
    /// Recolor pane `0` to swatch `1` of the active frame palette (pins it + frame/dot on).
    RecolorPane(usize, usize),
    /// Set pane `0`'s per-pane frame override to `1`.
    SetPaneFrame(usize, bool),
    /// Set pane `0`'s per-pane dot override to `1`.
    SetPaneDot(usize, bool),
    /// Toggle whether pane `0`'s ambient-AI summary line is muted.
    ToggleMuteAi(usize),
    /// Toggle whether pane `0`'s "talk" (speak new Claude assistant replies aloud) is on.
    ToggleTalk(usize),
    // ---- speech (global; routed to the ControlHost's SpeechService) ----
    /// Kill any in-flight/queued speech immediately (command palette "Speech: Stop Now").
    SpeechStopNow,
    /// Toggle the global speech mute flag.
    SpeechToggleMuted,
    /// Toggle "only speak the focused pane" (background talkers stay silent while unfocused).
    SpeechToggleFocusedOnly,
    /// Maximize/restore (zoom-in-tab) pane `0`.
    ZoomPane(usize),
    /// Fullscreen/exit-fullscreen pane `0`.
    FullscreenPane(usize),
    /// Restart pane `0`'s shell (kills + respawns its session in place).
    RestartPane(usize),
    /// Re-resolve a FRESH (registry-backed) environment and restart pane `0`'s shell in
    /// place, keeping its live cwd + env overrides (#28; the pane menu's "Refresh Env").
    RefreshEnvPane(usize),
    /// Open pane `0`'s current working directory in the OS file explorer (#23).
    RevealPaneCwd(usize),
    /// Route a URL through Preferences → Browser: the OS default handler, one chosen
    /// browser, or the [`crate::state::Overlay::AskBrowser`] chooser. The single entry
    /// point for "something in a pane wants a link opened", so the setting can never be
    /// bypassed by a caller that opens a URL directly.
    OpenLink(String),
    /// Answer the browser chooser: open its held URL in browser row `0`, then close it.
    PickBrowser(usize),
    /// Open the in-pane search box on pane `0`.
    SearchPane(usize),
    /// Open the in-pane search box on the focused pane (the Ctrl+F keybinding).
    SearchFocused,
    /// Copy pane `0`'s current selection to the clipboard.
    CopyPane(usize),
    /// Copy the focused pane's selection (the Ctrl+Shift+C keybinding) — the explicit copy
    /// gesture now that copy-on-select defaults off. No-op without a selection.
    CopyFocused,
    /// Paste the clipboard into pane `0`'s session.
    PastePane(usize),
    /// Paste the clipboard into the focused pane's session (the Ctrl+V keybinding). Reads
    /// the OS clipboard fresh app-side (arboard, with open retries) instead of forwarding a
    /// raw 0x16 for the shell to resolve — PSReadLine's own clipboard read has no retry and
    /// can come up empty/stale right after an external copy (#9). Unbinding `pane.paste`
    /// in Preferences restores the literal-0x16 passthrough for shells that want it.
    PasteFocused,
    /// Forward a literal Ctrl+V (0x16) to the focused pane (the Alt+V keybinding) so an in-pane
    /// TUI that reads the OS clipboard itself — e.g. Claude Code's image paste — can pull a
    /// clipboard IMAGE. hyperpanes' text paste can't carry image bytes through the pty; this
    /// hands the clipboard read to the focused program. Matches the shortcut Claude Code
    /// documents for terminals that intercept Ctrl+V.
    PasteImageFocused,
    /// Select all of pane `0`'s viewport.
    SelectAllPane(usize),
    /// Clear pane `0`'s screen + scrollback.
    ClearPane(usize),
    // ---- reminder panes (Track F) ----
    /// Park pane `0` until quick-offset `1` from now: it leaves the layout but its session
    /// stays alive; it lives in the sidebar bell list until restored.
    RemindPane(usize, ReminderOffset),
    /// Toggle the sidebar bell's reminder-list panel.
    ToggleReminders,
    /// Re-dock the parked pane with session uid `0` into the active tab + clear its reminder.
    RestoreReminder(String),
    /// Hide the fired-reminder alert toast for session uid `0` (the reminder + bell badge stay).
    DismissReminderToast(String),
    /// Move pane `0` into a brand-new tab (disabled when its tab has <2 panes).
    MovePaneToNewTab(usize),
    /// Move pane `0` into existing tab `1`.
    MovePaneToTab(usize, usize),
    // ---- tab context-menu actions (target a specific tab by index) ----
    /// Duplicate tab `0` (a fresh tab with the same number of panes + its layout).
    DuplicateTab(usize),
    /// Close every tab except tab `0`.
    CloseOtherTabs(usize),
    /// Close every tab to the right of tab `0`.
    CloseTabsToRight(usize),
    /// Reopen the most-recently closed tab (replay-primed; no-op when none).
    ReopenClosedTab,
    /// Set tab `0`'s layout to `1`.
    SetTabLayout(usize, Layout),
    /// Move the whole of tab `0` to a new OS window.
    MoveTabToNewWindow(usize),
    // ---- context-menu lifecycle ----
    /// Open the pane context menu for pane `0` at window-logical `(1, 2)`.
    OpenPaneContext(usize, f32, f32),
    /// Open the single-layout taskbar's pane menu for pane `0` at `(1, 2)` (the `inTaskbar`
    /// variant: a leading Show row, no Maximize).
    OpenTaskbarContext(usize, f32, f32),
    /// Open the tab context menu for tab `0` at window-logical `(1, 2)`.
    OpenTabContext(usize, f32, f32),
    /// Open the application (hamburger) menu at window-logical `(0, 1)`.
    OpenAppContext(f32, f32),
    /// Dismiss the open context menu.
    CloseContext,
    // ---- workspace file (application menu) ----
    /// Pick a `workspace.json` and load it (the application menu's "Open workspace…").
    OpenWorkspace,
    /// Serialize the active tab and save it to a chosen file (the menu's "Save workspace…").
    /// Writes back silently to the remembered path once the workspace has one.
    SaveWorkspace,
    // ---- workspace library + sets (M6) ----
    /// Always prompt for a destination, save the active tab there, and remember it.
    SaveWorkspaceAs,
    /// Write the active tab into the checkout it is working in, as
    /// `.hyperpanes/project.json` — the layout travels with the repo, not the laptop.
    SaveProject,
    /// Save every non-empty tab as a member workspace and index them in a `sets/*.json`.
    SaveSet,
    /// Pick a saved set and load every member workspace (reattach-or-spawn per pane).
    OpenSet,
    // ---- multi-window (Phase 4) ----
    /// Open a fresh OS window with an empty tab.
    NewWindow,
    /// Re-host the focused pane in a new OS window (replay-primed, no PTY restart).
    MovePaneToNewWindow,
    // ---- Wave-2 overlays (Seam #3) ----
    /// Dismiss whatever overlay panel is open.
    CloseOverlay,
    // command palette
    PaletteOpen,
    PaletteQuery(String),
    /// Move the highlighted palette row by ±1.
    PaletteNav(i32),
    /// Highlight a specific visible palette row (a mouse hover/click).
    PaletteSelect(usize),
    /// Run the highlighted palette row's command (then close the palette).
    PaletteActivate,
    // preferences
    PrefsOpen,
    ApplySetting(Setting),
    /// Edit the appearance draft (previews only; commits on Done).
    DraftSetting(Setting),
    /// Commit the appearance draft and close (the Done button / Save).
    PrefsDone,
    /// Resolve the save/discard prompt: 0 keep · 1 discard · 2 save.
    PrefsConfirm(i32),
    /// Font picker: select option `i` (== FONT_OPTIONS.len() → Custom… mode).
    FontSelect(usize),
    /// Font picker: set the custom font path typed in the Custom… field.
    FontCustomValue(String),
    // sidebar / projects
    /// Show/hide the whole right-edge rail.
    ToggleSidebar,
    /// Expand/collapse the projects flyout behind the 📁 icon.
    ToggleProjects,
    OpenProject(usize),
    /// Recolor flyout row `0` to palette swatch `1`.
    SetProjectColor(usize, usize),
    /// Rename flyout row `0` to `1`.
    RenameProject(usize, String),
    /// Forget flyout row `0`.
    RemoveProject(usize),
    /// Open the "Add project" dialog (the ＋ on the sidebar's PROJECTS header).
    OpenAddProject,
    /// Submit the Add-Project dialog with the typed directory path (validated in state;
    /// a bad path keeps the dialog open with an inline error).
    SubmitAddProject(String),
    // ---- the left slide-out panel (mux plan M5) ----
    /// Show/hide the left panel (workspace tree · library · detached sessions).
    ToggleLeftPanel,
    /// Workspace tree: focus pane `1` of tab `0` (switching to that tab first).
    LeftFocusPane(usize, usize),
    /// Workspace tree: drag pane `1` of tab `0` onto tab `2`, landing at insertion index `3`
    /// among that tab's panes (re-host, no PTY restart).
    LeftMovePane(usize, usize, usize, usize),
    /// Workspace tree: drag pane `1` of tab `0` to insertion index `2` within its OWN tab —
    /// the same gesture as a cross-group drop, resolved as a reorder because it never left.
    LeftReorderPane(usize, usize, usize),
    /// Library: load saved workspace row `0` as new tabs.
    LeftOpenWorkspace(usize),
    /// Library: save the active tab into the workspace library (no file dialog).
    LeftSaveWorkspace,
    /// A SETS row clicked: open every member workspace of set `0` (index into the panel's
    /// set list) as its own tab.
    LeftOpenSet(usize),
    /// The SETS header's save button: store every non-empty tab as a new named set.
    LeftSaveSet,
    /// Detached: adopt live session uid `0` into the active tab (re-attach + replay).
    LeftAdoptSession(String),
}

/// A side effect the controller must apply outside the state (UI/window layer). The
/// multi-window layer ([`crate::app`]) applies these against the owning window + the
/// app-level window registry.
#[derive(Debug)]
pub enum Effect {
    None,
    /// The workspace is empty — close this window (and quit when it was the last).
    Quit,
    /// Apply OS fullscreen (true) or restore (false) to this window.
    SetFullscreen(bool),
    /// Open a fresh empty OS window.
    NewWindow,
    /// Re-host `det` in a new OS window; `source_alive` is `false` when detaching it
    /// emptied this window (so the controller closes it).
    MoveToNewWindow {
        det: DetachedPane,
        source_alive: bool,
    },
    /// Re-host a whole tab (its panes, title + layout) in a new OS window. `source_alive`
    /// is `false` when moving it emptied this window.
    MoveTabToNewWindow {
        tab: DetachedTab,
        source_alive: bool,
    },
    /// Speech commands route through the `ControlHost`'s `SpeechService` (owned above `State`),
    /// so `dispatch` bubbles them up as effects rather than mutating state directly.
    SpeechStopNow,
    SpeechToggleMuted,
    SpeechToggleFocusedOnly,
}

/// The keyboard layout-cycle order (skips `single`, which the menu still offers).
const LAYOUT_CYCLE: [Layout; 5] = [
    Layout::Auto,
    Layout::Columns,
    Layout::Rows,
    Layout::Grid,
    Layout::MainStack,
];

/// Run `cmd` against `state`. Returns any [`Effect`] the caller must apply.
pub fn dispatch(state: &mut State, cmd: Command, mgr: &SessionManager) -> Effect {
    // Any action other than renaming itself cancels an in-progress tab rename,
    // so the inline edit box never lingers when you interact elsewhere.
    if state.editing_tab != -1 && !matches!(cmd, Command::BeginRename(_) | Command::RenameTab(..)) {
        state.editing_tab = -1;
        state.dirty = true;
    }
    // Likewise, any action other than a pane rename cancels an in-progress pane-label edit
    // (so the inline box never lingers when you interact elsewhere).
    if state.editing_pane != -1
        && !matches!(cmd, Command::BeginRenamePane(_) | Command::RenamePane(..))
    {
        state.editing_pane = -1;
        state.dirty = true;
    }
    match cmd {
        Command::NewPane => state.add_pane(mgr),
        Command::OpenNewPane => state.open_new_pane(),
        Command::SubmitNewPane(opts) => {
            state.add_pane_opts(mgr, opts);
            state.close_overlay();
        }
        Command::OpenNewGoal => state.open_new_goal(),
        Command::GoalQuery(q) => state.goal_set_text(q),
        Command::GoalNav(d) => state.goal_nav(d),
        Command::GoalToggleOptions => state.goal_toggle_options(),
        Command::GoalCollapse => state.goal_collapse(),
        Command::GoalMenu(open) => state.goal_menu_toggle(open),
        Command::GoalMenuNav(d) => state.goal_menu_nav(d),
        Command::GoalMenuPick => state.goal_menu_pick(),
        Command::GoalFieldClick(i) => state.goal_field_click(i),
        Command::GoalMenuClick(i) => state.goal_menu_click(i),
        Command::GoalSubmit => state.goal_submit(mgr),
        Command::GoalPasteClipboard => state.goal_paste_clipboard(),
        Command::GoalAttachImage => state.goal_attach_images(),
        Command::GoalPasteImage => {
            state.goal_paste_image();
        }
        Command::GoalRemoveImage(i) => state.goal_remove_image(i),
        Command::CloseFocused => {
            let f = state.active_tab().focused;
            if !state.close_pane(f, mgr) {
                return Effect::Quit;
            }
        }
        Command::ClosePane(i) => {
            if !state.close_pane(i, mgr) {
                return Effect::Quit;
            }
        }
        Command::FocusPane(i) => state.focus_pane(i),
        Command::ViewNavigate(i, target) => state.view_navigate(i, target),
        Command::FocusDir(d) => state.focus_dir(d),
        Command::SetLayout(l) => state.set_layout(l),
        Command::CycleLayout => {
            let cur = state.active_tab().layout;
            let i = LAYOUT_CYCLE.iter().position(|l| *l == cur).unwrap_or(0);
            state.set_layout(LAYOUT_CYCLE[(i + 1) % LAYOUT_CYCLE.len()]);
        }
        Command::ToggleZoom => state.toggle_zoom(),
        Command::ToggleFullscreen => {
            let on = !state.fullscreen;
            state.set_fullscreen(on);
            return Effect::SetFullscreen(on);
        }
        Command::FontZoom(delta) => state.font_zoom(delta),
        Command::FontReset => state.font_reset(),
        Command::ResizeDivider { kind, index, delta } => state.resize_divider(kind, index, delta),
        Command::NewTab => state.new_tab(mgr),
        Command::CloseTab(i) => {
            // Reopenable close: with ≥2 tabs the tab is parked (sessions alive) on the closed
            // stack; closing the last tab still kills + quits.
            if !state.close_tab_menu(i, mgr) {
                return Effect::Quit;
            }
        }
        Command::SwitchTab(i) => state.switch_tab(i),
        Command::NextTab => state.cycle_tab(1),
        Command::PrevTab => state.cycle_tab(-1),
        Command::BeginRename(i) => state.begin_rename(i),
        Command::RenameTab(i, t) => state.rename_tab(i, &t),
        Command::BeginRenamePane(i) => state.begin_rename_pane(i),
        Command::RenamePane(i, t) => state.rename_pane(i, &t),
        // ---- pane context-menu actions ----
        Command::RecolorPane(i, swatch) => state.recolor_pane(i, swatch),
        Command::SetPaneFrame(i, on) => state.set_pane_frame(i, on),
        Command::SetPaneDot(i, on) => state.set_pane_dot(i, on),
        Command::ToggleMuteAi(i) => state.toggle_mute_ai(i),
        Command::ToggleTalk(i) => state.toggle_talk(i),
        Command::SpeechStopNow => return Effect::SpeechStopNow,
        Command::SpeechToggleMuted => return Effect::SpeechToggleMuted,
        Command::SpeechToggleFocusedOnly => return Effect::SpeechToggleFocusedOnly,
        Command::ZoomPane(i) => state.zoom_pane(i),
        Command::FullscreenPane(i) => {
            state.focus_pane(i);
            let on = !state.fullscreen;
            state.set_fullscreen(on);
            return Effect::SetFullscreen(on);
        }
        Command::RestartPane(i) => state.restart_pane(i, mgr),
        Command::RefreshEnvPane(i) => state.refresh_env_pane(i, mgr),
        Command::RevealPaneCwd(i) => {
            // Open the pane's live cwd (reported by shell integration) in the OS file explorer.
            // This used to branch `explorer` / `xdg-open` inline, which meant it did nothing
            // at all on macOS (no `xdg-open` there); `core::open` owns the per-OS launch now.
            if let Some(cwd) = state.active_tab().panes.get(i).and_then(|p| p.cwd.clone()) {
                if let Err(e) = hyperpanes_core::open::reveal_path(std::path::Path::new(&cwd)) {
                    crate::dbg_log(&format!("RevealPaneCwd {cwd}: {e}"));
                }
            }
        }
        Command::OpenLink(url) => {
            // The routing itself lives in `State::open_link` (it may mount an overlay, so it
            // needs `&mut State`); all that's left here is saying why a link went nowhere.
            if let Err(e) = state.open_link(&url) {
                crate::dbg_log(&format!("OpenLink: {e}"));
            }
        }
        Command::PickBrowser(i) => {
            if let Err(e) = state.pick_browser(i) {
                crate::dbg_log(&format!("PickBrowser {i}: {e}"));
            }
        }
        Command::OpenFileBrowser(i) => {
            // The pane's live cwd if shell integration reported one, else its configured
            // cwd, else home — a browser with nowhere to start is worse than one that
            // starts somewhere obvious.
            let start = state
                .active_tab()
                .panes
                .get(i)
                .and_then(|p| p.cwd.clone().filter(|c| !c.is_empty()))
                .or_else(|| {
                    std::env::var("HOME")
                        .ok()
                        .or_else(|| std::env::var("USERPROFILE").ok())
                });
            let label = start
                .as_deref()
                .map(std::path::Path::new)
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .or_else(|| Some("Files".to_string()));
            state.add_pane_opts(
                mgr,
                NewPaneOpts {
                    label,
                    // A view pane's target IS its cwd — see `State::view_navigate`.
                    cwd: start,
                    command: None,
                    shell: None,
                    accent: None,
                    show_frame: None,
                    show_dot: None,
                    env: None,
                    startup: None,
                    kind: Some(hyperpanes_core::tools::kind::PaneKind::FileBrowser),
                    // A view pane holds a directory, not a conversation.
                    session: None,
                },
            );
        }

        // ---- left panel: Files mode (D14/D15) ----
        Command::RevealInFiles { path, line, col } => {
            state.reveal_in_files(std::path::Path::new(&path), line, col);
        }
        Command::FilesClick(path) => state.files_click(std::path::Path::new(&path)),
        Command::OpenFileContext(path, x, y) => {
            state.open_file_context(std::path::Path::new(&path), x, y)
        }
        Command::FilesSetQuery(q) => state.files_set_query(q),
        Command::FilesSetRoot(dir) => state.files_set_root(std::path::PathBuf::from(dir)),
        Command::FilesUp => state.files_go_up(),
        Command::FilesRefresh => {
            // Entering the mode (or pressing refresh) re-anchors first: the explorer is
            // rooted on the SELECTED pane, and focus may well have moved while another
            // mode was on screen.
            state.sync_left_root(crate::paneview::LEFT_MODE_FILES);
            state.rebuild_files();
        }
        // ---- left panel: Git mode (J) ----
        Command::GitRefresh => {
            state.sync_left_root(crate::paneview::LEFT_MODE_GIT);
            state.rebuild_git();
        }
        Command::GitClick(path) => state.git_click(&path),
        Command::GitOpen(path) => {
            // Resolved here and re-dispatched rather than duplicated: a git row and an
            // explorer row must open a file the same way, and there is one implementation.
            let Some(abs) = state.git_abs(&path) else {
                return Effect::None;
            };
            return dispatch(
                state,
                Command::FilesOpen(abs.to_string_lossy().into_owned()),
                mgr,
            );
        }
        Command::GitContext(path, x, y) => {
            if let Some(abs) = state.git_abs(&path) {
                state.git_click(&path);
                state.open_file_context(&abs, x, y);
            }
        }
        Command::FilesOpen(path) => {
            let p = std::path::PathBuf::from(&path);
            if p.is_dir() {
                state.files_set_root(p);
                return Effect::None;
            }
            // `.md` gets the renderer, everything else the plain viewer — the same split the
            // pane menu makes, so a file opens the same way however it was reached.
            let md = p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown")
            });
            let kind = if md {
                hyperpanes_core::tools::kind::PaneKind::Markdown
            } else {
                hyperpanes_core::tools::kind::PaneKind::FileViewer
            };
            let label = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .or_else(|| Some("File".to_string()));
            state.add_pane_opts(
                mgr,
                NewPaneOpts {
                    label,
                    // A view pane's target IS its cwd — see `State::view_navigate`.
                    cwd: Some(path),
                    command: None,
                    shell: None,
                    accent: None,
                    show_frame: None,
                    show_dot: None,
                    env: None,
                    startup: None,
                    kind: Some(kind),
                    // A view pane holds a file, not a conversation.
                    session: None,
                },
            );
        }
        Command::OpenPathWith { path, tool } => {
            // The tool's resolved binary, so a user override in Preferences → Tools is what
            // actually runs. Falling back to the registry's bare bin name lets PATH decide,
            // which is right when the override was cleared but the tool is still installed.
            let Some(def) = hyperpanes_core::tools::registry::by_id(&tool) else {
                crate::dbg_log(&format!("OpenPathWith: unknown tool {tool}"));
                return Effect::None;
            };
            let bin = hyperpanes_core::tools::detect::resolve(def, &state.settings.tool_paths)
                .map(|r| r.path.display().to_string())
                .unwrap_or_else(|| def.bin.to_string());
            let p = std::path::PathBuf::from(&path);
            // The editor runs *in* the file's directory, so its own file-relative commands
            // (`:e ../other`, a project search) mean what the human expects.
            let cwd = p
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.display().to_string());
            let label = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .or_else(|| Some(def.name.to_string()));
            state.add_pane_opts(
                mgr,
                NewPaneOpts {
                    label,
                    cwd,
                    command: Some(format!("{} {}", quote_arg(&bin), quote_arg(&path))),
                    shell: None,
                    accent: None,
                    show_frame: None,
                    show_dot: None,
                    env: None,
                    startup: None,
                    kind: Some(hyperpanes_core::tools::kind::PaneKind::Tool(tool)),
                    // "Open this file in vim" starts an editor on a file — there is no
                    // conversation to come back to.
                    session: None,
                },
            );
        }
        Command::CopyPathText(path) => {
            let f = state.active_tab().focused;
            state.copy_link_text(f, &path);
        }
        Command::RevealPath(path) => {
            if let Err(e) = hyperpanes_core::open::reveal_path(std::path::Path::new(&path)) {
                crate::dbg_log(&format!("RevealPath {path}: {e}"));
            }
        }
        Command::SearchPane(i) => state.open_search(i),
        Command::SearchFocused => {
            let f = state.active_tab().focused;
            state.open_search(f);
        }
        Command::CopyPane(i) => state.copy_pane(i),
        Command::CopyFocused => {
            let f = state.active_tab().focused;
            state.copy_pane(f);
        }
        Command::PastePane(i) => state.paste_pane(i, mgr),
        Command::PasteFocused => {
            let f = state.active_tab().focused;
            state.paste_pane(f, mgr);
        }
        Command::PasteImageFocused => {
            let f = state.active_tab().focused;
            state.paste_image_focused(f, mgr);
        }
        Command::SelectAllPane(i) => state.select_all_pane(i),
        Command::ClearPane(i) => state.clear_pane(i),
        // ---- reminder panes ----
        Command::RemindPane(i, off) => state.remind_pane(i, off),
        Command::ToggleReminders => state.toggle_reminders(),
        Command::RestoreReminder(uid) => state.restore_reminder(&uid, mgr),
        Command::DismissReminderToast(uid) => state.dismiss_reminder_toast(&uid),
        Command::MovePaneToNewTab(i) => state.move_pane_to_new_tab(i, mgr),
        Command::MovePaneToTab(i, t) => state.move_pane_to_tab(i, t, mgr),
        // ---- tab context-menu actions ----
        Command::DuplicateTab(i) => state.duplicate_tab(i, mgr),
        Command::CloseOtherTabs(i) => state.close_other_tabs(i, mgr),
        Command::CloseTabsToRight(i) => state.close_tabs_to_right(i, mgr),
        Command::ReopenClosedTab => state.reopen_closed_tab(mgr),
        Command::SetTabLayout(i, l) => state.set_tab_layout(i, l),
        Command::MoveTabToNewWindow(i) => {
            if let Some((tab, source_alive)) = state.detach_tab(i) {
                return Effect::MoveTabToNewWindow { tab, source_alive };
            }
        }
        // ---- context-menu lifecycle ----
        Command::OpenPaneContext(i, x, y) => state.open_pane_context(i, x, y),
        Command::OpenTaskbarContext(i, x, y) => state.open_taskbar_context(i, x, y),
        Command::OpenTabContext(i, x, y) => state.open_tab_context(i, x, y),
        Command::OpenAppContext(x, y) => state.open_app_context(x, y),
        Command::CloseContext => state.close_context(),
        // ---- workspace file (application menu) ----
        Command::OpenWorkspace => state.open_workspace(mgr),
        Command::SaveWorkspace => state.save_workspace(),
        // ---- workspace library + sets (M6) ----
        Command::SaveWorkspaceAs => state.save_workspace_as(),
        Command::SaveProject => state.save_project(),
        Command::SaveSet => state.save_set(),
        Command::OpenSet => state.open_set(mgr),
        // ---- multi-window ----
        Command::NewWindow => return Effect::NewWindow,
        Command::MovePaneToNewWindow => {
            if let Some((det, source_alive)) = state.detach_focused(mgr) {
                return Effect::MoveToNewWindow { det, source_alive };
            }
        }
        // ---- Wave-2 overlays ----
        Command::CloseOverlay => state.close_overlay(),
        // Ctrl+Shift+P TOGGLES (the binding id is `palette.toggle`, matching the renderer):
        // pressed with the palette already up it dismisses instead of resetting the query.
        Command::PaletteOpen => {
            if state.overlay == crate::state::Overlay::Palette {
                state.close_overlay();
            } else {
                state.open_palette();
            }
        }
        Command::PaletteQuery(q) => state.palette_set_query(&q),
        Command::PaletteNav(d) => state.palette_nav(d),
        Command::PaletteSelect(i) => state.palette_select(i),
        Command::PaletteActivate => {
            // Run the highlighted entry's command through the same dispatch, then close.
            if let Some(inner) = state.palette_command() {
                state.close_overlay();
                return dispatch(state, inner, mgr);
            }
            state.close_overlay();
        }
        Command::PrefsOpen => state.open_prefs(),
        Command::ApplySetting(s) => state.apply_setting(s),
        Command::DraftSetting(s) => state.draft_setting(s),
        Command::PrefsDone => state.prefs_done(),
        Command::PrefsConfirm(a) => state.prefs_confirm_resolve(a),
        Command::FontSelect(i) => state.font_select(i),
        Command::FontCustomValue(v) => state.font_custom_value(v),
        Command::ToggleSidebar => state.toggle_sidebar(),
        Command::ToggleProjects => state.toggle_projects(),
        Command::OpenProject(i) => state.open_project(i, mgr),
        Command::SetProjectColor(i, swatch) => state.set_project_color(i, swatch),
        Command::RenameProject(i, name) => state.rename_project(i, &name),
        Command::RemoveProject(i) => state.remove_project(i),
        Command::OpenAddProject => state.open_add_project(),
        Command::SubmitAddProject(path) => state.submit_add_project(&path),
        // ---- the left slide-out panel ----
        Command::ToggleLeftPanel => state.toggle_left_panel(),
        Command::LeftFocusPane(ti, i) => state.focus_pane_in_tab(ti, i),
        Command::LeftMovePane(from, i, to, at) => {
            state.move_pane_between_tabs_at(from, i, to, at, mgr)
        }
        Command::LeftReorderPane(ti, from, to) => state.reorder_pane_in(ti, from, to),
        Command::LeftOpenWorkspace(i) => state.open_workspace_from_library(i, mgr),
        Command::LeftSaveWorkspace => state.save_workspace_to_library(),
        Command::LeftOpenSet(i) => state.open_set_from_library(i, mgr),
        Command::LeftSaveSet => state.save_set_to_library(),
        Command::LeftAdoptSession(uid) => state.adopt_detached_session(&uid, mgr),
    }
    Effect::None
}

/// Map a layout menu id (from the Slint picker) to a `SetLayout` command.
pub fn set_layout_from_id(id: i32) -> Command {
    Command::SetLayout(theme::layout_from_id(id))
}

/// Wrap a path (or a program path) for the shell that runs a pane's `command`. Single quotes
/// with `'\''` for an embedded quote — the one form that is literal for every character in
/// POSIX shells, which matters here because filenames chosen by humans contain spaces,
/// parentheses and `$` far more often than they contain apostrophes.
///
/// A bare word is left alone so the common case reads as itself in the pane header.
fn quote_arg(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '=' | ':' | '~')
        });
    if plain {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod quote_tests {
    use super::quote_arg;

    #[test]
    fn an_ordinary_path_is_left_as_it_reads() {
        assert_eq!(quote_arg("/usr/bin/vim"), "/usr/bin/vim");
        assert_eq!(quote_arg("src/state.rs"), "src/state.rs");
    }

    #[test]
    fn a_path_with_shell_metacharacters_is_quoted_whole() {
        assert_eq!(quote_arg("/tmp/my notes.md"), "'/tmp/my notes.md'");
        assert_eq!(quote_arg("/tmp/$HOME (1).txt"), "'/tmp/$HOME (1).txt'");
    }

    #[test]
    fn an_apostrophe_closes_and_reopens_the_quoting() {
        // The classic: 'it'\''s' — four tokens the shell concatenates back into `it's`.
        assert_eq!(quote_arg("it's"), r"'it'\''s'");
    }
}
