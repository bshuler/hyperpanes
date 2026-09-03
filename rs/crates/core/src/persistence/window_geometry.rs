//! The main window's remembered frame — `window-geometry.json` in the state dir. Atomic
//! write, same `load()/load_from()/save()/save_to()` shape as the other persisted files.
//!
//! The app used to have no geometry of its own and leaned on macOS's own frame
//! restoration. That is why startup visibly passed through TWO sizes: Slint created the
//! window at the `.slint` `preferred-width`/`preferred-height` (1280x800), and only then
//! did the OS shove it back to the frame the human actually left it at. Windows and Linux
//! got no restoration at all — every launch reopened at 1280x800 in the middle of the
//! screen. Remembering the frame ourselves and applying it BEFORE the native window is
//! created removes both the flicker and the platform asymmetry (and, incidentally, most of
//! the startup pty-resize churn that `paneview::PTY_RESIZE_SETTLE` exists to absorb).
//!
//! Units are LOGICAL (device-independent) pixels, the coordinate space Slint's
//! `Window::set_position`/`set_size` speak, so a frame saved on a HiDPI display comes back
//! the same apparent size on a 1x one. `x`/`y` are the OUTER top-left (what
//! `Window::position()` reports); `width`/`height` are the INNER size (what
//! `Window::size()` reports) — the same asymmetric pair the Slint API uses, kept as-is so
//! save and restore are exact inverses of each other.
//!
//! Every field is optional and omitted when unset: a first-ever launch writes nothing, and
//! a file that lost a key still loads. A geometry with no size is not usable and is
//! reported as such by [`WindowGeometry::size`] rather than being half-applied.

use crate::persistence::paths;
use serde::{Deserialize, Serialize};

/// A remembered window frame in logical pixels, plus the maximized flag.
///
/// `Default` is "nothing remembered" — every field `None`/`false`, which the caller reads
/// as "use the `.slint` preferred size and let the OS place the window".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WindowGeometry {
    /// Outer left edge, logical px. Negative is legal and common (a monitor left of the
    /// primary one), which is why this is `i64` and not unsigned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i64>,
    /// Outer top edge, logical px.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i64>,
    /// Inner (client-area) width, logical px.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    /// Inner (client-area) height, logical px.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    /// The window was maximized when it was last saved. The x/y/width/height above are then
    /// the last *restored* (un-maximized) frame, so un-maximizing after a relaunch lands
    /// where the human left it rather than at some default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub maximized: bool,
}

/// A display's usable area in the same logical coordinate space as [`WindowGeometry`].
///
/// Only what the clamp needs. Platform enumeration lives in the app crate (`window/`),
/// because it is the one place that already carries per-OS window code; this crate just
/// does the arithmetic, so it stays testable without a windowing system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayRect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

/// How much of the window's title-bar row must land on a display for the frame to count as
/// reachable. A window whose only on-screen sliver is a corner is not draggable back — the
/// grab handle is the top edge, so that is what has to be visible.
const MIN_VISIBLE: i64 = 80;

/// The smallest frame we will ever restore. Matches the `.slint` `min-width`/`min-height`
/// so a clamped-down window cannot be handed a size the layout refuses anyway.
const MIN_W: i64 = 480;
const MIN_H: i64 = 320;

impl WindowGeometry {
    /// The remembered inner size, or `None` when the file never recorded one. Both halves
    /// are required: a width with no height cannot be applied, and applying half of it
    /// would produce a window shape nobody ever chose.
    #[tracing::instrument(level = "debug", ret)]
    pub fn size(&self) -> Option<(i64, i64)> {
        Some((self.width?, self.height?))
    }

    /// The remembered outer position, or `None` when the file never recorded one.
    #[tracing::instrument(level = "debug", ret)]
    pub fn position(&self) -> Option<(i64, i64)> {
        Some((self.x?, self.y?))
    }

