#!/bin/bash
# STT/dictation E2E demo: boots an ISOLATED headless control server and drives the whole
# microphone pipeline with a fake recorder and a fake transcriber — no audio hardware, no
# whisper install, no display. The pane under test runs `cat > typed.txt`, so the evidence
# that the transcript reached the TOOL (not just the API) is a file the tool itself wrote.
# See docs/stt-feature.md.
set -u
REPO="$(cd "$(dirname "$0")/.." && pwd)"
A="${DICTATION_DEMO_DIR:-$(mktemp -d /tmp/hp-stt-demo.XXXXXX)}"
rm -rf "$A"; mkdir -p "$A/state/hyperpanes" "$A/config/hyperpanes" "$A/data"
# Settings (speech.json / stt.json) resolve through config_dir(), which is XDG only on Linux
# and $HOME/Library/Application Support on macOS — so XDG overrides ALONE do not sandbox them,
# and a demo that skipped HOME would silently read the developer's real settings. Override HOME
# too and seed both candidates; the one this platform reads wins, the other is inert.
export HOME="$A/home"
mkdir -p "$A/config/hyperpanes" "$A/home/Library/Application Support/hyperpanes"
TYPED="$A/typed.txt"; EV="$A/events.txt"; : > "$TYPED"; : > "$EV"
FAILED=0
ok()   { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILED=1; }
check() { if [ "$2" = "$3" ]; then ok "$1"; else fail "$1 (want '$3', got '$2')"; fi; }

[ -x "$REPO/rs/target/debug/headless" ] || (cd "$REPO/rs" && cargo build --locked -p hyperpanes-core --bin headless) || exit 1

# Fake recorder. A Custom recorder is stopped with SIGINT on macOS/Linux (backend.rs's
# stop_kind), so it must write its WAV up front and then exit cleanly on the signal — the
# 0.2s sleeps are what let the trap run between them. Touch $A/tiny to make it under-record,
# which is how the "nothing was recorded" guard gets exercised without unplugging anything.
cat > "$A/rec.sh" <<'EOF'
#!/bin/sh
D=$(dirname "$0")
trap 'exit 0' INT TERM
if [ -f "$D/tiny" ]; then head -c 100 /dev/zero > "$1"; else head -c 4096 /dev/zero > "$1"; fi
echo "REC|$1" >> "$D/events.txt"
i=0; while [ $i -lt 900 ]; do sleep 0.2; i=$((i+1)); done
EOF

# Fake transcriber: prints the "speech" on stdout, which is the whole contract, and records
# that it saw a real file so a silently-missing WAV cannot pass as a successful transcription.
cat > "$A/tr.sh" <<'EOF'
#!/bin/sh
D=$(dirname "$0")
echo "TRANSCRIBE|$1|$(wc -c < "$1" | tr -d ' ')" >> "$D/events.txt"
echo "  hello from the **microphone**  "
EOF
chmod +x "$A/rec.sh" "$A/tr.sh"

# stt.json is a USER SETTING -> config dir, read once at startup (DictationService::new).
# submit:true so the demo proves the SEPARATE delayed Enter as well as the insert.
jq -n --arg r "$A/rec.sh" --arg t "$A/tr.sh" \
  '{recordTemplate:["/bin/sh",$r,"{wav}"],transcribeTemplate:["/bin/sh",$t,"{wav}"],submit:true}' \
  > "$A/stt.json"
cp "$A/stt.json" "$A/config/hyperpanes/stt.json"
cp "$A/stt.json" "$A/home/Library/Application Support/hyperpanes/stt.json"

