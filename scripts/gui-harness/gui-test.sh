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

# Without a window manager nothing maps or focuses a new window, and
# `xdotool windowactivate` has no EWMH to talk to — focus then lands wherever it
# likes and synthetic keys reach a window that is not listening. That was a real
# flake, not a theoretical one: two runs in five lost their keystrokes.
openbox --sm-disable >/tmp/openbox.log 2>&1 &
OPENBOX=$!
for _ in $(seq 1 50); do
    xprop -root _NET_SUPPORTING_WM_CHECK >/dev/null 2>&1 && break
    sleep 0.2
done
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
    kill "${OPENBOX:-}" 2>/dev/null
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
# Same race as the pane check below: /health answers before the window is
# mapped, and sampling once reported "no window" for an app that had one a
# quarter-second later.
for _ in $(seq 1 60); do
    WIN="$(xdotool search --onlyvisible --class -- . 2>/dev/null | head -1)"
    [[ -n "$WIN" ]] && break
    sleep 0.25
done
want "a window exists on the X server"        '[[ -n "$WIN" ]]'
# `ctl panes` prints TSV (id, tab, status, label) — one row per pane, not JSON.
# /health answers before the first pane's pty is up, so this polls rather than
# sampling once: an instant sample raced the startup and reported "no panes" for
# an app that plainly had one a moment later.
for _ in $(seq 1 40); do
    [[ "$(ctl panes 2>/dev/null | grep -c .)" -ge 1 ]] && break
    sleep 0.25
done
want "the app reports at least one pane"      '[[ "$(ctl panes 2>/dev/null | grep -c .)" -ge 1 ]]'

PANE="$(ctl panes 2>/dev/null | head -1 | cut -f1)"

echo "== synthetic keyboard reaches the pty =="
# Input goes through XTEST (global), not `xdotool --window` (XSendEvent). winit's
# X11 backend is entitled to ignore a synthetic send_event, and quietly does for
# some event types; XTEST is indistinguishable from a real key at the server.
# Stealing focus to do that is exactly why this runs in a container on a seatless
# box — there is nobody here whose typing it can land in.
# `windowactivate --sync` returns once the server has acted, which is before the
# app has processed FocusIn — keys sent in that gap are delivered to a window
# that is not yet listening, and vanish. So confirm focus actually landed.
act() {
    xdotool windowactivate --sync "$WIN" 2>/dev/null || xdotool windowfocus "$WIN" 2>/dev/null
    for _ in $(seq 1 20); do
        [[ "$(xdotool getwindowfocus 2>/dev/null)" == "$WIN" ]] && return 0
        sleep 0.1
    done
}
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
        # Shell punctuation. Without these the harness could not type a command
        # with a semicolon or a quote in it, which quietly ruled out every test
        # that needs to set the pane up before looking at it.
        ";") echo semicolon ;;   ":") echo colon ;;        ",") echo comma ;;
        \') echo apostrophe ;;   \") echo quotedbl ;;      "!") echo exclam ;;
        "@") echo at ;;          "#") echo numbersign ;;   "%") echo percent ;;
        "^") echo asciicircum ;; "&") echo ampersand ;;    "*") echo asterisk ;;
        "(") echo parenleft ;;   ")") echo parenright ;;   "+") echo plus ;;
        "?") echo question ;;    "[") echo bracketleft ;;  "]") echo bracketright ;;
        "{") echo braceleft ;;   "}") echo braceright ;;   "|") echo bar ;;
        "<") echo less ;;        ">") echo greater ;;      "~") echo asciitilde ;;
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

# A pane row appears in `ctl panes` before its shell has drawn a prompt, and
# keystrokes typed at a pty nobody is reading yet are simply lost. Waiting for
# the pane's first output is the readiness signal; without it this stage failed
# about one run in three. A retry would have hidden that instead of fixing it.
for _ in $(seq 1 60); do
    [[ -n "$(ctl read "$PANE" --tail 5 2>/dev/null | tr -d '[:space:]')" ]] && break
    sleep 0.25
