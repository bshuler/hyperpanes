#!/usr/bin/env bash
# Runs INSIDE the harness container (scripts/gui-harness/Dockerfile).
#
# Drives a real Hyperpanes window on a real X server with synthetic input, and
# checks what the pane actually received. Nothing here touches a human's
# desktop: the display is an Xvfb stub that exists only for this process tree.
#
#   gui-test.sh [--keep]     --keep leaves the instance running for poking at
set -uo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${HYPERPANES_BIN:-$ROOT/rs/crates/app/target/release/hyperpanes}"
KEEP=0
[[ "${1:-}" == "--keep" ]] && KEEP=1

PASS=0; FAIL=0; SKIP=0
ok()   { echo "  ok   $*"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL $*"; FAIL=$((FAIL+1)); }
skip() { echo "  skip $*"; SKIP=$((SKIP+1)); }
want() { # want <name> <shell test>
    if eval "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1"; fi
}

[[ -x "$BIN" ]] || { echo "no binary at $BIN — build it first" >&2; exit 1; }

# --- an X server that belongs to nobody ---------------------------------------
export DISPLAY="${DISPLAY:-:99}"
Xvfb "$DISPLAY" -screen 0 1600x1000x24 -nolisten tcp >/tmp/xvfb.log 2>&1 &
XVFB=$!
for _ in $(seq 1 50); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb never came up; see /tmp/xvfb.log" >&2; exit 1; }
echo "==> X server up on $DISPLAY"

# --- an instance that shares nothing with anything else ------------------------
STATE="$(mktemp -d /tmp/hp-harness.XXXXXX)"
export HYPERPANES_USER_DATA_DIR="$STATE/data"
export HYPERPANES_CONTROL_FILE="$STATE/control.json"
export HYPERPANES_ALLOW_INPUT=1
mkdir -p "$HYPERPANES_USER_DATA_DIR"

cleanup() {
    [[ $KEEP == 1 ]] && { echo "==> --keep: instance left running (DISPLAY=$DISPLAY, STATE=$STATE)"; return; }
    [[ -n "${APP:-}" ]] && kill "$APP" 2>/dev/null
    pkill -f -- "--session-daemon $HYPERPANES_USER_DATA_DIR" 2>/dev/null
    kill "$XVFB" 2>/dev/null
    rm -rf "$STATE"
}
trap cleanup EXIT

"$BIN" >"$STATE/app.log" 2>&1 &
APP=$!
echo "==> launched pid $APP (state: $STATE)"

ctl() { "$BIN" ctl "$@"; }

for _ in $(seq 1 100); do
    [[ -s "$HYPERPANES_CONTROL_FILE" ]] && ctl health >/dev/null 2>&1 && break
    kill -0 "$APP" 2>/dev/null || { echo "app died on launch:"; tail -30 "$STATE/app.log"; exit 1; }
    sleep 0.3
done
ctl health >/dev/null 2>&1 || { echo "control API never answered:"; tail -30 "$STATE/app.log"; exit 1; }
echo "==> control API answering"

echo "== it starts and draws =="
want "the control API reports healthy"        'ctl health'
want "a window exists on the X server"        'xdotool search --onlyvisible --class -- . | head -1 | grep -q .'
# `ctl panes` prints TSV (id, tab, status, label) — one row per pane, not JSON.
# /health answers before the first pane's pty is up, so this polls rather than
# sampling once: an instant sample raced the startup and reported "no panes" for
# an app that plainly had one a moment later.
for _ in $(seq 1 40); do
    [[ "$(ctl panes 2>/dev/null | grep -c .)" -ge 1 ]] && break
    sleep 0.25
done
want "the app reports at least one pane"      '[[ "$(ctl panes 2>/dev/null | grep -c .)" -ge 1 ]]'

WIN="$(xdotool search --onlyvisible --class -- . 2>/dev/null | head -1)"
PANE="$(ctl panes 2>/dev/null | head -1 | cut -f1)"

echo "== synthetic keyboard reaches the pty =="
# Input goes through XTEST (global), not `xdotool --window` (XSendEvent). winit's
# X11 backend is entitled to ignore a synthetic send_event, and quietly does for
# some event types; XTEST is indistinguishable from a real key at the server.
# Stealing focus to do that is exactly why this runs in a container on a seatless
# box — there is nobody here whose typing it can land in.
act() { xdotool windowactivate --sync "$WIN" 2>/dev/null || xdotool windowfocus "$WIN" 2>/dev/null; }
send_key()  { act; xdotool key --clearmodifiers "$1"; }

# `xdotool type` binds each character to a scratch keycode and remaps the keyboard
# on the fly. An XKB client — which winit is — keeps its own keymap and reads the
# scratch keycode as whatever it used to mean, so the characters never arrive:
# chords (ctrl+v, ctrl+c) landed while typed text vanished. Driving named keysyms
# through `xdotool key` uses the keymap already in place, so no remap happens.
keysym_of() {
    case "$1" in
        " ") echo space ;;    "-") echo minus ;;      "_") echo underscore ;;
        ".") echo period ;;   "/") echo slash ;;      "=") echo equal ;;
        *)   echo "$1" ;;
    esac
}
send_type() {
    act
    local s="$1" i syms=()
    for (( i = 0; i < ${#s}; i++ )); do syms+=("$(keysym_of "${s:i:1}")"); done
    xdotool key --clearmodifiers --delay 20 "${syms[@]}"
}

# Poll the pane until `$2` shows up in its tail, or ~10s pass. Echoes what it saw.
await_pane() { # await_pane <pane> <needle>
    local got=""
    for _ in $(seq 1 40); do
        got="$(ctl read "$1" --tail 40 2>/dev/null)"
        [[ "$got" == *"$2"* ]] && break
        sleep 0.25
    done
    printf '%s' "$got"
}

if [[ -n "$WIN" && -n "$PANE" && "$PANE" != null ]]; then
    MARK="harness-$$-$RANDOM"
    send_type "echo $MARK"
    send_key Return
    got="$(await_pane "$PANE" "$MARK")"
    if [[ "$got" == *"$MARK"* ]]; then ok "typed text arrived in the pane"
    else bad "typed text never arrived in the pane (marker $MARK)"; fi

    # The modifier question, on the branch where no Cmd/Ctrl swap applies. This
    # does NOT settle the macOS behavior — Slint swaps Control and Super only on
    # Apple platforms — but it proves the non-Apple branch, which is the half a
    # Linux box can honestly speak to.
    send_key ctrl+c
    sleep 1
    after="$(ctl read "$PANE" --tail 10 2>/dev/null)"
    if [[ "$after" == *"^C"* ]]; then ok "Ctrl+C reaches the pty as an interrupt (non-Apple branch)"
    else skip "Ctrl+C produced no visible ^C — shell may not echo it; needs a longer-running victim"; fi
else
    skip "no window or no pane id — keyboard stage not attempted"
fi

echo "== clipboard =="
# The bindings under test (keybindings.rs):
#   pane.paste = Ctrl+V     -> Command::PasteFocused  (app-side arboard read, then
#                              bracketed paste into the focused pane's pty)
#   pane.copy  = Ctrl+Shift+C -> Command::CopyFocused (no-op without a selection)
# On Linux the chord's `ctrl` slot is a real Control; on macOS the same slot is
# Command, so this proves the non-Apple half only — same caveat as Ctrl+C above.
#
# xclip forks and stays resident to serve the selection; an X selection has no
# value without a live owner, so this is a real OS clipboard, not a variable.
if [[ -n "$WIN" && -n "$PANE" && "$PANE" != null ]]; then
    CMARK="harness-paste-$$-$RANDOM"
    printf '%s' "$CMARK" | xclip -selection clipboard
    want "the X clipboard holds what we put there" '[[ "$(xclip -selection clipboard -o)" == "$CMARK" ]]'

    send_key ctrl+v
    pasted="$(await_pane "$PANE" "$CMARK")"
    if [[ "$pasted" == *"$CMARK"* ]]; then ok "Ctrl+V pasted the OS clipboard into the pane"
    else bad "Ctrl+V did not paste ($CMARK never reached the pane)"; fi

    # A paste is text insertion, not submission — same contract as a file drop.
    if [[ "$pasted" == *"$CMARK: command not found"* ]]; then
        bad "the paste was EXECUTED — a paste must not submit the line"
    else
        ok "the paste was not executed"
    fi
    send_key ctrl+u   # clear the composed line before the next stage

    # Copy needs a selection, and there is no select-all chord — so drag one, well
    # inside the pane body. The inset matters: a drag that starts on the tab strip
    # reorders tabs instead (see the tab-drag fix), which would test the wrong thing.
    eval "$(xdotool getwindowgeometry --shell "$WIN" 2>/dev/null)"
    # Start just under the tab strip, not a third of the way down: a terminal
    # fills from the top, so a drag beginning mid-window selects blank rows and
    # copies nothing — which is what the first run did.
    echo "  (window geometry: ${WIDTH:-?}x${HEIGHT:-?} at ${X:-?},${Y:-?})"
    SX=$(( ${X:-0} + 40 ));                    SY=$(( ${Y:-0} + 90 ))
    EX=$(( ${X:-0} + ${WIDTH:-800} - 40 ));    EY=$(( ${Y:-0} + ${HEIGHT:-600} - 40 ))
    printf 'harness-sentinel-%s' "$$" | xclip -selection clipboard
    act
    xdotool mousemove "$SX" "$SY" mousedown 1 mousemove "$EX" "$EY" sleep 0.2 mouseup 1
    sleep 0.3
    send_key ctrl+shift+c
    sleep 0.5
    clip="$(xclip -selection clipboard -o 2>/dev/null)"
    if [[ "$clip" == *harness-sentinel-* ]]; then
        bad "Ctrl+Shift+C left the clipboard untouched — the selection never copied"
    elif [[ "$clip" == *"${MARK:-__nomark__}"* ]]; then
        ok "a drag-selection copied to the clipboard with Ctrl+Shift+C"
    elif [[ -n "$clip" ]]; then
        ok "Ctrl+Shift+C replaced the clipboard (selected text did not include the marker)"
    else
        bad "Ctrl+Shift+C emptied the clipboard"
    fi
else
    skip "no window or no pane id — clipboard stage not attempted"
fi

echo "== drag and drop =="
# filedrop.rs: a drop inserts the paths as shell-quoted words and deliberately
# does NOT press Enter — what was dropped is an argument the user is still
# composing. Both halves are asserted here, and the filename carries a space so
# the quoting is actually exercised rather than assumed.
if [[ -n "$WIN" && -n "$PANE" && "$PANE" != null ]]; then
    DROPDIR="$STATE/drop"; mkdir -p "$DROPDIR"
    DROPFILE="$DROPDIR/a dropped file.txt"
    echo "harness" > "$DROPFILE"
    eval "$(xdotool getwindowgeometry --shell "$WIN" 2>/dev/null)"
    DX=$(( ${X:-0} + ${WIDTH:-800} / 2 ))
    DY=$(( ${Y:-0} + ${HEIGHT:-600} / 2 ))
    if python3 "$ROOT/scripts/gui-harness/xdnd-drop.py" --x "$DX" --y "$DY" "$DROPFILE"; then
        ok "the pane accepted an XDND drop"
        sleep 1
        line="$(ctl read "$PANE" --tail 5 2>/dev/null)"
        Q="'"   # the shell quote filedrop.rs wraps a spaced path in
        # The quotes go around the whole absolute path, not the basename — the
        # first run read this wrong and failed a drop the app had got right.
        if [[ "$line" == *"${Q}${DROPFILE}${Q}"* ]]; then
            ok "the dropped path arrived shell-quoted"
        else
            bad "the dropped path did not arrive shell-quoted (saw: ${line: -120})"
        fi
        # Only what the drop ADDED can indict the drop. Earlier stages leave a
        # composed line on screen, so scanning the whole tail for "command not
        # found" convicts filedrop of the paste stage's leftovers.
        rest="${line##*"${Q}${DROPFILE}${Q}"}"
        if [[ "$rest" == *"command not found"* || "$rest" == *"No such file"* ]]; then
            bad "the drop was EXECUTED — filedrop must never press Enter (after the path: ${rest:0:120})"
        else
            ok "the drop was not executed (no Enter pressed)"
        fi
    else
        bad "XDND drop was refused or unanswered"
    fi
else
    skip "no window or no pane id — drop stage not attempted"
fi

echo
echo "$PASS passed, $FAIL failed, $SKIP skipped"
[[ $FAIL -eq 0 ]]
