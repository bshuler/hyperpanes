//! Palette, layout metadata, and font loading — the small presentation helpers the
//! controller reaches for. No state here; pure look-up tables + `load_font`.

use std::borrow::Cow;

use hyperpanes_core::layout::presets::Layout;
use hyperpanes_terminal_widget::Font;
use slint::Color;

/// The selectable frame palettes (pane dot + frame-border colors), the native port of
/// the renderer's `theme.ts` `PALETTES`. Every palette shares the same 8 slots in the
/// same order — red · amber · green · blue · purple · pink · teal · yellow — so a pane
/// created at index `i` keeps its logical hue when the active palette changes (the accent
/// is recomputed by index against the new palette). Which one is active is the
/// `frame_palette` appearance setting; index 0 (Muted) is the default.
pub const FRAME_PALETTES: [(&str, [(u8, u8, u8); 8]); 4] = [
    // "Muted" — the original saturated set (renderer `dark`), kept as the default.
    (
        "Muted",
        [
            (0xe5, 0x48, 0x4d), // red
            (0xf5, 0xa6, 0x23), // amber
            (0x30, 0xa4, 0x6c), // green
            (0x3b, 0x82, 0xf6), // blue
            (0xa8, 0x55, 0xf7), // purple
            (0xec, 0x48, 0x99), // pink
            (0x14, 0xb8, 0xa6), // teal
            (0xea, 0xb3, 0x08), // yellow
        ],
    ),
    // "Vivid" — bold, fully-saturated hues (renderer `medium`).
    (
        "Vivid",
        [
            (0xff, 0x40, 0x40),
            (0xff, 0xa1, 0x2e),
            (0x21, 0xc2, 0x5c),
            (0x35, 0x73, 0xf0),
            (0xad, 0x44, 0xf2),
            (0xf7, 0x3d, 0x92),
            (0x14, 0xc8, 0xb6),
            (0xf7, 0xcb, 0x24),
        ],
    ),
    // "Neon" — brightest, near-pure colors (renderer `light`).
    (
        "Neon",
        [
            (0xff, 0x1a, 0x1a),
            (0xff, 0x88, 0x00),
            (0x00, 0xdd, 0x33),
            (0x2e, 0x8b, 0xff),
            (0xc0, 0x26, 0xff),
            (0xff, 0x1f, 0x8c),
            (0x00, 0xe6, 0xcf),
            (0xff, 0xe0, 0x00),
        ],
    ),
    // "Grayscale" — 8 distinct grays, all readable against the dark UI.
    (
        "Grayscale",
        [
            (0xe0, 0xe0, 0xe0),
            (0xc8, 0xc8, 0xc8),
            (0xb0, 0xb0, 0xb0),
            (0x98, 0x98, 0x98),
            (0x80, 0x80, 0x80),
            (0x6a, 0x6a, 0x6a),
            (0x56, 0x56, 0x56),
            (0x44, 0x44, 0x44),
        ],
    ),
];

/// Clamp a (possibly stale) palette index to a real palette, returning its 8 slots
/// (defaults to index 0 = Muted).
pub fn frame_palette(idx: usize) -> &'static [(u8, u8, u8); 8] {
    &FRAME_PALETTES[idx.min(FRAME_PALETTES.len() - 1)].1
}

