#!/bin/sh
# Gemini CLI SessionStart / SessionEnd hook -> hyperpanes pane->conversation map.
#
# Register in ~/.gemini/settings.json (or $GEMINI_CLI_HOME/.gemini/settings.json) under a
# top-level "hooks" key, in BOTH events. Gemini takes Claude Code's nested matcher-group
# JSON, not cursor/copilot's flat one, and the event names are PascalCase — an event
# spelled `sessionStart` is silently never called:
#   { "hooks": {
#       "SessionStart": [ { "hooks": [ { "type": "command", "command": "<this script>" } ] } ],
#       "SessionEnd":   [ { "hooks": [ { "type": "command", "command": "<this script>" } ] } ] } }
# That gemini ships a `gemini hooks migrate` ("Migrate hooks from Claude Code to Gemini
# CLI") is the corroboration for the shape. Unlike codex, gemini does NOT trust-gate hooks:
# the file above is honoured as written, with nothing for the human to approve.
#
# gemini pipes hook JSON on stdin. Verified against 0.58.0:
#   SessionStart { "session_id": "<uuid>", "transcript_path": "…/chats/session-<stamp>-<8>.jsonl",
#                  "cwd": "/path", "hook_event_name": "SessionStart",
#                  "timestamp": "…", "source": "startup" }
#   SessionEnd   { …same session_id/transcript_path/cwd…, "hook_event_name": "SessionEnd",
#                  "timestamp": "…", "reason": "exit" }
# `session_id` is the full uuid; the chat filename embeds only its first 8 characters, and
# the directory above it is named by a first-seen-wins scheme, so the Talk tailer searches
# ~/.gemini/tmp/*/chats and confirms the hit against the id on the file's first line.
#
# When gemini runs inside a hyperpanes pane (HYPERPANES_PANE_ID in the pane env — the hook
# child inherits it) this writes
#   <state dir>/tool-sessions/gemini/<pane-id>.json = { "sessionId":..., "cwd":... }
# on SessionStart and removes it on SessionEnd, so a marker exists exactly while a
# conversation is live in that pane.
# (Path must mirror hyperpanes-core tools::session_hook::marker_dir.)
#
# Outside a pane, or on any error, this exits 0 silently — a hook must never break the tool.
[ -n "$HYPERPANES_PANE_ID" ] || { cat >/dev/null 2>&1; exit 0; }

case "$(uname 2>/dev/null)" in
  Darwin) base="$HOME/Library/Application Support/hyperpanes" ;;
  *)      base="${XDG_STATE_HOME:-$HOME/.local/state}/hyperpanes" ;;
esac

HP_SESS_DIR="$base/tool-sessions/gemini" HP_PANE="$HYPERPANES_PANE_ID" python3 -c '
import json, os, sys
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
d = os.environ["HP_SESS_DIR"]
path = os.path.join(d, os.environ["HP_PANE"] + ".json")
try:
    # SessionEnd is named explicitly by the payload; anything else is treated as a start,
    # so a future lifecycle event fails towards recording a live conversation rather than
    # towards deleting the record of one.
    if data.get("hook_event_name") == "SessionEnd":
        try:
            os.remove(path)
        except FileNotFoundError:
            pass
    else:
        os.makedirs(d, exist_ok=True)
        tmp = path + ".tmp"
        with open(tmp, "w") as f:
            json.dump({
                "sessionId": data.get("session_id", ""),
                "cwd": data.get("cwd", ""),
            }, f)
        os.replace(tmp, path)
except OSError:
    pass
' 2>/dev/null
exit 0
