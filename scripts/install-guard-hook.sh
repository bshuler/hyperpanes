#!/usr/bin/env bash
# Install the live-session guard as a MACHINE-WIDE Claude Code hook.
#
# The repo-local wiring in .claude/settings.json only protects sessions whose
# checkout contains it. A worktree on an older revision has no hook at all, which
# is how `rm -rf /Applications/Hyperpanes.app` got submitted on 2026-09-02 with
# nothing to refuse it (docs/live-session-safety.md). Installing the guard at
# user level removes the dependency on what happens to be checked out.
#
# Copy, never symlink: a symlink into a worktree reintroduces the dependency.
# Re-run to pick up changes to the tested repository copy.
#
#   bash scripts/install-guard-hook.sh          # install / refresh
#   bash scripts/install-guard-hook.sh --check   # report only, exit 1 if absent
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
SRC="$ROOT/scripts/guard-live-sessions.py"
DEST_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
DEST="$DEST_DIR/hooks/guard-live-sessions.py"
SETTINGS="$DEST_DIR/settings.json"
CHECK=0
[ "${1:-}" = "--check" ] && CHECK=1

[ -f "$SRC" ] || { echo "missing $SRC" >&2; exit 1; }

if [ "$CHECK" = 1 ]; then
  python3 - "$DEST" "$SETTINGS" <<'PY'
import json,os,sys
dest,settings=sys.argv[1],sys.argv[2]
ok=os.path.exists(dest)
wired=False
if os.path.exists(settings):
    try:
        d=json.load(open(settings))
        for e in d.get('hooks',{}).get('PreToolUse',[]):
            for h in e.get('hooks',[]):
                if 'guard-live-sessions' in h.get('command',''): wired=True
    except ValueError: pass
print(f"script installed: {ok}\nhook wired:       {wired}")
sys.exit(0 if (ok and wired) else 1)
PY
  exit $?
fi

mkdir -p "$DEST_DIR/hooks"
cp "$SRC" "$DEST"
chmod +x "$DEST"
echo "installed $DEST"

cp "$SETTINGS" "$SETTINGS.bak-$(date +%Y%m%d%H%M%S)" 2>/dev/null || true
python3 - "$DEST" "$SETTINGS" <<'PY'
import json,os,sys
dest,settings=sys.argv[1],sys.argv[2]
d={}
if os.path.exists(settings):
    try: d=json.load(open(settings))
    except ValueError:
        print("settings.json is not valid JSON; refusing to edit",file=sys.stderr); sys.exit(1)
pre=d.setdefault('hooks',{}).setdefault('PreToolUse',[])
for e in pre:
    for h in e.get('hooks',[]):
        if 'guard-live-sessions' in h.get('command',''):
            h['command']=f'python3 "{dest}"'
            json.dump(d,open(settings,'w'),indent=2); print("refreshed existing hook entry"); sys.exit(0)
pre.append({'matcher':'Bash','hooks':[{'type':'command','command':f'python3 "{dest}"'}]})
json.dump(d,open(settings,'w'),indent=2)
print("wired PreToolUse/Bash hook")
PY

printf '%s' '{"tool_name":"Bash","tool_input":{"command":"rm -rf /Applications/Hyperpanes.app"}}' \
  | python3 "$DEST" >/dev/null 2>&1 && { echo "SELFTEST FAILED: guard did not block rm" >&2; exit 1; }
echo "selftest ok: guard blocks rm -rf /Applications/Hyperpanes.app"
