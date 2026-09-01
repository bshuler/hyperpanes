#!/usr/bin/env bash
# Table test for scripts/guard-live-sessions.py — the PreToolUse hook that keeps
# a stray command from ending the user's live panes.
#
#   bash scripts/guard-live-sessions.test.sh
#
# DENY cases are the ones that have actually happened or are one typo away from
# happening; ALLOW cases are the ordinary work the guard must not get in the way
# of. A guard that blocks routine commands gets disabled, and then it guards
# nothing.
set -uo pipefail

GUARD="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/guard-live-sessions.py"
pass=0
fail=0

check() { # check <expect: deny|allow> <command>
    local expect="$1" cmd="$2" out rc
    out="$(python3 -c 'import json,sys; print(json.dumps({"tool_name":"Bash","tool_input":{"command":sys.argv[1]}}))' "$cmd" | python3 "$GUARD" 2>&1)"
    rc=$?
    local got=allow
    [[ $rc -eq 2 ]] && got=deny
    if [[ "$got" == "$expect" ]]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        echo "FAIL  expected $expect, got $got:  $cmd" >&2
        [[ -n "$out" ]] && echo "      reason: ${out%%$'\n'*}" >&2
    fi
}

# --- the bundle swap that killed a live daemon --------------------------------
check deny "rm -rf /Applications/Hyperpanes.app"
check deny "sudo rm -rf /Applications/Hyperpanes.app"
check deny "rm -rf /Applications/Hyperpanes.app && ditto rs/packaging/out/macos-stage/Hyperpanes.app /Applications/Hyperpanes.app"
check deny "ditto rs/packaging/out/macos-stage/Hyperpanes.app /Applications/Hyperpanes.app"
check deny "cp -R build/Hyperpanes.app /Applications/"
check deny "rsync -a stage/Hyperpanes.app/ /Applications/Hyperpanes.app/"
check deny "mv /Applications/Hyperpanes.app /tmp/old.app"
check deny "cargo build --release && rm -rf /Applications/Hyperpanes.app"
check deny "find /Applications -name 'Hyperpanes.app' -exec rm -rf {} +"
check deny "ls /Applications/*.app | xargs rm -rf"
check deny "chflags -R nouchg /Applications/Hyperpanes.app"

# --- ending the daemon, which ends every pane ---------------------------------
check deny "pkill -f -- '--session-daemon'"
check deny "killall hyperpanes"
check deny "kill 31166 # hyperpanes daemon"

# --- taking the keyboard away from the person using it ------------------------
check deny "osascript -e 'tell application \"Hyperpanes\" to activate'"
check deny "osascript -e 'tell application \"System Events\" to keystroke \"c\" using control down'"
check deny "open -a Hyperpanes"
check deny "hyperpanes ctl focus-pane 9c87051f-8951-47bd-a6bd-15c1e1028fe1"

# --- a heredoc body is data, unless a shell is eating it ----------------------
check allow "$(printf 'cat > docs/x.md <<%s\nrm -rf /Applications/Hyperpanes.app\nMD\n' "'MD'")"
check deny  "$(printf 'bash <<%s\nrm -rf /Applications/Hyperpanes.app\nSH\n' "'SH'")"

# --- the sanctioned path is not blocked ---------------------------------------
check allow "bash scripts/install-macos.sh"
check allow "scripts/install-macos.sh rs/packaging/out/macos-stage/Hyperpanes.app"

# --- ordinary work must stay unobstructed -------------------------------------
check allow "ls -l /Applications"
check allow "codesign --verify --strict /Applications/Hyperpanes.app"
check allow "rm -rf rs/packaging/out/macos-stage"
check allow "cargo test -p hyperpanes-core --lib"
check allow "git commit -m 'fix: something'"
check allow "pgrep -fl hyperpanes"
check allow "hyperpanes ctl read 3 --tail 40"
check allow "hyperpanes ctl new-pane --window 1"
check allow "kill -0 4242"
check allow "osascript -e 'display notification \"build done\"'"
check allow "bash rs/packaging/macos/bundle.sh 0.0.28"

echo "$pass passed, $fail failed"
[[ $fail -eq 0 ]]