/// The selectable terminal colour themes (the terminal's own bg/fg + 16 ANSI colours),
/// the native port of the renderer's `TERMINAL_THEMES`. Each is the 16 base colours the
/// glyph grid uses: index 0 = background, 7 = foreground, 1–6 the ANSI colours, 8–15 the
/// bright variants (see `terminal-widget`'s `set_base16`). Index 0 (Dark) is the default.
pub const TERMINAL_THEMES: [(&str, [[u8; 3]; 16]); 4] = [
    // "Dark" — Catppuccin Mocha (the original look).
    (
        "Dark",
        [
            [0x11, 0x11, 0x1b], // bg
            [0xf3, 0x8b, 0xa8], // red
            [0xa6, 0xe3, 0xa1], // green
            [0xf9, 0xe2, 0xaf], // yellow
            [0x89, 0xb4, 0xfa], // blue
            [0xf5, 0xc2, 0xe7], // magenta
            [0x94, 0xe2, 0xd5], // cyan
            [0xcd, 0xd6, 0xf4], // fg
            [0x58, 0x5b, 0x70], // bright black
            [0xf3, 0x8b, 0xa8],
            [0xa6, 0xe3, 0xa1],
            [0xf9, 0xe2, 0xaf],
            [0x89, 0xb4, 0xfa],
            [0xf5, 0xc2, 0xe7],
            [0x94, 0xe2, 0xd5],
            [0xa6, 0xad, 0xc8],
        ],
    ),
    // "Black" — pure-black background (OLED-friendly).
    (
        "Black",
        [
            [0x00, 0x00, 0x00],
            [0xff, 0x5c, 0x57],
            [0x5a, 0xf7, 0x8e],
            [0xf3, 0xf9, 0x9d],
            [0x57, 0xc7, 0xff],
            [0xff, 0x6a, 0xc1],
            [0x9a, 0xed, 0xfe],
            [0xe6, 0xe6, 0xe6],
            [0x68, 0x68, 0x68],
            [0xff, 0x5c, 0x57],
            [0x5a, 0xf7, 0x8e],
            [0xf3, 0xf9, 0x9d],
            [0x57, 0xc7, 0xff],
            [0xff, 0x6a, 0xc1],
            [0x9a, 0xed, 0xfe],
            [0xff, 0xff, 0xff],
        ],
    ),
    // "Light" — Catppuccin Latte (light background, light-tuned ANSI).
    (
        "Light",
        [
            [0xef, 0xf1, 0xf5],
            [0xd2, 0x0f, 0x39],
            [0x40, 0xa0, 0x2b],
            [0xdf, 0x8e, 0x1d],
            [0x1e, 0x66, 0xf5],
            [0xea, 0x76, 0xcb],
            [0x17, 0x92, 0x99],
            [0x4c, 0x4f, 0x69],
            [0x6c, 0x6f, 0x85],
            [0xd2, 0x0f, 0x39],
            [0x40, 0xa0, 0x2b],
            [0xdf, 0x8e, 0x1d],
            [0x1e, 0x66, 0xf5],
            [0xea, 0x76, 0xcb],
            [0x17, 0x92, 0x99],
            [0xbc, 0xc0, 0xcc],
        ],
    ),
    // "High contrast" — white-on-black with vivid ANSI colours.
    (
        "High contrast",
        [
            [0x00, 0x00, 0x00],
            [0xff, 0x55, 0x55],
            [0x00, 0xff, 0x00],
            [0xff, 0xff, 0x00],
            [0x5c, 0x5c, 0xff],
            [0xff, 0x55, 0xff],
            [0x00, 0xff, 0xff],
            [0xff, 0xff, 0xff],
            [0x88, 0x88, 0x88],
            [0xff, 0x55, 0x55],
            [0x55, 0xff, 0x55],
            [0xff, 0xff, 0x55],
            [0x7c, 0x7c, 0xff],
            [0xff, 0x7c, 0xff],
            [0x55, 0xff, 0xff],
            [0xff, 0xff, 0xff],
        ],
    ),
];

/// Clamp a (possibly stale) theme index to a real theme, returning its 16 base colours
/// (defaults to index 0 = Dark).
pub fn terminal_theme(idx: usize) -> [[u8; 3]; 16] {
    TERMINAL_THEMES[idx.min(TERMINAL_THEMES.len() - 1)].1
}

/// A colour from a theme's base-16 slot, as a Slint `Color` (used by the preview).
pub fn theme_color(idx: usize, slot: usize) -> Color {
    let c = terminal_theme(idx)[slot.min(15)];
    Color::from_rgb_u8(c[0], c[1], c[2])
}

