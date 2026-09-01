#!/usr/bin/env bash
# End-to-end test for scripts/install-macos.sh, run against a throwaway --dest.
#
#   bash scripts/install-macos.test.sh
#
# The property under test is the one that matters: a process running out of the
# installed bundle is still running after the bundle has been replaced. That is
# what `rm -rf` breaks and what the rename-aside is for, so it is asserted with
# a real live process, not by reading the script.
set -uo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
INSTALL="$HERE/install-macos.sh"
ROOT="$(mktemp -d)"
DEST="$ROOT/apps/Hyperpanes.app"
pass=0
fail=0

cleanup() {
    [[ -n "${VICTIM:-}" ]] && kill "$VICTIM" 2>/dev/null
    chflags -R nouchg "$ROOT" 2>/dev/null
    rm -rf "$ROOT"
}
trap cleanup EXIT

ok()  { pass=$((pass + 1)); }
no()  { fail=$((fail + 1)); echo "FAIL  $1" >&2; }
want() { if eval "$2"; then ok; else no "$1"; fi; }

make_bundle() { # make_bundle <dir> <marker>
    mkdir -p "$1/Contents/MacOS"
    printf '#!/bin/sh\nwhile :; do sleep 1; done\n' > "$1/Contents/MacOS/hyperpanes"
    chmod +x "$1/Contents/MacOS/hyperpanes"
    echo "$2" > "$1/Contents/MacOS/marker"
}

make_bundle "$ROOT/v1" one
make_bundle "$ROOT/v2" two

# --- first install, into an empty destination ---------------------------------
bash "$INSTALL" "$ROOT/v1" --dest "$DEST" >/dev/null 2>&1
want "first install lands the bundle" '[[ -x "$DEST/Contents/MacOS/hyperpanes" ]]'
want "first install copies the right build" '[[ "$(cat "$DEST/Contents/MacOS/marker")" == one ]]'
want "installed bundle is locked" '/bin/ls -ldO "$DEST" | grep -q uchg'
want "the lock refuses rm -rf" '! rm -rf "$DEST" 2>/dev/null && [[ -d "$DEST" ]]'
want "the lock refuses an overwrite" '! ditto "$ROOT/v2" "$DEST" 2>/dev/null'
want "the lock refuses a rename" '! mv "$DEST" "$ROOT/apps/moved.app" 2>/dev/null'

# --- a process running out of the installed bundle ----------------------------
"$DEST/Contents/MacOS/hyperpanes" &
VICTIM=$!
disown 2>/dev/null   # else the shell announces "Terminated" when cleanup reaps it
sleep 0.3
want "the victim process started" 'kill -0 "$VICTIM" 2>/dev/null'

# --- second install, with that process live -----------------------------------
bash "$INSTALL" "$ROOT/v2" --dest "$DEST" >/dev/null 2>&1
want "second install replaced the bundle" '[[ "$(cat "$DEST/Contents/MacOS/marker")" == two ]]'
want "THE PROCESS SURVIVED THE SWAP" 'kill -0 "$VICTIM" 2>/dev/null'
want "the old bundle was retired, not deleted" '[[ -n "$(/bin/ls -1d "$ROOT/apps/.hyperpanes-attic"/*.app 2>/dev/null)" ]]'
want "the new bundle is locked again" '/bin/ls -ldO "$DEST" | grep -q uchg'
want "no staging directory is left behind" '[[ ! -e "$ROOT/apps/.Hyperpanes.app.incoming" ]]'

# --- the attic keeps the newest few and prunes the rest ------------------------
for v in 3 4 5; do
    make_bundle "$ROOT/v$v" "$v"
    bash "$INSTALL" "$ROOT/v$v" --dest "$DEST" >/dev/null 2>&1
done
retired=$(/bin/ls -1d "$ROOT/apps/.hyperpanes-attic"/*.app 2>/dev/null | wc -l | tr -d ' ')
want "the attic is pruned (kept $retired)" '[[ "$retired" -le 3 ]]'
want "the process is STILL alive after four swaps" 'kill -0 "$VICTIM" 2>/dev/null'

# --- a bad source is refused before anything is touched ------------------------
bash "$INSTALL" "$ROOT/nope" --dest "$DEST" >/dev/null 2>&1
rc=$?
want "a missing source exits non-zero" '[[ "$rc" -ne 0 ]]'
marker="$(cat "$DEST/Contents/MacOS/marker" 2>&1)"
want "and leaves the install intact" '[[ "$marker" == 5 ]]'

echo "$pass passed, $fail failed"
[[ $fail -eq 0 ]]