CJ="$A/state/hyperpanes/control.json"
# HYPERPANES_CONTROL_FILE leaks in from this pane's env and would clobber the LIVE app's
# discovery file — pin it into the sandbox explicitly.
# A fresh sandbox has no control-settings.json, so `allowInput` defaults OFF — and dictation
# types into a pty, so it lives or dies with that switch. Turn it on explicitly rather than
# inheriting whatever the developer happens to have enabled.
env -u HYPERPANES_PANE_ID \
  XDG_STATE_HOME="$A/state" XDG_CONFIG_HOME="$A/config" XDG_DATA_HOME="$A/data" \
  HYPERPANES_CONTROL_FILE="$CJ" HYPERPANES_MSG_NUDGE=0 HYPERPANES_ALLOW_INPUT=1 \
  "$REPO/rs/target/debug/headless" > "$A/headless.log" 2>&1 &
HPID=$!
trap 'kill $HPID 2>/dev/null' EXIT

for i in $(seq 1 50); do [ -s "$CJ" ] && break; sleep 0.2; done
[ -s "$CJ" ] || { echo "FAIL: control.json never appeared"; cat "$A/headless.log"; exit 1; }
T=$(jq -r .token "$CJ"); P=$(jq -r .port "$CJ"); H=$(jq -r '.bindAddress // "127.0.0.1"' "$CJ")
hp() { local m=$1 p=$2 b=${3:-}; if [ -n "$b" ]; then curl -sS -m 30 -X "$m" "http://$H:$P$p" -H "Authorization: Bearer $T" -H 'content-type: application/json' -d "$b"; else curl -sS -m 30 -X "$m" "http://$H:$P$p" -H "Authorization: Bearer $T"; fi }
echo "== headless up on $H:$P (pid $HPID), sandbox $A"

# The recordings directory is pid-scoped scratch: raw audio of the user never lands beside
# the settings, and this demo asserts it is empty again at the end.
WAVDIR="${TMPDIR:-/tmp}/hyperpanes-dictation-$HPID"

# A pane whose program is a sink: whatever the pty delivers ends up in $TYPED, and only a
# real Enter (the separate delayed CR write) makes `cat` flush a line out.
WID=$(hp GET /state | jq '.windows[0].windowId')
PANE=$(hp POST /command "{\"type\":\"newPane\",\"windowId\":$WID,\"pane\":{\"command\":\"/bin/sh\",\"args\":[\"-c\",\"cat > '$TYPED'\"],\"label\":\"dictate\"}}" | jq -r .result)
[ -n "$PANE" ] && [ "$PANE" != null ] || { echo "FAIL: newPane"; cat "$A/headless.log"; exit 1; }
OTHER=$(hp POST /command "{\"type\":\"newPane\",\"windowId\":$WID,\"pane\":{\"command\":\"sleep\",\"args\":[\"600\"],\"label\":\"other\"}}" | jq -r .result)
echo "panes: $PANE $OTHER"
sleep 0.5

echo "-- backends as detected from stt.json"
hp GET /state | jq -c .dictation

# 1. start -> the pane is listed as recording, and the recorder process actually ran.
hp POST /command "{\"type\":\"startDictation\",\"paneId\":\"$PANE\"}" | jq -c .
sleep 0.8
check "start lists the pane as recording" \
  "$(hp GET /state | jq -r --arg p "$PANE" '.dictation.recordingPanes | index($p) != null')" true
check "the recorder was actually spawned" "$(grep -c '^REC|' "$EV")" 1

# 2. a second start on the same pane must not spawn a second recorder.
hp POST /command "{\"type\":\"startDictation\",\"paneId\":\"$PANE\"}" | jq -c .
check "starting an already-recording pane spawns nothing new" "$(grep -c '^REC|' "$EV")" 1

# 3. stop -> transcribe, sanitize, type, and (submit:true) press Enter as a separate write.
STOP=$(hp POST /command "{\"type\":\"stopDictation\",\"paneId\":\"$PANE\"}")
echo "$STOP" | jq -c .
check "stop returns the transcriber's text, whitespace-trimmed" \
  "$(echo "$STOP" | jq -r '.text // ""')" "hello from the **microphone**"
