//! Port of `src/renderer/layout/presets.ts` — the 5 layout presets (single / columns / rows /
//! grid / main-stack), the explicit `grid-CxR` shapes the port adds on top of them, and
//! auto-resolution (1→single, 2-3→columns, 4+→grid). Computes tile rects
//! as fractions 0..1 from (preset, pane count, sizes, mainFraction):
//! `compute_tiles(...) -> Vec<Tile>` (each carries a `Rect { x, y, w, h }`). Mirror `presets.test.ts`.

use std::borrow::Cow;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::sizes::{clamp_fraction, equal_sizes, normalize};

/// `'auto'` tiles by pane count (see [`effective_layout`]); the rest are concrete presets.
/// Serializes to the same kebab strings as the TS `Layout` union (`"main-stack"` etc.),
/// which the explicit shapes extend with `"grid-2x3"` rather than departing from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Auto,
    Single,
    Columns,
    Rows,
    Grid,
    MainStack,
    /// An explicit `cols x rows` grid — the shapes the layout menu offers by name (2x2, 2x3,
    /// 3x2, 3x3). `Grid` derives its shape from the pane count and reflows as panes come and
    /// go; this one keeps the column count the user picked, so a pane always lands in the
    /// same cell. Carried as data rather than one fieldless variant per shape so adding a
    /// 4x3 is a menu-table edit, not a change to the enum and every match over it.
    GridFixed(u8, u8),
}

impl Layout {
    /// The kebab token this layout serializes to: the strings of the TS `Layout` union, plus
    /// `grid-<cols>x<rows>` for an explicit shape. Borrowed for every preset, so only the
    /// fixed grids allocate.
    pub fn token(self) -> Cow<'static, str> {
        match self {
            Layout::Auto => Cow::Borrowed("auto"),
            Layout::Single => Cow::Borrowed("single"),
            Layout::Columns => Cow::Borrowed("columns"),
            Layout::Rows => Cow::Borrowed("rows"),
            Layout::Grid => Cow::Borrowed("grid"),
            Layout::MainStack => Cow::Borrowed("main-stack"),
            Layout::GridFixed(cols, rows) => Cow::Owned(format!("grid-{cols}x{rows}")),
        }
    }

    /// Parse a token back to a layout. `None` for anything unrecognised — including a
    /// `grid-CxR` with a zero or non-numeric dimension — so each caller picks its own
    /// fallback rather than inheriting one from here.
    pub fn from_token(s: &str) -> Option<Layout> {
        match s {
            "auto" => Some(Layout::Auto),
            "single" => Some(Layout::Single),
            "columns" => Some(Layout::Columns),
            "rows" => Some(Layout::Rows),
            "grid" => Some(Layout::Grid),
            "main-stack" => Some(Layout::MainStack),
            _ => {
                let (cols, rows) = s.strip_prefix("grid-")?.split_once('x')?;
                let cols: u8 = cols.parse().ok()?;
                let rows: u8 = rows.parse().ok()?;
                (cols > 0 && rows > 0).then_some(Layout::GridFixed(cols, rows))
            }
        }
    }
}

// A hand-written string serde rather than a derive: the derive would render `GridFixed` as
// the externally-tagged `{"gridFixed":[2,2]}`, which is neither the TS union's shape nor
// what the workspace format stores. Writing it by hand keeps every layout — old and new — a
// single flat string, so a `grid-2x2` costs no more compatibility than a `grid` did.
impl Serialize for Layout {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.token())
    }
}

// An unrecognised token decodes as `Auto` instead of failing the whole document. A build
// that predates a shape must still open a workspace saved by a newer one, and losing a tab's
// preferred layout is a far smaller loss than losing the tab; this matches the app's
// `GroupSpec.layout` path, which is an `Option<String>` parsed with the same fallback.
impl<'de> Deserialize<'de> for Layout {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Layout::from_token(&s).unwrap_or(Layout::Auto))
    }
}

/// The columns→grid boundary for auto: 2..=AUTO_COLUMNS_MAX panes tile as columns,
/// more tile as a grid. A single tunable knob (Q2).
pub const AUTO_COLUMNS_MAX: usize = 3;

