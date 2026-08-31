#!/bin/sh
# Cursor Agent sessionStart / sessionEnd hook -> hyperpanes pane->conversation map.
#
# Register in ~/.cursor/hooks.json (user scope) under BOTH events:
#   { "version": 1, "hooks": {
#       "sessionStart": [ { "command": "<path to this script>" } ],
#       "sessionEnd":   [ { "command": "<path to this script>" } ] } }
#
# cursor-agent pipes hook JSON on stdin. Verified against 2026.08.25-3e8eec8, where a
# sessionStart payload is
#   { "conversation_id": "<uuid>", "session_id": "<uuid>", "hook_event_name": "sessionStart",
#     "workspace_roots": ["/path/to/proj"], ... }
# and sessionEnd adds "reason" plus a real "transcript_path" of
#   ~/.cursor/projects/<slug>/agent-transcripts/<conversation_id>/<conversation_id>.jsonl
# — which is what confirms `conversation_id` is the id the on-disk history is keyed by,
# and therefore the id `cursor-agent --resume <id>` takes.
#
# There is no `cwd` field: resume is directory-scoped, and workspace_roots[0] is the
# directory cursor-agent itself considers the conversation to belong to.
#
# When the agent runs inside a hyperpanes pane (HYPERPANES_PANE_ID in the pane env — the
# hook child inherits it, verified) this writes
#   <state dir>/tool-sessions/cursor-agent/<pane-id>.json = { "sessionId":..., "cwd":... }
# on sessionStart and removes it on sessionEnd, so a marker exists exactly while a
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

HP_SESS_DIR="$base/tool-sessions/cursor-agent" HP_PANE="$HYPERPANES_PANE_ID" python3 -c '
import json, os, sys

try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
d = os.environ["HP_SESS_DIR"]
path = os.path.join(d, os.environ["HP_PANE"] + ".json")
try:
    # sessionEnd is named explicitly by the payload; anything else is treated as a start,
    # so a future lifecycle event fails towards recording a live conversation rather than
    # towards deleting the record of one.
    if data.get("hook_event_name") == "sessionEnd":
        try:
            os.remove(path)
        except FileNotFoundError:
            pass
    else:
        roots = data.get("workspace_roots") or []
        cwd = roots[0] if roots and isinstance(roots[0], str) else ""
        os.makedirs(d, exist_ok=True)
        tmp = path + ".tmp"
        with open(tmp, "w") as f:
            json.dump({
                "sessionId": data.get("conversation_id", ""),
                "cwd": cwd,
            }, f)
        os.replace(tmp, path)
except OSError:
    pass
' 2>/dev/null
exit 0