/// The colour tokens of the app *shell* — top bar, sidebar, menus, overlays. Field-for-field
/// the `Theme` global in `ui/theme.slint`; the controller copies one of these into that
/// global on startup and again whenever the user picks another palette, so a swap is one
/// assignment per token rather than a rebuild.
///
/// Every value is 0xAARRGGBB. Alpha is carried explicitly (rather than the `(u8,u8,u8)`
/// triples [`FRAME_PALETTES`] uses) because two of the tokens — `scrim` and `veil` — are
/// translucent by definition, and a palette that could not set their opacity would leave a
/// light theme wearing a dark theme's shadows.
///
/// Distinct from [`FRAME_PALETTES`] (the eight pane hues) and [`TERMINAL_THEMES`] (the
/// base-16 inside a pane): those two colour a pane's *contents*, this one colours the
/// window around them. All three are independent settings on purpose — a light shell over
/// dark terminals is a normal way to work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiPalette {
    pub name: &'static str,
    pub bg: u32,
    pub mantle: u32,
    pub surface: u32,
    pub surface2: u32,
    pub border: u32,
    pub text: u32,
    pub subtext: u32,
    pub faint: u32,
    pub accent: u32,
    pub danger: u32,
    pub ok: u32,
    pub warn: u32,
    pub link: u32,
    pub scrim: u32,
    pub veil: u32,
}

/// The selectable shell palettes, in picker order. Index 0 (Mocha) is the default and is
/// byte-identical to the literals `ui/theme.slint` still carries as its property defaults —
/// so a build that somehow never pushes a palette looks exactly as it always has.
///
/// The set deliberately spans more than hue: Latte is the light option and High Contrast
/// the accessibility one, because the complaint that produced this table was "I can't tell
/// which tab is selected", and for some eyes the fix is contrast, not colour.
pub const UI_PALETTES: [UiPalette; 5] = [
    // Catppuccin Mocha — the original shell, matched to the Electron app's `:root` vars.
    UiPalette {
        name: "Mocha",
        bg: 0xff11111b,
        mantle: 0xff181825,
        surface: 0xff1e1e2e,
        surface2: 0xff313244,
        border: 0xff313244,
        text: 0xffcdd6f4,
        subtext: 0xff9399b2,
        faint: 0xff6c7086,
        accent: 0xff89b4fa,
        danger: 0xffe5484d,
        ok: 0xffa6e3a1,
        warn: 0xfff9e2af,
        link: 0xff94e2d5,
        scrim: 0x8c000000,
        veil: 0xcc0b0c12,
    },
    // Catppuccin Latte — the light one. `bg` is the *lightest* surface here, which is what
    // keeps "ink on an accent chip" (`Theme.bg` over `Theme.accent`) legible either way up.
    UiPalette {
        name: "Latte",
        bg: 0xffeff1f5,
        mantle: 0xffe6e9ef,
        surface: 0xffdce0e8,
        surface2: 0xffccd0da,
        border: 0xffbcc0cc,
        text: 0xff4c4f69,
        subtext: 0xff6c6f85,
        faint: 0xff8c8fa1,
        accent: 0xff1e66d5,
        danger: 0xffd20f39,
        ok: 0xff40a02b,
        warn: 0xffdf8e1d,
        link: 0xff179299,
        // A light shell wants a *lighter* shadow and a *thinner* veil: Mocha's 55% black
        // shadow under a white card reads as a smudge, and its 80% backdrop as a blackout.
        scrim: 0x59000000,
        veil: 0x66313244,
    },
    // Nord — cooler and lower-contrast than Mocha; the "easy on the eyes" option.
    UiPalette {
        name: "Nord",
        bg: 0xff2e3440,
        mantle: 0xff272b35,
        surface: 0xff3b4252,
        surface2: 0xff434c5e,
        border: 0xff434c5e,
        text: 0xffeceff4,
        subtext: 0xffaeb7c7,
        faint: 0xff7b8494,
        accent: 0xff88c0d0,
        danger: 0xffbf616a,
        ok: 0xffa3be8c,
        warn: 0xffebcb8b,
        link: 0xff8fbcbb,
        scrim: 0x8c000000,
        veil: 0xcc1c2029,
    },
    // Gruvbox Dark — warm, high-legibility, the long-standing terminal favourite.
    UiPalette {
        name: "Gruvbox",
        bg: 0xff1d2021,
        mantle: 0xff282828,
        surface: 0xff32302f,
        surface2: 0xff3c3836,
        border: 0xff504945,
        text: 0xffebdbb2,
        subtext: 0xffbdae93,
        faint: 0xff928374,
        accent: 0xff83a598,
        danger: 0xfffb4934,
        ok: 0xffb8bb26,
        warn: 0xfffabd2f,
        link: 0xff8ec07c,
        scrim: 0x8c000000,
        veil: 0xcc141617,
    },
    // High Contrast — pure black ground, pure white ink, a border light enough to see. The
    // gap between `bg` and every other surface is far wider than in the other palettes,
    // which is the whole point: the selected tab has to be obvious, not merely present.
    UiPalette {
        name: "High Contrast",
        bg: 0xff000000,
        mantle: 0xff0d0d0d,
        surface: 0xff1a1a1a,
        surface2: 0xff2e2e2e,
        border: 0xff6e6e6e,
        text: 0xffffffff,
        subtext: 0xffd0d0d0,
        faint: 0xff9a9a9a,
        accent: 0xff4cc2ff,
        danger: 0xffff5f5f,
        ok: 0xff4ff07a,
        warn: 0xffffd000,
        link: 0xff6ee7d5,
        scrim: 0xb3000000,
        veil: 0xe6000000,
    },
];