done

if [[ -n "$WIN" && -n "$PANE" && "$PANE" != null ]]; then
    MARK="harness-$$-$RANDOM"
    send_type "echo $MARK"
    send_key Return
    got="$(await_pane "$PANE" "$MARK")"
    if [[ "$got" == *"$MARK"* ]]; then ok "typed text arrived in the pane"
    else
        bad "typed text never arrived in the pane (marker $MARK)"
        # Enough to tell an app-side race from a harness one without another run:
        # who held focus, what the pane actually shows, and whether a second
        # attempt lands (which would say the app was simply not ready yet).
        echo "       focus=$(xdotool getwindowfocus 2>/dev/null) win=$WIN"
        echo "       pane tail: $(ctl read "$PANE" --tail 10 2>/dev/null | tr '\n' '|' | tail -c 200)"
        send_type "echo retry-$MARK"
        send_key Return
        if [[ "$(await_pane "$PANE" "retry-$MARK")" == *"retry-$MARK"* ]]; then
            echo "       (a second attempt DID land — the app was not ready for the first)"
        else
            echo "       (a second attempt also failed — input is not reaching this pane at all)"
        fi
    fi

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

    # --- the round trip: select output, copy it, paste it into the input line ----
    #
    # The two halves above each prove themselves in isolation — a clipboard we
    # planted goes in, a selection we dragged comes out — and neither proves the
    # gesture a person actually performs: select some text the tool printed, then
    # paste it into the box you type in. Only the round trip does, and it is the
    # one that can fail while both halves pass.
    #
    # The screen is cleared first so the copied row is a value this test chose. An
    # arbitrary row is not safe to assert on: the first attempt dragged a row that
    # already read "command not found", pasted it faithfully, and then convicted
    # the paste of executing. That is the third time this suite has accused the app
    # of a bug that turned out to be the assertion's.
    #
    # `clear; echo RT-x` leaves the token alone on the top row with the prompt
    # below it, so the selection is one short line holding no shell syntax and no
    # prompt string. One row, not the body: a multi-line paste would be testing
    # bracketed-paste mode in whatever shell the pane runs, not the copy path.
    # The token is printed on a band of rows, not one, because the drag below aims
    # in pixels and the pane's first text row is not at a height this script knows.
    # The first attempt echoed it once and dragged through the prompt line sitting
    # underneath it, copying the prompt instead. Identical rows make the aim's
    # remaining error harmless while keeping the selection one row tall — a
    # multi-row selection would be exercising the shell's bracketed-paste handling
    # rather than the copy path this stage is about.
    # Only the digits are matched on. The drag starts a few pixels inside the pane
    # and so can begin mid-character, which clipped the leading "R" off the first
    # working version and read as a copy failure; a marker that survives losing a
    # character or two off its front cannot be broken that way again.
    RT_DIGITS="$$$RANDOM$RANDOM"
    RT="RTX$RT_DIGITS"
    send_key ctrl+u
    send_type "clear; for i in 1 2 3 4 5 6 7 8 9 10 11 12; do echo $RT; done"
    send_key Return
    await_pane "$PANE" "$RT" >/dev/null
    sleep 0.4

    count_rt() { ctl read "$PANE" --tail 60 2>/dev/null | grep -oF -- "$RT_DIGITS" | grep -c . ; }
    before="$(count_rt)"
    act
    # Several heights are tried so a pane whose text starts lower than expected
    # reports a copy failure only when the copy genuinely failed.
    copied=""
    for dy in 90 120 150 60 180; do
        printf 'harness-nothing-was-copied-%s' "$$" | xclip -selection clipboard
        RY=$(( ${Y:-0} + dy ))
        xdotool mousemove $(( ${X:-0} + 20 )) "$RY" mousedown 1 \
                mousemove $(( ${X:-0} + 240 )) "$RY" sleep 0.2 mouseup 1
        sleep 0.3
        send_key ctrl+shift+c
        sleep 0.5
        copied="$(xclip -selection clipboard -o 2>/dev/null)"
        [[ "$copied" == *"$RT_DIGITS"* ]] && break
    done

    if [[ "$copied" != *"$RT_DIGITS"* ]]; then
        bad "the pane's own output did not copy out ($RT absent; clipboard holds '${copied:0:60}')"
    else
        ok "output printed by the pane copied out to the OS clipboard"
        send_key ctrl+v
        after="$before"
        for _ in $(seq 1 40); do
            after="$(count_rt)"
            [[ "$after" -gt "$before" ]] && break
            sleep 0.25
        done
        if [[ "$after" -gt "$before" ]]; then
            ok "copied output pasted back into the input line (round trip)"
        else
            bad "the round trip failed: $RT copied out but never came back (${before} -> ${after})"
        fi
        # Execution is what makes the difference observable: the pasted token is
        # not a command, so a shell that ran it would answer "$RT: command not
        # found" — a string the copied text cannot itself contain, which is the
        # property the previous version of this check lacked.
        sleep 0.5
        tail_now="$(ctl read "$PANE" --tail 6 2>/dev/null)"
        if [[ "$tail_now" == *"$RT_DIGITS: command not found"* || "$tail_now" == *"$RT_DIGITS: not found"* ]]; then
            bad "the round-tripped paste was EXECUTED — a paste must not submit the line"
        else
            ok "the round-tripped paste was not executed"
        fi
        send_key ctrl+u
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

