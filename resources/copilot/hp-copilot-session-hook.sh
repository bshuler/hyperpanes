#!/bin/sh
# GitHub Copilot CLI sessionStart / sessionEnd hook -> hyperpanes pane->conversation map.
#
# Register in ~/.copilot/settings.json (user scope) under BOTH events:
#   { "hooks": {
#       "sessionStart": [ { "command": "<path to this script>" } ],
#       "sessionEnd":   [ { "command": "<path to this script>" } ] } }
#
# NOT config.json: writing the same block there works, but the CLI migrates it into
# settings.json on next launch ("User settings belong in settings.json. This file is
# managed automatically."), so settings.json is the file to own. And no "version": 1
# wrapper at this level — that belongs to the repo-scoped .github/hooks/*.json form,
# which is trust-gated and did not fire non-interactively.
#
# copilot pipes hook JSON on stdin. Verified against 1.0.80 (runtime 1.0.82):
#   sessionStart { "sessionId": "<uuid>", "timestamp": …, "cwd": "/path", "source": "new",
#                  "initialPrompt": "…" }
#   sessionEnd   { "sessionId": "<uuid>", "timestamp": …, "cwd": "/path", "reason": "complete" }
# `sessionId` is exactly the id the CLI prints as `copilot --resume=<id>`.
#
# Neither payload carries an event name, so the two are told apart by the field only the
# end one has ("reason"). Erring towards "start" means an unrecognised payload records a
# live conversation rather than deleting the record of one.
#
# When copilot runs inside a hyperpanes pane (HYPERPANES_PANE_ID in the pane env — the
# hook child inherits it, verified) this writes
#   <state dir>/tool-sessions/copilot/<pane-id>.json = { "sessionId":..., "cwd":... }
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

HP_SESS_DIR="$base/tool-sessions/copilot" HP_PANE="$HYPERPANES_PANE_ID" python3 -c '
import json, os, sys

try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
d = os.environ["HP_SESS_DIR"]
path = os.path.join(d, os.environ["HP_PANE"] + ".json")
try:
    if "reason" in data:
        try:
            os.remove(path)
        except FileNotFoundError:
            pass
    else:
        os.makedirs(d, exist_ok=True)
        tmp = path + ".tmp"
        with open(tmp, "w") as f:
            json.dump({
                "sessionId": data.get("sessionId", ""),
                "cwd": data.get("cwd", ""),
            }, f)
        os.replace(tmp, path)
except OSError:
    pass
' 2>/dev/null
exit 0