/// Clamp a (possibly stale) shell-palette index to a real palette — a settings file written
/// by a later build, or hand-edited, must not panic the startup path (defaults to Mocha).
pub fn ui_palette(idx: usize) -> UiPalette {
    UI_PALETTES[idx.min(UI_PALETTES.len() - 1)]
}

/// The pane accent for creation index `i` under frame-palette `palette`.
pub fn accent_for(i: usize, palette: usize) -> Color {
    let slots = frame_palette(palette);
    let (r, g, b) = slots[i % slots.len()];
    Color::from_rgb_u8(r, g, b)
}

/// The full set of user-selectable layouts, in menu order. `Auto` leads (the
/// smart default); the four concrete presets follow. `Single` is reachable via
/// this menu too so every preset is selectable per the Wave-1 spec. The explicit
/// grid shapes come last, after the presets, so the ids of the rows that existed
/// before them do not move (see [`layout_id`]).
///
/// A slice rather than an array: the set of offered shapes is open — a 4x3 is one
/// entry away — and a `[Layout; N]` would make every call site carry the count.
pub const LAYOUT_MENU: &[Layout] = &[
    Layout::Auto,
    Layout::Single,
    Layout::Columns,
    Layout::Rows,
    Layout::Grid,
    Layout::MainStack,
    Layout::GridFixed(2, 2),
    Layout::GridFixed(2, 3),
    Layout::GridFixed(3, 2),
    Layout::GridFixed(3, 3),
];

/// Stable order index used to round-trip a `Layout` through the Slint menu (which
/// passes an `int` id). Positional in [`LAYOUT_MENU`], so rows are only ever
/// appended: the id doubles as the drawn-mini selector in `IconLayout`
/// (`ui/contextmenu.slint`, via [`layout_icon_kind`]), and reordering the menu
/// would silently repaint every layout icon. Ids are never persisted — the
/// workspace file stores the token from [`layout_name`] — so growing the menu
/// costs nothing on disk.
pub fn layout_id(l: Layout) -> i32 {
    if let Some(i) = menu_position(l) {
        return i;
    }
    // A grid shape the menu does not offer: `grid-4x3` in a hand-edited workspace file is a
    // valid layout with no row of its own. Borrowing the square grid's id puts the checkmark
    // and the drawn lattice on the nearest thing we have, rather than on Automatic.
    if matches!(l, Layout::GridFixed(..)) {
        return menu_position(Layout::Grid).unwrap_or(0);
    }
    0
}