    /// Pull the frame back onto the attached displays.
    ///
    /// A frame is remembered against the monitor layout of the moment it was saved. Unplug
    /// the external display it lived on (or dock/undock, or change the arrangement) and the
    /// same numbers now describe empty space: the window opens somewhere no compositor will
    /// ever show it, and the human sees a launch that "did nothing". So:
    ///
    /// * the size is capped to the target display and floored at the layout's own minimum,
    ///   because a frame remembered on a 4K monitor does not fit a laptop panel;
    /// * the position is nudged until at least [`MIN_VISIBLE`] px of the window's TOP edge
    ///   overlaps a display, so it is always grabbable;
    /// * an EMPTY display list means "we could not enumerate" (a Wayland session, an
    ///   unexpected platform) — the frame is then returned untouched, which degrades to the
    ///   old behaviour instead of second-guessing with no data.
    ///
    /// The chosen display is the one the frame overlaps most; with no overlap at all it is
    /// the first (primary) one.
    #[tracing::instrument(level = "debug", ret)]
    pub fn clamp_to_displays(&self, displays: &[DisplayRect]) -> WindowGeometry {
        let (Some((w, h)), Some((x, y))) = (self.size(), self.position()) else {
            // Nothing to clamp — a size-only or empty geometry is applied (or not) as-is.
            return *self;
        };
        if displays.is_empty() {
            return *self;
        }
        let best = displays
            .iter()
            .max_by_key(|d| overlap_area(x, y, w, h, d))
            .copied()
            .unwrap_or(displays[0]);
        let overlaps_any = displays.iter().any(|d| top_edge_visible(x, y, w, d));

        let w = w.clamp(MIN_W.min(best.width), best.width);
        let h = h.clamp(MIN_H.min(best.height), best.height);
        let (x, y) = if overlaps_any {
            (x, y)
        } else {
            // Re-seat on the best display: centre horizontally, top-align with a small inset
            // so the title bar is never under a menu bar / panel at the very top pixel row.
            (
                best.x + (best.width - w).max(0) / 2,
                best.y + ((best.height - h).max(0) / 4),
            )
        };
        // Even a frame that started on-screen must not hang so far right/down that the whole
        // title row leaves the display it was matched to.
        let x = x.clamp(
            best.x - (w - MIN_VISIBLE).max(0),
            best.x + best.width - MIN_VISIBLE,
        );
        let y = y.clamp(best.y, best.y + best.height - MIN_VISIBLE);
        WindowGeometry {
            x: Some(x),
            y: Some(y),
            width: Some(w),
            height: Some(h),
            maximized: self.maximized,
        }
    }
}

/// Area of the intersection between a frame and a display, `0` when they do not touch.
#[tracing::instrument(level = "debug", ret)]
fn overlap_area(x: i64, y: i64, w: i64, h: i64, d: &DisplayRect) -> i64 {
    let ox = (x + w).min(d.x + d.width) - x.max(d.x);
    let oy = (y + h).min(d.y + d.height) - y.max(d.y);
    ox.max(0) * oy.max(0)
}

/// Does enough of the frame's TOP edge (the drag handle) land on this display?
#[tracing::instrument(level = "debug", ret)]
fn top_edge_visible(x: i64, y: i64, w: i64, d: &DisplayRect) -> bool {
    let ox = (x + w).min(d.x + d.width) - x.max(d.x);
    ox >= MIN_VISIBLE && y >= d.y && y < d.y + d.height
}

/// Read the remembered frame from the canonical `window-geometry.json`.
#[tracing::instrument(level = "debug", ret)]
pub fn load() -> WindowGeometry {
    load_from(&paths::window_geometry_json())
}

/// Read the remembered frame from `path`, returning "nothing remembered" on any error.
///
/// Forgiving on purpose, and read through a generic `Value` for the same reason the other
/// persisted files are: this runs on the startup path before a window exists, so a corrupt
/// or hand-edited file must cost a default-sized window, never a failed launch. A field
/// that is not a whole number (a `null`, a string, a float from some other writer) is
/// treated as absent rather than poisoning the whole frame.
#[tracing::instrument(level = "debug", ret)]
pub fn load_from(path: &std::path::Path) -> WindowGeometry {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return WindowGeometry::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return WindowGeometry::default();
    };
    let int = |key: &str| value.get(key).and_then(|v| v.as_i64());
    WindowGeometry {
        x: int("x"),
        y: int("y"),
        // A zero-or-negative extent is not a window; drop it so the caller falls back to the
        // preferred size instead of asking the compositor for an impossible surface.
        width: int("width").filter(|&w| w > 0),
        height: int("height").filter(|&h| h > 0),
        maximized: value.get("maximized").and_then(|v| v.as_bool()) == Some(true),
    }
}

/// Persist the frame to the canonical `window-geometry.json` (atomic).
#[tracing::instrument(level = "debug", ret)]
pub fn save(geometry: &WindowGeometry) -> std::io::Result<()> {
    save_to(&paths::window_geometry_json(), geometry)
}

