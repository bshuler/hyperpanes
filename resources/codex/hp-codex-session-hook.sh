#!/bin/sh
# Codex CLI SessionStart / SessionEnd hook -> hyperpanes pane->conversation map.
#
# Register in $CODEX_HOME/hooks.json (default ~/.codex/hooks.json) under BOTH events.
# Codex takes Claude Code's nested matcher-group JSON, not cursor/copilot's flat one, and
# the event names are PascalCase — an event spelled `sessionStart` is silently never called:
#   { "hooks": {
#       "SessionStart": [ { "hooks": [ { "type": "command", "command": "<this script>" } ] } ],
#       "SessionEnd":   [ { "hooks": [ { "type": "command", "command": "<this script>" } ] } ] } }
#
# NOT config.toml (it accepts unknown keys silently, so a wrong shape there looks like it
# worked and never fires) and NOT the project-scoped <cwd>/.codex/hooks.json, which did not
# fire in testing. Codex also trust-gates hooks: the file above is only honoured after the
# human approves it once inside codex (or with --dangerously-bypass-hook-trust). That is a
# security control — hyperpanes writes the file and leaves the approval to the person.
#
# codex pipes hook JSON on stdin. Verified against 0.151.0:
#   SessionStart { "session_id": "<uuid>", "transcript_path": "…/rollout-<stamp>-<id>.jsonl",
#                  "cwd": "/path", "hook_event_name": "SessionStart", "model": "…",
#                  "permission_mode": "…", "source": "startup" }
#   SessionEnd   { …same session_id/transcript_path/cwd…, "hook_event_name": "SessionEnd",
#                  "reason": "other" }
# `session_id` is the id `codex resume <id>` takes, and the id embedded in the rollout
# filename that the Talk tailer searches the dated session tree for.
#
# When codex runs inside a hyperpanes pane (HYPERPANES_PANE_ID in the pane env — the hook
# child inherits it, verified) this writes
#   <state dir>/tool-sessions/codex/<pane-id>.json = { "sessionId":..., "cwd":... }
# on SessionStart and removes it on SessionEnd, so a marker exists exactly while a
# conversation is live in that pane. The GUI's relaunch snapshot adopts the id as the
# pane's tool session mark, letting a restored pane resume the same conversation.
# (Path must mirror hyperpanes-core tools::session_hook::marker_dir.)
#
# Outside a pane, or on any error, this exits 0 silently — a hook must never break the tool.
[ -n "$HYPERPANES_PANE_ID" ] || { cat >/dev/null 2>&1; exit 0; }

case "$(uname 2>/dev/null)" in
  Darwin) base="$HOME/Library/Application Support/hyperpanes" ;;
  *)      base="${XDG_STATE_HOME:-$HOME/.local/state}/hyperpanes" ;;
esac

HP_SESS_DIR="$base/tool-sessions/codex" HP_PANE="$HYPERPANES_PANE_ID" python3 -c '
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