fn menu_position(l: Layout) -> Option<i32> {
    LAYOUT_MENU.iter().position(|x| *x == l).map(|i| i as i32)
}

/// Resolve a menu id back to a `Layout` (defaults to `Auto` on an out-of-range id).
pub fn layout_from_id(id: i32) -> Layout {
    usize::try_from(id)
        .ok()
        .and_then(|i| LAYOUT_MENU.get(i))
        .copied()
        .unwrap_or(Layout::Auto)
}

/// The serialization token for a layout — borrowed for every preset, owned only for the
/// explicit `grid-CxR` shapes, which have no fixed string to point at.
pub fn layout_name(l: Layout) -> Cow<'static, str> {
    l.token()
}

/// Drawn-icon kinds for the application (hamburger) menu's action rows. Icons used to be
/// font glyph strings (Segoe MDL2 Assets PUA codepoints, before that emoji, before that
/// the geometric ▤▥▦… chars) but Slint on Windows renders a hollow box whenever the
/// resolved font lacks the codepoint — the recurring failure df73005 / the wave3x bell
/// fixed by drawing geometry. Each kind selects a vector-drawn icon in the `MenuIcon`
/// component (`ui/contextmenu.slint`); keep the two lists in lock-step.
pub mod menu_icon {
    // (0 = no leading icon — rows pass a literal 0 rather than a const.)
    /// New pane — a drawn "+".
    pub const NEW_PANE: i32 = 1;
    /// Command palette — a drawn ">_" prompt.
    pub const COMMAND_PALETTE: i32 = 2;
    /// Open workspace — a drawn folder.
    pub const OPEN_WORKSPACE: i32 = 3;
    /// Save workspace — a drawn floppy disk.
    pub const SAVE_WORKSPACE: i32 = 4;
    /// Preferences — drawn slider bars.
    pub const PREFERENCES: i32 = 5;
    /// Left panel — a drawn window frame with its leading column filled.
    pub const LEFT_PANEL: i32 = 6;
    /// Restart — a drawn circular arrow.
    pub const RESTART: i32 = 7;
    /// Base for the layout minis: `LAYOUT_BASE + layout_id(l)` (see [`super::layout_icon_kind`]).
    pub const LAYOUT_BASE: i32 = 10;
    /// Base for the per-tool marks. Kinds from here up are allocated by the core registry
    /// (`hyperpanes_core::tools::registry::TOOL_ICON_BASE`, which MUST equal this) and drawn
    /// by `ToolIcon` in `ui/contextmenu.slint`. The two constants live in different crates
    /// because the registry is data in core while the drawing is app-side; the assertion in
    /// [`super::tests::tool_icons_start_where_the_registry_says`] is what keeps them equal.
    pub const TOOL_BASE: i32 = 40;
}

/// The drawn-icon kind evoking a layout (`menu_icon::LAYOUT_BASE + layout_id`), rendered
/// as an outlined mini-frame with dividers by `IconLayout` in `ui/contextmenu.slint` —
/// the vector replacement for the Electron `presets.ts` ⊞ □ ▥ ▤ ▦ ▧ chars, which the
/// default UI font lacks. Used by the application + tab Layout submenus.
pub fn layout_icon_kind(l: Layout) -> i32 {
    menu_icon::LAYOUT_BASE + layout_id(l)
}

/// The human display label for each layout, matching Electron's `LAYOUTS[].label` /
/// `AUTO_LAYOUT.label` (Title Case). Used in the menus; the HUD/serialization keep the
/// lowercase token from [`layout_name`].
pub fn layout_label(l: Layout) -> Cow<'static, str> {
    match l {
        Layout::Auto => Cow::Borrowed("Automatic"),
        Layout::Single => Cow::Borrowed("Single"),
        Layout::Columns => Cow::Borrowed("Columns"),
        Layout::Rows => Cow::Borrowed("Rows"),
        Layout::Grid => Cow::Borrowed("Grid"),
        Layout::MainStack => Cow::Borrowed("Main + Stack"),
        // Spaced around the ×: "2 x 2" reads as a shape at menu size, where "2x2" reads as a
        // token. The plain `x` (not `×`) is deliberate — the menu font is resolved per
        // platform and a missing glyph draws a hollow box (see `menu_icon`).
        Layout::GridFixed(cols, rows) => Cow::Owned(format!("{cols} x {rows}")),
    }
}