/// Resolve a layout to a concrete preset for a given pane count. `'auto'` maps
/// 1 → single, 2..=AUTO_COLUMNS_MAX → columns, more → grid; `'main-stack'` and
/// `'rows'` are manual-only and never produced here. Concrete layouts pass through
/// unchanged, so compute_tiles/compute_dividers/neighbor_index always see a real
/// preset — never `'auto'`.
pub fn effective_layout(layout: Layout, n: usize) -> Layout {
    if layout != Layout::Auto {
        return layout;
    }
    if n <= 1 {
        return Layout::Single;
    }
    if n <= AUTO_COLUMNS_MAX {
        return Layout::Columns;
    }
    Layout::Grid
}

/// A rectangle in fractions of the container (0..1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tile {
    /// index into the pane order
    pub index: usize,
    pub rect: Rect,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DividerKind {
    Size,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DividerDesc {
    pub id: String,
    pub kind: DividerKind,
    pub orientation: Orientation,
    /// boundary after pane `index` (for kind `Size`); -1 for `Main`
    pub index: i32,
    /// position along the axis, fraction 0..1
    pub at: f64,
}

const FULL: Rect = Rect {
    x: 0.0,
    y: 0.0,
    w: 1.0,
    h: 1.0,
};

/// Row-major tiles for a `cols`-wide grid at least `min_rows` tall, shared by the auto-square
/// [`Layout::Grid`] (which passes `min_rows: 0`, letting the count decide) and the explicit
/// [`Layout::GridFixed`] shapes. Callers resolve `n == 0` before getting here.
///
/// Two edges the fixed shapes reach and the square grid never does. **Over-subscription**: a
/// 2x2 asked to hold six panes grows extra rows rather than dropping the panes that do not
/// fit — a pane without a tile is a live terminal the user can no longer see. **Under-fill**:
/// the requested rows are honoured even when the panes do not reach them, so a 2x3 stays
/// visibly taller-celled than a 2x2 instead of collapsing into one; only the last row that
/// actually holds panes spreads its items across the full width, exactly as the square grid
/// does with its partial row.
fn grid_tiles(n: usize, cols: usize, min_rows: usize) -> Vec<Tile> {
    let cols = cols.max(1);
    let rows = min_rows.max(n.div_ceil(cols));
    let last_row = (n - 1) / cols;
    (0..n)
        .map(|i| {
            let r = i / cols;
            let items_in_row = if r < last_row {
                cols
            } else {
                n - cols * last_row
            };
            let c = i - r * cols;
            Tile {
                index: i,
                rect: Rect {
                    x: c as f64 / items_in_row as f64,
                    y: r as f64 / rows as f64,
                    w: 1.0 / items_in_row as f64,
                    h: 1.0 / rows as f64,
                },
                visible: true,
            }
        })
        .collect()
}

/// Maps (layout, pane count, sizes) to a rectangle per pane. Every pane gets a
/// tile every time (panes stay mounted); `visible: false` just hides it (used by
/// the `single` preset) so terminal sessions and scrollback are never destroyed.
pub fn compute_tiles(
    layout: Layout,
    n: usize,
    sizes: &[f64],
    main_fraction: f64,
    focused_index: i32,
) -> Vec<Tile> {
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![Tile {
            index: 0,
            rect: FULL,
            visible: true,
        }];
    }

    let fallback = equal_sizes(n);
    let norm = normalize(if sizes.len() == n { sizes } else { &fallback });
    let mut tiles: Vec<Tile> = Vec::new();

    match layout {
        Layout::Single => {
            let shown = if focused_index >= 0 && (focused_index as usize) < n {
                focused_index as usize
            } else {
                0
            };
            for i in 0..n {
                tiles.push(Tile {
                    index: i,
                    rect: FULL,
                    visible: i == shown,
                });
            }
            tiles
        }
        Layout::Columns => {
            let mut x = 0.0;
            for (i, &frac) in norm.iter().enumerate() {
                tiles.push(Tile {
                    index: i,
                    rect: Rect {
                        x,
                        y: 0.0,
                        w: frac,
                        h: 1.0,
                    },
                    visible: true,
                });
                x += frac;
            }
            tiles
        }
        Layout::Rows => {
            let mut y = 0.0;
            for (i, &frac) in norm.iter().enumerate() {
                tiles.push(Tile {
                    index: i,
                    rect: Rect {
                        x: 0.0,
                        y,
                        w: 1.0,
                        h: frac,
                    },
                    visible: true,
                });
                y += frac;
            }
            tiles
        }
        // The square-ish grid: the column count follows the pane count, so the shape
        // reflows every time a pane appears or disappears.
        Layout::Grid => grid_tiles(n, (n as f64).sqrt().ceil() as usize, 0),
        Layout::GridFixed(cols, rows) => grid_tiles(n, cols as usize, rows as usize),
        Layout::MainStack => {
            let mf = clamp_fraction(main_fraction);
            tiles.push(Tile {
                index: 0,
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: mf,
                    h: 1.0,
                },
                visible: true,
            });
            let stack_n = n - 1;
            let h = 1.0 / stack_n as f64;
            for i in 1..n {
                tiles.push(Tile {
                    index: i,
                    rect: Rect {
                        x: mf,
                        y: (i - 1) as f64 * h,
                        w: 1.0 - mf,
                        h,
                    },
                    visible: true,
                });
            }
            tiles
        }
        // 'auto' never reaches here (resolved via effective_layout first).
        Layout::Auto => tiles,
    }
}

