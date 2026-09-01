#!/bin/bash
# Talk-feature E2E demo: boots an ISOLATED headless control server, fakes two panes'
# Claude session markers + transcripts, and proves the talk pipeline end-to-end with a
# file-backed TTS backend (no audio, no display needed). See docs/talk-feature.md.
set -u
REPO="$(cd "$(dirname "$0")/.." && pwd)"
A="${TALK_DEMO_DIR:-$(mktemp -d /tmp/hp-talk-demo.XXXXXX)}"
rm -rf "$A"; mkdir -p "$A/state" "$A/config" "$A/data" "$A/proj" "$A/claude"
UTT="$A/utterances.txt"

[ -x "$REPO/rs/target/debug/headless" ] || (cd "$REPO/rs" && cargo build --locked -p hyperpanes-core --bin headless) || exit 1

cat > "$A/speak.sh" <<'EOF'
#!/bin/sh
# Fake TTS backend: serialized evidence writer. $1 = spoken text.
exec 9>>"$(dirname "$0")/utt.lock"
flock 9
echo "START|$1" >> "$(dirname "$0")/utterances.txt"
sleep 0.4
echo "END|$1" >> "$(dirname "$0")/utterances.txt"
EOF
chmod +x "$A/speak.sh"

# Where the app itself will look. XDG overrides sandbox nothing on macOS — every app dir there
# hangs off $HOME — so HOME is overridden too and the paths are resolved the same way the app
# resolves them. Without this the demo seeds a sandbox and then reads the DEVELOPER's real
# settings and session markers, which is a demo that proves nothing.
export HOME="$A/home"
case "$(uname -s)" in
  Darwin) CFG="$A/home/Library/Application Support/hyperpanes"; ST="$CFG" ;;
  *)      CFG="$A/config/hyperpanes";                           ST="$A/state/hyperpanes" ;;
esac
mkdir -p "$CFG" "$ST"

# Pre-seed speech settings with the custom command template (headless reads at startup).
# speech.json is a USER SETTING -> config dir (like ai-settings.json), not the state dir.
printf '{"commandTemplate":["/bin/sh","%s","{text}"]}\n' "$A/speak.sh" > "$CFG/speech.json"

CJ="$ST/control.json"
# HYPERPANES_CONTROL_FILE leaks in from this pane's env and would clobber the LIVE app's
# discovery file — pin it into the sandbox explicitly.
# A fresh sandbox has no control-settings.json, so `allowInput` defaults OFF. Turn it on
# explicitly rather than inheriting whatever the developer happens to have enabled.
env -u HYPERPANES_PANE_ID \
  XDG_STATE_HOME="$A/state" XDG_CONFIG_HOME="$A/config" XDG_DATA_HOME="$A/data" \
  HYPERPANES_CONTROL_FILE="$CJ" HYPERPANES_MSG_NUDGE=0 HYPERPANES_ALLOW_INPUT=1 \
  "$REPO/rs/target/debug/headless" > "$A/headless.log" 2>&1 &
HPID=$!
trap 'kill $HPID 2>/dev/null' EXIT

for i in $(seq 1 50); do [ -s "$CJ" ] && break; sleep 0.2; done
[ -s "$CJ" ] || { echo "FAIL: control.json never appeared"; cat "$A/headless.log"; exit 1; }
T=$(jq -r .token "$CJ"); P=$(jq -r .port "$CJ"); H=$(jq -r '.bindAddress // "127.0.0.1"' "$CJ")
hp() { local m=$1 p=$2 b=${3:-}; if [ -n "$b" ]; then curl -sS -m 10 -X "$m" "http://$H:$P$p" -H "Authorization: Bearer $T" -H 'content-type: application/json' -d "$b"; else curl -sS -m 10 -X "$m" "http://$H:$P$p" -H "Authorization: Bearer $T"; fi }

echo "== headless up on $H:$P"

# Two panes (plain sleep shells; the talk pipeline reads transcripts, not the PTY).
WID=$(hp GET /state | jq '.windows[0].windowId')
P1=$(hp POST /command "{\"type\":\"newPane\",\"windowId\":$WID,\"pane\":{\"command\":\"sleep\",\"args\":[\"600\"],\"label\":\"alpha\"}}" | jq -r .result)
P2=$(hp POST /command "{\"type\":\"newPane\",\"windowId\":$WID,\"pane\":{\"command\":\"sleep\",\"args\":[\"600\"],\"label\":\"bravo\"}}" | jq -r .result)
echo "panes: $P1 $P2"
[ -n "$P1" ] && [ -n "$P2" ] && [ "$P1" != null ] || { echo FAIL:newPane; exit 1; }

# Fake Claude session markers + transcripts (configDir-aware store).
S1=aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa
S2=bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb
ENC=$(echo -n "$A/proj" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$A/claude/projects/$ENC" "$ST/claude-sessions"
TR1="$A/claude/projects/$ENC/$S1.jsonl"; TR2="$A/claude/projects/$ENC/$S2.jsonl"
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"OLD history must never be spoken"}]}}' > "$TR1"
: > "$TR2"
jq -n --arg s "$S1" --arg c "$A/proj" --arg d "$A/claude" '{sessionId:$s,cwd:$c,configDir:$d}' > "$ST/claude-sessions/$P1.json"
jq -n --arg s "$S2" --arg c "$A/proj" --arg d "$A/claude" '{sessionId:$s,cwd:$c,configDir:$d}' > "$ST/claude-sessions/$P2.json"

# C: toggle talk via control command; read state back.
hp POST /command "{\"type\":\"setTalk\",\"paneId\":\"$P1\",\"enabled\":true}" | jq -c .
hp POST /command "{\"type\":\"setTalk\",\"paneId\":\"$P2\",\"enabled\":true}" | jq -c .
sleep 1
hp GET /state | jq -c '{talk:[.windows[].tabs[].panes[]|select(.talk==true)|.id], speech:.speech}'

# D+E: append markdown-y assistant replies to BOTH transcripts, interleaved.
for i in 1 2 3; do
  echo '{"type":"assistant","message":{"content":[{"type":"text","text":"**Alpha reply '$i'** with `inline code` and a [link](https://example.com).","other":1},{"type":"tool_use","name":"Bash","input":{}}]}}' >> "$TR1"
  echo '{"type":"assistant","message":{"content":[{"type":"text","text":"# Bravo heading '$i'\n- bullet one\n```rust\nfn hidden() {}\n```"}]}}' >> "$TR2"
  sleep 0.5
done
sleep 6
echo "== utterances after overlap test:"; cat "$UTT" 2>/dev/null

# Stop test: long utterance then stopSpeech kills it.
# Re-write rather than `sed -i`: BSD sed reads the script as the backup suffix, so the
# in-place edit silently did nothing on macOS and the stop test never had anything to kill.
sed 's/sleep 0.4/sleep 20/' "$A/speak.sh" > "$A/speak.next" && mv "$A/speak.next" "$A/speak.sh"
chmod +x "$A/speak.sh"
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"This long utterance should be killed by stopSpeech."}]}}' >> "$TR1"
sleep 2.5
BEFORE=$(grep -c 'START|' "$UTT")
hp POST /command '{"type":"stopSpeech"}' | jq -c .
sleep 2
AFTER_END=$(grep -c '^END|This long' "$UTT" || true)
echo "== stop test: starts=$BEFORE ends_of_killed=$AFTER_END"
hp GET /state | jq -c .speech
echo "== history check (must be absent):"; grep -c 'OLD history' "$UTT" || echo "0 (good)"
kill $HPID 2>/dev/null
echo "== DONE — evidence in $UTT (dir kept: $A)"