/// Load a monospace font at the given UI scale (best-available Cascadia/Consolas on
/// Windows; the platform default everywhere else).
pub fn load_font(scale: f32) -> Font {
    let candidates = [
        "C:/Windows/Fonts/CascadiaMono.ttf",
        "C:/Windows/Fonts/CascadiaCode.ttf",
        "C:/Windows/Fonts/consola.ttf",
    ];
    let path = candidates
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|p| p.to_string())
        // Non-Windows (or a stripped Windows): the per-platform default resolution.
        .unwrap_or_else(|| crate::prefs::resolve_or_default(""));
    load_font_at(&path, 14.0, scale)
}

/// Load a monospace font from `path` at `base_px` logical points, scaled for DPI.
/// The Wave-2 preferences feature uses this to re-load the terminal font when the user
/// changes family/size (the new font flows through `relayout`'s cell-metric reflow).
///
/// A missing/unloadable `path` falls back to the platform default resolution, then the
/// bundled OFL fonts (extracted at startup) — so font loading never panics over an
/// uninstalled font on any OS.
pub fn load_font_at(path: &str, base_px: f32, scale: f32) -> Font {
    let px = (base_px * scale).round().max(8.0);
    if let Ok(f) = Font::from_path(path, px) {
        return f;
    }
    let mut candidates = vec![crate::prefs::resolve_or_default("")];
    let bundled = crate::prefs::bundled_font_dir();
    for (name, _) in crate::prefs::BUNDLED_FONTS {
        candidates.push(bundled.join(name).to_string_lossy().replace('\\', "/"));
    }
    for c in &candidates {
        if let Ok(f) = Font::from_path(c, px) {
            return f;
        }
    }
    // The bundled fonts are written at startup (`prefs::init_bundled_fonts`); reaching
    // here means even those are gone — nothing sensible left to draw with.
    panic!("no loadable monospace font: tried {path:?}, then {candidates:?}");
}

#[cfg(test)]
mod tests {
    #[test]
    fn tool_icons_start_where_the_registry_says() {
        // Two crates, one allocation. Drift here silently draws the wrong mark.
        assert_eq!(
            super::menu_icon::TOOL_BASE as u32,
            hyperpanes_core::tools::registry::TOOL_ICON_BASE
        );
    }

    /// Every menu row must survive the trip out through Slint's `int` and back, or picking a
    /// layout would set a different one.
    #[test]
    fn every_menu_layout_round_trips_through_its_id() {
        for l in super::LAYOUT_MENU {
            assert_eq!(super::layout_from_id(super::layout_id(*l)), *l);
        }
    }

    /// The ids of the rows that predate the explicit grids must not move: the same number
    /// selects the drawn mini in `ui/contextmenu.slint`, which is matched by literal.
    #[test]
    fn the_original_menu_ids_are_unchanged() {
        use hyperpanes_core::layout::presets::Layout;
        for (i, l) in [
            Layout::Auto,
            Layout::Single,
            Layout::Columns,
            Layout::Rows,
            Layout::Grid,
            Layout::MainStack,
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(super::layout_id(*l), i as i32);
        }
    }

    /// Every id the menu hands out must stay inside the block reserved for layout minis
    /// (`LAYOUT_BASE`..`TOOL_BASE`), or a layout row would draw a tool's mark.
    #[test]
    fn layout_icon_kinds_stay_inside_their_reserved_block() {
        for l in super::LAYOUT_MENU {
            let kind = super::layout_icon_kind(*l);
            assert!(kind >= super::menu_icon::LAYOUT_BASE && kind < super::menu_icon::TOOL_BASE);
        }
    }