/// Draggable seams for the current layout. Phase 2 resizes columns, rows, and
/// the main divider of main-stack; every grid (square or explicit) and the stack
/// interior use fixed splits.
pub fn compute_dividers(
    layout: Layout,
    n: usize,
    sizes: &[f64],
    main_fraction: f64,
) -> Vec<DividerDesc> {
    if n < 2 {
        return Vec::new();
    }
    let fallback = equal_sizes(n);
    let norm = normalize(if sizes.len() == n { sizes } else { &fallback });
    let mut out: Vec<DividerDesc> = Vec::new();

    match layout {
        Layout::Columns => {
            let mut x = 0.0;
            for (i, &frac) in norm.iter().enumerate().take(n - 1) {
                x += frac;
                out.push(DividerDesc {
                    id: format!("v-{i}"),
                    kind: DividerKind::Size,
                    orientation: Orientation::Vertical,
                    index: i as i32,
                    at: x,
                });
            }
        }
        Layout::Rows => {
            let mut y = 0.0;
            for (i, &frac) in norm.iter().enumerate().take(n - 1) {
                y += frac;
                out.push(DividerDesc {
                    id: format!("h-{i}"),
                    kind: DividerKind::Size,
                    orientation: Orientation::Horizontal,
                    index: i as i32,
                    at: y,
                });
            }
        }
        Layout::MainStack => {
            out.push(DividerDesc {
                id: "main".to_string(),
                kind: DividerKind::Main,
                orientation: Orientation::Vertical,
                index: -1,
                at: clamp_fraction(main_fraction),
            });
        }
        _ => {}
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.005
    }

    // ---- compute_tiles ----

    #[test]
    fn a_single_pane_fills_the_area_for_any_layout() {
        let t = compute_tiles(Layout::Grid, 1, &[1.0], 0.6, 0);
        assert_eq!(t.len(), 1);
        assert_eq!(
            t[0].rect,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0
            }
        );
        assert!(t[0].visible);
    }

    #[test]
    fn columns_are_full_height_and_widths_sum_to_1() {
        let t = compute_tiles(Layout::Columns, 3, &equal_sizes(3), 0.6, 0);
        assert_eq!(t.len(), 3);
        assert!(t.iter().all(|x| x.visible && x.rect.h == 1.0));
        assert!(close(t.iter().map(|x| x.rect.w).sum(), 1.0));
    }

    #[test]
    fn single_layout_shows_only_the_focused_pane() {
        let t = compute_tiles(Layout::Single, 3, &equal_sizes(3), 0.6, 1);
        assert_eq!(t.iter().filter(|x| x.visible).count(), 1);
        assert!(t[1].visible);
    }

    #[test]
    fn grid_keeps_every_tile_within_bounds() {
        let t = compute_tiles(Layout::Grid, 4, &equal_sizes(4), 0.6, 0);
        assert_eq!(t.len(), 4);
        assert!(t
            .iter()
            .all(|x| x.rect.x >= 0.0 && x.rect.x + x.rect.w <= 1.0001));
    }

    /// The exact fit: 4 panes in a 2x2 land in the four quarters, in reading order.
    #[test]
    fn a_fixed_grid_that_exactly_fits_puts_one_pane_per_cell() {
        let t = compute_tiles(Layout::GridFixed(2, 2), 4, &equal_sizes(4), 0.6, 0);
        assert_eq!(t.len(), 4);
        for tile in &t {
            assert!(close(tile.rect.w, 0.5) && close(tile.rect.h, 0.5));
        }
        let corners: Vec<(f64, f64)> = t.iter().map(|x| (x.rect.x, x.rect.y)).collect();
        for (got, want) in corners.iter().zip([(0.0, 0.0), (0.5, 0.0), (0.0, 0.5), (0.5, 0.5)]) {
            assert!(close(got.0, want.0) && close(got.1, want.1));
        }
    }

    /// Under-fill keeps the requested row count (a 2x3 cell is a third of the height, not a
    /// half), and the partial row spreads across the full width like the square grid's does.
    #[test]
    fn an_under_filled_fixed_grid_keeps_its_row_height_and_spreads_the_partial_row() {
        let t = compute_tiles(Layout::GridFixed(2, 3), 3, &equal_sizes(3), 0.6, 0);
        assert_eq!(t.len(), 3);
        assert!(t.iter().all(|x| close(x.rect.h, 1.0 / 3.0)));
        assert!(close(t[0].rect.w, 0.5) && close(t[1].rect.w, 0.5));
        assert!(close(t[2].rect.w, 1.0) && close(t[2].rect.y, 1.0 / 3.0));
    }

    /// Over-subscription grows rows instead of dropping panes: 7 panes in a 2x2 still get 7
    /// tiles, two per row, all inside the container.
    #[test]
    fn an_over_filled_fixed_grid_adds_rows_rather_than_dropping_panes() {
        let t = compute_tiles(Layout::GridFixed(2, 2), 7, &equal_sizes(7), 0.6, 0);
        assert_eq!(t.len(), 7);
        assert!(t.iter().all(|x| x.visible && close(x.rect.h, 0.25)));
        assert!(t
            .iter()
            .all(|x| x.rect.x >= 0.0 && x.rect.x + x.rect.w <= 1.0001));
        assert!(t
            .iter()
            .all(|x| x.rect.y >= 0.0 && x.rect.y + x.rect.h <= 1.0001));
        // The lone pane on the fourth row takes the whole width.
        assert!(close(t[6].rect.w, 1.0) && close(t[6].rect.y, 0.75));
    }

    /// The fixed shape is what distinguishes the menu entries: at 4 panes a 2x2 and a 2x3
    /// must not tile identically, or picking one over the other would do nothing.
    #[test]
    fn a_2x3_is_not_a_2x2_at_the_same_pane_count() {
        let a = compute_tiles(Layout::GridFixed(2, 2), 4, &equal_sizes(4), 0.6, 0);
        let b = compute_tiles(Layout::GridFixed(2, 3), 4, &equal_sizes(4), 0.6, 0);
        assert_ne!(a[0].rect.h, b[0].rect.h);
    }

    /// A degenerate shape must not divide by zero or panic; it degrades to a single column.
    #[test]
    fn a_zero_column_fixed_grid_degrades_to_one_column() {
        let t = compute_tiles(Layout::GridFixed(0, 0), 3, &equal_sizes(3), 0.6, 0);
        assert_eq!(t.len(), 3);
        assert!(t.iter().all(|x| close(x.rect.w, 1.0)));
    }

    #[test]
    fn main_stack_gives_pane_0_the_main_width_and_stacks_the_rest() {
        let t = compute_tiles(Layout::MainStack, 3, &equal_sizes(3), 0.6, 0);
        assert!(close(t[0].rect.w, 0.6));
        assert!(close(t[1].rect.x, 0.6));
        assert!(close(t[2].rect.x, 0.6));
    }

    // ---- effective_layout ----

    #[test]
    fn maps_1_pane_to_single() {
        assert_eq!(effective_layout(Layout::Auto, 1), Layout::Single);
    }

    #[test]
    fn maps_2_and_3_panes_to_columns() {
        assert_eq!(effective_layout(Layout::Auto, 2), Layout::Columns);
        assert_eq!(effective_layout(Layout::Auto, 3), Layout::Columns);
    }

    #[test]
    fn maps_4_plus_panes_to_grid() {
        assert_eq!(effective_layout(Layout::Auto, 4), Layout::Grid);
        assert_eq!(effective_layout(Layout::Auto, 9), Layout::Grid);
        assert_eq!(effective_layout(Layout::Auto, 25), Layout::Grid);
    }

    #[test]
    fn treats_an_empty_group_as_single() {
        assert_eq!(effective_layout(Layout::Auto, 0), Layout::Single);
    }

    #[test]
    fn never_auto_selects_rows_or_main_stack_at_any_count() {
        for n in 0..=30 {
            let eff = effective_layout(Layout::Auto, n);
            assert_ne!(eff, Layout::Rows);
            assert_ne!(eff, Layout::MainStack);
            assert_ne!(eff, Layout::Auto);
        }
    }

    #[test]
    fn passes_concrete_layouts_through_unchanged_regardless_of_count() {
        assert_eq!(effective_layout(Layout::Rows, 5), Layout::Rows);
        assert_eq!(effective_layout(Layout::MainStack, 9), Layout::MainStack);
        assert_eq!(effective_layout(Layout::Single, 4), Layout::Single);
        assert_eq!(effective_layout(Layout::Columns, 1), Layout::Columns);
        assert_eq!(effective_layout(Layout::Grid, 2), Layout::Grid);
        assert_eq!(
            effective_layout(Layout::GridFixed(2, 3), 2),
            Layout::GridFixed(2, 3)
        );
    }

    // ---- compute_dividers ----

    #[test]
    fn columns_produce_n_minus_1_vertical_dividers() {
        assert_eq!(
            compute_dividers(Layout::Columns, 3, &equal_sizes(3), 0.6).len(),
            2
        );
    }

    #[test]
    fn grid_has_no_draggable_dividers() {
        assert_eq!(
            compute_dividers(Layout::Grid, 4, &equal_sizes(4), 0.6).len(),
            0
        );
    }

    /// Fixed grids inherit the square grid's fixed splits — no divider dragging.
    #[test]
    fn a_fixed_grid_has_no_draggable_dividers() {
        assert_eq!(
            compute_dividers(Layout::GridFixed(2, 3), 6, &equal_sizes(6), 0.6).len(),
            0
        );
    }

    #[test]
    fn main_stack_has_a_single_main_divider() {
        let d = compute_dividers(Layout::MainStack, 3, &equal_sizes(3), 0.6);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, DividerKind::Main);
    }

    // serde parity: the kebab strings match the TS `Layout` union.
    #[test]
    fn layout_serializes_to_ts_strings() {
        assert_eq!(
            serde_json::to_string(&Layout::MainStack).unwrap(),
            "\"main-stack\""
        );
        assert_eq!(serde_json::to_string(&Layout::Auto).unwrap(), "\"auto\"");
        // The explicit shapes join the union as flat strings, not as a tagged object.
        assert_eq!(
            serde_json::to_string(&Layout::GridFixed(2, 3)).unwrap(),
            "\"grid-2x3\""
        );
    }

    #[test]
    fn every_layout_survives_a_string_round_trip() {
        for l in [
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
            Layout::GridFixed(5, 4),
        ] {
            assert_eq!(Layout::from_token(&l.token()), Some(l));
            let json = serde_json::to_string(&l).unwrap();
            assert_eq!(serde_json::from_str::<Layout>(&json).unwrap(), l);
        }
    }

    /// A workspace file written before the explicit shapes existed says `"grid"`, and must
    /// still open as the auto-square grid rather than as anything new.
    #[test]
    fn the_old_grid_token_still_decodes_to_the_square_grid() {
        assert_eq!(Layout::from_token("grid"), Some(Layout::Grid));
        assert_eq!(
            serde_json::from_str::<Layout>("\"grid\"").unwrap(),
            Layout::Grid
        );
    }

    /// A token from a future build (or a typo in a hand-edited file) loses the layout, never
    /// the document: `from_token` reports it, and the deserializer falls back to `Auto`.
    #[test]
    fn an_unknown_token_falls_back_instead_of_failing() {
        for bad in ["grid-0x2", "grid-2x0", "grid-2x", "grid-axb", "hexagons", ""] {
            assert_eq!(Layout::from_token(bad), None, "{bad}");
            assert_eq!(
                serde_json::from_str::<Layout>(&format!("\"{bad}\"")).unwrap(),
                Layout::Auto,
                "{bad}"
            );
        }
    }
}