/// Persist the frame to `path`, atomically, 2-space pretty-printed like every other file
/// under the userData dir. Unset fields are omitted rather than written as `null`.
#[tracing::instrument(level = "debug", ret)]
pub fn save_to(path: &std::path::Path, geometry: &WindowGeometry) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(geometry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    paths::write_atomic(path, json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hp-window-geometry-{}-{tag}.json",
            std::process::id()
        ))
    }

    fn frame(x: i64, y: i64, w: i64, h: i64) -> WindowGeometry {
        WindowGeometry {
            x: Some(x),
            y: Some(y),
            width: Some(w),
            height: Some(h),
            maximized: false,
        }
    }

    const LAPTOP: DisplayRect = DisplayRect {
        x: 0,
        y: 0,
        width: 1512,
        height: 982,
    };
    /// A 4K monitor sitting to the LEFT of the laptop — negative origin, the case an
    /// unsigned coordinate type would have silently mangled.
    const EXTERNAL: DisplayRect = DisplayRect {
        x: -3840,
        y: -400,
        width: 3840,
        height: 2160,
    };

    #[test]
    fn missing_file_yields_nothing_remembered() {
        let p = temp_path("missing");
        let _ = std::fs::remove_file(&p);
        assert_eq!(load_from(&p), WindowGeometry::default());
    }

    #[test]
    fn corrupt_file_yields_nothing_remembered() {
        let p = temp_path("corrupt");
        std::fs::write(&p, b"{ not json").unwrap();
        assert_eq!(load_from(&p), WindowGeometry::default());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn save_then_load_round_trips() {
        let p = temp_path("roundtrip");
        let g = WindowGeometry {
            maximized: true,
            ..frame(-1200, 40, 1440, 900)
        };
        save_to(&p, &g).unwrap();
        assert_eq!(load_from(&p), g);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn saved_shape_is_camel_case_pretty_and_omits_unset() {
        let p = temp_path("shape");
        save_to(&p, &frame(10, 20, 1280, 800)).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("\n  \"x\": 10"), "2-space pretty: {raw}");
        assert!(raw.contains("\"width\": 1280"), "{raw}");
        // `maximized: false` and the absent fields are not written at all.
        assert!(!raw.contains("maximized"), "{raw}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn zero_or_negative_extents_load_as_unset() {
        let p = temp_path("degenerate");
        std::fs::write(&p, br#"{ "x": 5, "y": 5, "width": 0, "height": -20 }"#).unwrap();
        let g = load_from(&p);
        assert_eq!(g.size(), None);
        assert_eq!(g.position(), Some((5, 5)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_frame_already_on_a_display_is_left_alone() {
        let g = frame(100, 60, 1280, 800);
        assert_eq!(g.clamp_to_displays(&[LAPTOP]), g);
    }

    #[test]
    fn an_empty_display_list_never_clamps() {
        // "We could not enumerate" must degrade to the old behaviour, not to a moved window.
        let g = frame(-9000, -9000, 1280, 800);
        assert_eq!(g.clamp_to_displays(&[]), g);
    }

    #[test]
    fn a_frame_on_an_unplugged_monitor_lands_on_a_real_one() {
        // Saved on EXTERNAL, relaunched with only the laptop attached.
        let g = frame(-3000, -200, 2400, 1500);
        let c = g.clamp_to_displays(&[LAPTOP]);
        let (x, y) = c.position().unwrap();
        let (w, h) = c.size().unwrap();
        assert!(
            w <= LAPTOP.width && h <= LAPTOP.height,
            "shrunk to fit: {c:?}"
        );
        assert!(x >= LAPTOP.x && x + w <= LAPTOP.x + LAPTOP.width, "{c:?}");
        assert!((LAPTOP.y..LAPTOP.y + LAPTOP.height).contains(&y), "{c:?}");
    }

    #[test]
    fn a_frame_on_a_still_attached_second_monitor_is_kept() {
        let g = frame(-3000, -200, 2400, 1500);
        assert_eq!(g.clamp_to_displays(&[LAPTOP, EXTERNAL]), g);
    }

    #[test]
    fn a_frame_dragged_mostly_off_the_right_edge_keeps_a_grabbable_strip() {
        let g = frame(1500, 500, 1280, 800);
        let c = g.clamp_to_displays(&[LAPTOP]);
        let (x, _) = c.position().unwrap();
        assert!(x + MIN_VISIBLE <= LAPTOP.x + LAPTOP.width, "{c:?}");
    }

    #[test]
    fn a_frame_dragged_above_the_top_edge_comes_back_down() {
        // Only the title bar can be dragged, so a negative Y is unrecoverable by hand.
        let g = frame(200, -600, 1280, 800);
        let c = g.clamp_to_displays(&[LAPTOP]);
        let (x, y) = c.position().unwrap();
        assert!((LAPTOP.y..LAPTOP.y + LAPTOP.height).contains(&y), "{c:?}");
        assert!(x >= LAPTOP.x, "{c:?}");
    }

    #[test]
    fn clamping_preserves_the_maximized_flag() {
        let g = WindowGeometry {
            maximized: true,
            ..frame(-9000, -9000, 1280, 800)
        };
        assert!(g.clamp_to_displays(&[LAPTOP]).maximized);
    }

    #[test]
    fn a_geometry_with_no_size_is_returned_untouched() {
        let g = WindowGeometry {
            x: Some(10),
            y: Some(10),
            ..Default::default()
        };
        assert_eq!(g.clamp_to_displays(&[LAPTOP]), g);
    }
}