echo "== shift+enter sends a newline, not a submit =="
# The gesture TUIs bind for "another line, don't send yet" — Claude Code among
# them. There is no code point for it, so terminals settled on a meta-prefixed
# CR, which is exactly what Claude Code's own /terminal-setup programs iTerm2
# and VS Code to emit.
#
# `stty -icanon -echo; cat -v` is the witness. `cat -v` renders the bytes it is
# given, so ESC CR shows up on screen as ^[^M — but only with the line
# discipline out of the way. In cooked mode the tty swallows the CR as the line
# terminator and hands cat just the ESC, which is what the first version of this
# check saw and misread as a missing CR. Raw-ish mode keeps every byte, so this
# tests the encoder's output rather than some downstream program's tolerance of
# a bare CR. `-icrnl` matters as much as `-icanon`: with CR-to-NL translation
# left on, the tty rewrites the CR into a newline and cat prints a line break
# where the ^M should be — which is what the second version of this check saw,
# and it too was the tty's doing rather than the encoder's. `-icanon` alone
# leaves isig on, so Ctrl+C still gets us out.
if [[ -n "$WIN" && -n "$PANE" && "$PANE" != null ]]; then
    act
    send_key ctrl+u
    send_type "stty -icanon -echo -icrnl; cat -v"
    send_key Return
    sleep 1
    send_type "before"
    send_key shift+Return
    sleep 0.8
    seen="$(ctl read "$PANE" --tail 12 2>/dev/null)"
    send_key ctrl+c
    sleep 0.4
    send_type "stty sane"
    send_key Return
    sleep 0.5
    if [[ "$seen" == *'^[^M'* ]]; then
        ok "Shift+Enter reaches the pty as ESC CR (newline, not submit)"
    else
        bad "Shift+Enter did not reach the pty as ESC CR (tail: ${seen: -160})"
    fi

    # A plain Enter must still submit, or the fix has broken the common case.
    send_key ctrl+u
    ET="ENTER-$$-$RANDOM"
    send_type "echo $ET"
    send_key Return
    if await_pane "$PANE" "$ET" >/dev/null; then
        ok "plain Enter still submits the line"
    else
        bad "plain Enter no longer submits the line"
    fi
else
    skip "no window or no pane id — shift+enter stage not attempted"
fi

echo
echo "$PASS passed, $FAIL failed, $SKIP skipped"
[[ $FAIL -eq 0 ]]
