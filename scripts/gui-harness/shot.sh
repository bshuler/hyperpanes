#!/usr/bin/env bash
# Runs INSIDE the harness container (scripts/gui-harness/Dockerfile).
#
# Photographs a real Hyperpanes window on a real X server so a rendering change
# can be judged from pixels instead of from an opinion. Companion to
# `gui-test.sh`, which drives input; this one only looks.
#
#   shot.sh [outdir]        default /work/shots
#
# It writes, per palette in `theme::UI_PALETTES`:
#   pNN-window.png   the whole window
#   pNN-tabs.png     just the tab strip, scaled up, so the selected tab can be
#                    compared against its neighbours at reading size
#   pNN-md.png       the markdown pane's own rectangle
# plus `pitch.txt`, the measured distance between inked text rows in the
# markdown pane — the number the wrapped-line rhythm has to be argued from.
set -uo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${HYPERPANES_BIN:-$ROOT/rs/crates/app/target/release/hyperpanes}"
OUT="${1:-/work/shots}"
FIXTURE="${HYPERPANES_FIXTURE:-$ROOT/scripts/gui-harness/fixture.md}"
PALETTES="${HYPERPANES_PALETTES:-0 1}"

[[ -x "$BIN" ]] || { echo "no binary at $BIN — build it first" >&2; exit 1; }
[[ -f "$FIXTURE" ]] || { echo "no fixture at $FIXTURE" >&2; exit 1; }
mkdir -p "$OUT"; rm -f "$OUT"/*.png "$OUT"/pitch.txt

export DISPLAY="${DISPLAY:-:99}"
Xvfb "$DISPLAY" -screen 0 1600x1000x24 -nolisten tcp >/tmp/xvfb.log 2>&1 &
XVFB=$!
for _ in $(seq 1 50); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb never came up; see /tmp/xvfb.log" >&2; exit 1; }
openbox --sm-disable >/tmp/openbox.log 2>&1 &
OPENBOX=$!
for _ in $(seq 1 50); do
    xprop -root _NET_SUPPORTING_WM_CHECK >/dev/null 2>&1 && break
    sleep 0.2
done

STATE="$(mktemp -d /tmp/hp-shot.XXXXXX)"
export HYPERPANES_USER_DATA_DIR="$STATE/data"
export HYPERPANES_CONTROL_FILE="$STATE/control.json"
mkdir -p "$HYPERPANES_USER_DATA_DIR"

cleanup() {
    [[ -n "${APP:-}" ]] && kill "$APP" 2>/dev/null
    pkill -f -- "--session-daemon $HYPERPANES_USER_DATA_DIR" 2>/dev/null
    kill "${OPENBOX:-}" 2>/dev/null
    kill "$XVFB" 2>/dev/null
    rm -rf "$STATE"
}
trap cleanup EXIT

# Three tabs, because "is the selected one obvious" is a question about a strip,
# not about a chip: one tab alone always looks selected. The middle one holds the
# markdown preview (`pane.kind: view:markdown`, whose target IS its cwd), split
# beside a shell so the preview is about as wide as it is in real use — at the
# full window width the fixture's paragraphs barely wrap, and wrapping is the
# thing being photographed.
cat > "$STATE/ws.json" <<JSON
{
  "groups": [
    { "title": "server", "layout": "single", "panes": [ { "label": "server", "command": "cat" } ] },
    { "title": "fixture.md", "layout": "columns",
      "panes": [
        { "label": "fixture.md", "cwd": "$FIXTURE", "meta": { "pane.kind": "view:markdown" } },
        { "label": "shell", "command": "cat" }
      ] },
    { "title": "notes", "layout": "single", "panes": [ { "label": "notes", "command": "cat" } ] }
  ],
  "active": 1
}
JSON

"$BIN" "$STATE/ws.json" >"$STATE/app.log" 2>&1 &
APP=$!
ctl() { "$BIN" ctl "$@"; }
for _ in $(seq 1 100); do
    [[ -s "$HYPERPANES_CONTROL_FILE" ]] && ctl health >/dev/null 2>&1 && break
    ps -p "$APP" >/dev/null 2>&1 || { echo "app died on launch:"; tail -30 "$STATE/app.log"; exit 1; }
    sleep 0.3
done
ctl health >/dev/null 2>&1 || { echo "control API never answered:"; tail -30 "$STATE/app.log"; exit 1; }
for _ in $(seq 1 60); do
    WIN="$(xdotool search --onlyvisible --class -- . 2>/dev/null | head -1)"
    [[ -n "$WIN" ]] && break
    sleep 0.25
done
[[ -n "$WIN" ]] || { echo "no window appeared" >&2; exit 1; }
xdotool windowactivate --sync "$WIN" 2>/dev/null
# The workspace's own `active` index does not survive the launch — the app opens
# its own default tab and focuses that — so select the markdown tab by name. A
# tab line in `ctl tabs` is "<mark> <id>  <title>  [<layout>]"; the bracket is
# what tells a tab line apart from the pane lines indented under it.
MDTAB="$(ctl tabs 2>/dev/null | awk '/\[/ && /fixture\.md/ { if ($1 == "*") print $2; else print $1; exit }')"
[[ -n "$MDTAB" ]] || { echo "the markdown tab never appeared:"; ctl tabs; exit 1; }
ctl focus-tab "$MDTAB" >/dev/null || { echo "could not focus $MDTAB" >&2; exit 1; }

# The renderer is software Vulkan here; the first frame lands well after the
# window maps, and a screenshot taken too early photographs an empty surface.
sleep 3

for P in $PALETTES; do
    ctl set uiPalette "$P" >/dev/null 2>&1
    sleep 1.5
    N="$(printf 'p%02d' "$P")"
    import -window "$WIN" "$OUT/$N-window.png" 2>/dev/null || xwd -id "$WIN" | convert xwd:- "$OUT/$N-window.png"
    # The tab strip is the top band; 4x nearest-neighbour so the chip's edges are
    # judged as drawn rather than as the viewer's resampling of them.
    convert "$OUT/$N-window.png" -crop 'x44+0+0' +repage -filter point -resize 200% "$OUT/$N-tabs.png"
    # The markdown pane fills the window below the strip and left panel.
    convert "$OUT/$N-window.png" -crop '610x700+10+58' +repage "$OUT/$N-md.png"
done

# Row pitch: collapse the markdown crop to one column of "how much ink is on this
# scanline", then report the distance between the starts of consecutive inked
# runs. A uniform document is a column of one repeated number.
python3 - "$OUT/p00-md.png" "$OUT/pitch.txt" <<'PY'
import subprocess, sys
src, dst = sys.argv[1], sys.argv[2]
txt = subprocess.run(
    ["convert", src, "-colorspace", "Gray", "-scale", "1x!", "-depth", "8", "txt:-"],
    capture_output=True, text=True).stdout
rows = []
for line in txt.splitlines()[1:]:
    try:
        gray = int(line.split("gray(")[1].split(")")[0])
    except (IndexError, ValueError):
        continue
    rows.append(gray)
base = min(rows) if rows else 0
inked = [i for i, g in enumerate(rows) if g - base > 6]
runs, prev = [], -99
for i in inked:
    if i - prev > 1:
        runs.append(i)
    prev = i
with open(dst, "w") as f:
    f.write("inked row starts: %s\n" % runs)
    f.write("pitches: %s\n" % [b - a for a, b in zip(runs, runs[1:])])
PY
cat "$OUT/pitch.txt"
echo "==> shots in $OUT"
ls -1 "$OUT"