    /// A grid shape no menu row offers (a hand-edited workspace file may name any of them)
    /// still reads as a grid rather than falling all the way back to Automatic.
    #[test]
    fn an_off_menu_grid_shape_borrows_the_square_grids_row() {
        use hyperpanes_core::layout::presets::Layout;
        assert_eq!(
            super::layout_id(Layout::GridFixed(4, 3)),
            super::layout_id(Layout::Grid)
        );
    }

    /// The menu label and the on-disk token are different strings for the same layout; the
    /// token is what a workspace file must be able to read back.
    #[test]
    fn fixed_grids_label_and_serialize_distinctly() {
        use hyperpanes_core::layout::presets::Layout;
        assert_eq!(super::layout_label(Layout::GridFixed(2, 3)), "2 x 3");
        assert_eq!(super::layout_name(Layout::GridFixed(2, 3)), "grid-2x3");
        assert_eq!(
            Layout::from_token(&super::layout_name(Layout::GridFixed(2, 3))),
            Some(Layout::GridFixed(2, 3))
        );
    }

    /// A stale or hand-edited `uiPalette` index must clamp, not panic: settings are loaded
    /// before the window exists, so an out-of-range value here would take the app down on
    /// startup with no UI to report it.
    #[test]
    fn an_out_of_range_shell_palette_clamps_to_the_default() {
        assert_eq!(super::ui_palette(0).name, "Mocha");
        assert_eq!(super::ui_palette(999).name, super::UI_PALETTES[4].name);
    }

    /// Every shell token except the two translucent ones must be fully opaque. A palette
    /// that leaves alpha at 0 (the easy typo when writing `0x11111b` instead of
    /// `0xff11111b`) renders as an invisible top bar — a failure that only shows up by
    /// running the app, so it is worth a test that shows up by running the suite.
    #[test]
    fn shell_palettes_are_opaque_except_the_shadow_and_the_backdrop() {
        for p in super::UI_PALETTES {
            for (token, v) in [
                ("bg", p.bg),
                ("mantle", p.mantle),
                ("surface", p.surface),
                ("surface2", p.surface2),
                ("border", p.border),
                ("text", p.text),
                ("subtext", p.subtext),
                ("faint", p.faint),
                ("accent", p.accent),
                ("danger", p.danger),
                ("ok", p.ok),
                ("warn", p.warn),
                ("link", p.link),
            ] {
                assert_eq!(v >> 24, 0xff, "{} {} is not opaque", p.name, token);
            }
            // The other two are translucent by definition — a fully opaque one would paint
            // over the window instead of washing it.
            assert!(p.scrim >> 24 < 0xff, "{} scrim is opaque", p.name);
            assert!(p.veil >> 24 < 0xff, "{} veil is opaque", p.name);
        }
    }

    /// Index 0 is what a settings file with no `uiPalette` field falls back to, and what
    /// `ui/theme.slint` still carries as its property defaults. If the two ever drift, a
    /// build that failed to push a palette would look subtly wrong rather than identical.
    #[test]
    fn the_default_shell_palette_matches_the_slint_defaults() {
        let p = super::ui_palette(0);
        let ui = include_str!("../ui/theme.slint");
        for (prop, v) in [
            ("bg", p.bg),
            ("mantle", p.mantle),
            ("surface", p.surface),
            ("surface2", p.surface2),
            ("border", p.border),
            ("text", p.text),
            ("subtext", p.subtext),
            ("faint", p.faint),
            ("accent", p.accent),
            ("danger", p.danger),
            ("ok", p.ok),
            ("warn", p.warn),
            ("link", p.link),
        ] {
            let needle = format!("> {}: #{:06x};", prop, v & 0x00ff_ffff);
            assert!(
                ui.contains(&needle),
                "ui/theme.slint has no `{}` — Mocha and the Slint defaults have drifted",
                needle
            );
        }
    }
}