check "stop reports it submitted" "$(echo "$STOP" | jq -r '.submitted // ""')" true
check "the transcriber saw a real WAV" "$(grep -c '^TRANSCRIBE|.*|4096$' "$EV")" 1
check "the pane is no longer recording" \
  "$(hp GET /state | jq -r --arg p "$PANE" '.dictation.recordingPanes | index($p) != null')" false
sleep 1
check "the TOOL received the line" "$(cat "$TYPED")" "hello from the **microphone**"

# 4. cancel -> the recording ends and nothing at all is typed.
hp POST /command "{\"type\":\"startDictation\",\"paneId\":\"$PANE\"}" > /dev/null
sleep 0.6
hp POST /command "{\"type\":\"cancelDictation\",\"paneId\":\"$PANE\"}" | jq -c .
sleep 0.6
check "cancel stops the recording" \
  "$(hp GET /state | jq -r --arg p "$PANE" '.dictation.recordingPanes | index($p) != null')" false
check "cancel types nothing" "$(cat "$TYPED")" "hello from the **microphone**"
check "cancel never reaches the transcriber" "$(grep -c '^TRANSCRIBE|' "$EV")" 1

# 5. stopping a pane that was never started is an error, not a hang or a stray keystroke.
check "stopping an idle pane is refused" \
  "$(hp POST /command "{\"type\":\"stopDictation\",\"paneId\":\"$PANE\"}" | jq -r 'has("error")')" true

# 6. an under-recorded WAV must be reported, not transcribed — this is what a denied
#    microphone looks like from here, and it must never type garbage into a live prompt.
touch "$A/tiny"
hp POST /command "{\"type\":\"startDictation\",\"paneId\":\"$PANE\"}" > /dev/null
sleep 0.6
TINY=$(hp POST /command "{\"type\":\"stopDictation\",\"paneId\":\"$PANE\"}")
echo "$TINY" | jq -c .
case "$(echo "$TINY" | jq -r '.error // ""')" in
  *"nothing was recorded"*) ok "a too-short recording is reported, not typed" ;;
  *) fail "a too-short recording should be reported (got: $(echo "$TINY" | jq -c .))" ;;
esac
check "a too-short recording never reaches the transcriber" "$(grep -c '^TRANSCRIBE|' "$EV")" 1
rm -f "$A/tiny"

# 7. two panes record independently, and closing one while it records must not leave a
#    microphone running — a live recorder outliving its pane is the worst failure here.
hp POST /command "{\"type\":\"startDictation\",\"paneId\":\"$PANE\"}" > /dev/null
hp POST /command "{\"type\":\"startDictation\",\"paneId\":\"$OTHER\"}" > /dev/null
sleep 0.8
check "both panes record at once" \
  "$(hp GET /state | jq -r '.dictation.recordingPanes | length')" 2
hp POST /command "{\"type\":\"closePane\",\"paneId\":\"$OTHER\"}" | jq -c .
sleep 0.8
check "closing a pane cancels its recorder" \
  "$(hp GET /state | jq -r --arg p "$OTHER" '.dictation.recordingPanes | index($p) != null')" false
check "the other pane keeps recording" \
  "$(hp GET /state | jq -r --arg p "$PANE" '.dictation.recordingPanes | index($p) != null')" true
hp POST /command "{\"type\":\"cancelDictation\",\"paneId\":\"$PANE\"}" > /dev/null
sleep 0.5

# 8. no recorder process may survive the run, and no raw audio may be left on disk.
check "no fake recorder is still running" "$(pgrep -f "$A/rec.sh" | wc -l | tr -d ' ')" 0
LEFT=$(ls -1 "$WAVDIR" 2>/dev/null | wc -l | tr -d ' ')
check "no WAV outlives the dictation" "$LEFT" 0

kill $HPID 2>/dev/null
echo
if [ $FAILED -eq 0 ]; then echo "== ALL PASS — evidence in $A"; else echo "== FAILURES above — evidence in $A"; fi
exit $FAILED
