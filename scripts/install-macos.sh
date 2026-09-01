#!/usr/bin/env bash
# Hyperpanes — install a built Hyperpanes.app into /Applications without killing
# the running session daemon.
#
#   scripts/install-macos.sh [<source Hyperpanes.app>] [--dest <path>]
#     default source: rs/packaging/out/macos-stage/Hyperpanes.app
#     default dest:   /Applications/Hyperpanes.app
#
# Why this script exists at all
# ----------------------------
# The session daemon (docs/session-daemon-plan.md) is the whole reason panes
# survive a GUI restart: it is a detached process that owns every PTY, and the
# GUI is only a client. Quitting the GUI does not touch it. But it is running
# *out of the installed bundle*, and macOS kills a process whose code-signed
# executable is unlinked underneath it. So the obvious install —
#
#     rm -rf /Applications/Hyperpanes.app && ditto new /Applications/...
#
# — destroys every running program in every pane, which is exactly the failure
# the daemon was built to prevent. It is a one-line mistake with no warning and
# no undo.
#
# This script never deletes a bundle a process might still be running from. The
# old one is *renamed aside* into an attic; rename keeps the inode, so the live
# daemon keeps executing happily from the moved directory. It is deleted only on
# a later run, once nothing is running out of it.
#
# The installed bundle is then locked with `chflags -R uchg`, which makes
# `rm -rf`, `ditto`, `cp`, `rsync` and `mv` against it fail with "Operation not
# permitted" for anyone who is not this script. That lock is the guardrail: it
# does not depend on anybody — human or agent — remembering any of the above.
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

# --dest exists so install-macos.test.sh can exercise the swap, the lock and the
# attic against a throwaway directory. Real installs take the default.
DEST="/Applications/Hyperpanes.app"
SRC=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dest) DEST="${2:-}"; shift 2 ;;
        -h|--help)
            echo "usage: install-macos.sh [<source Hyperpanes.app>] [--dest <path>]"; exit 0 ;;
        *) SRC="$1"; shift ;;
    esac
done
SRC="${SRC:-$REPO_ROOT/rs/packaging/out/macos-stage/Hyperpanes.app}"
DEST_DIR="$(dirname -- "$DEST")"
ATTIC="$DEST_DIR/.hyperpanes-attic"
STAGE="$DEST_DIR/.Hyperpanes.app.incoming"
KEEP_RETIRED=2   # attic bundles kept even when nothing runs from them

die() { echo "error: $*" >&2; exit 1; }
note() { echo "==> $*"; }

[[ "$(uname -s)" == "Darwin" ]] || die "macOS only (this is the /Applications installer)"

# --- validate the source before anything on disk is touched --------------------
[[ -d "$SRC" ]] || die "no such bundle: $SRC
       build one first:  bash rs/packaging/macos/bundle.sh <version>"
SRC="$(cd -- "$SRC" && pwd)"
[[ -x "$SRC/Contents/MacOS/hyperpanes" ]] || die "$SRC has no Contents/MacOS/hyperpanes"
[[ "$SRC" == "$DEST" ]] && die "source and destination are the same path"

# PlistBuddy prints its "File Doesn't Exist, Will Create:" chatter on stdout and
# still exits 0, so the answer has to be validated rather than trusted — it ends
# up in a filename.
bundle_version() { # bundle_version <bundle> -> a filename-safe version
    local plist="$1/Contents/Info.plist" v=""
    [[ -f "$plist" ]] && v="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' \
        "$plist" 2>/dev/null | head -1)"
    case "$v" in ""|*[!A-Za-z0-9._-]*) v=unknown ;; esac
    printf '%s' "$v"
}

VERSION="$(bundle_version "$SRC")"

if ! codesign --verify --strict "$SRC" 2>/dev/null; then
    echo "WARNING: $SRC fails codesign --verify; installing anyway" >&2
fi

# --- who is running right now --------------------------------------------------
# Recorded before the swap and re-checked after it: a daemon that dies across
# this script took the user's sessions with it, and that has to be an error, not
# a shrug. The GUI is listed too but only for the report — losing it is cheap.
daemon_pids() { pgrep -f -- '--session-daemon' 2>/dev/null || true; }
gui_pids() { pgrep -f "^$DEST/Contents/MacOS/hyperpanes$" 2>/dev/null || true; }

BEFORE_DAEMONS="$(daemon_pids)"
note "installing $VERSION"
note "  from: $SRC"
note "    to: $DEST"
if [[ -n "$BEFORE_DAEMONS" ]]; then
    note "  live session daemon(s): $(echo "$BEFORE_DAEMONS" | tr '\n' ' ')— must survive"
else
    note "  no session daemon running"
fi
[[ -n "$(gui_pids)" ]] && note "  GUI is running; it can keep running (it relaunches from the new bundle)"

# --- stage the new bundle beside the old one -----------------------------------
# Staging inside /Applications keeps the final step a same-volume rename, which
# is atomic: there is no instant where the path exists but is half-written.
mkdir -p "$DEST_DIR"
if [[ -e "$STAGE" ]]; then
    chflags -R nouchg "$STAGE" 2>/dev/null || true
    rm -rf "$STAGE"
fi
note "staging"
ditto "$SRC" "$STAGE"
xattr -dr com.apple.quarantine "$STAGE" 2>/dev/null || true

# --- retire the old bundle: rename, never remove --------------------------------
if [[ -e "$DEST" ]]; then
    mkdir -p "$ATTIC"
    OLD_VERSION="$(bundle_version "$DEST")"
    # Second resolution is not enough on its own: two installs inside one second
    # collide, and `mv` onto an existing directory moves the bundle *inside* it
    # rather than failing.
    RETIRED_BASE="$ATTIC/Hyperpanes-$OLD_VERSION-$(date +%Y%m%d%H%M%S)"
    RETIRED="$RETIRED_BASE.app"
    n=1
    while [[ -e "$RETIRED" ]]; do
        RETIRED="$RETIRED_BASE-$n.app"
        n=$((n + 1))
    done
    chflags -R nouchg "$DEST"
    mv "$DEST" "$RETIRED"
    note "retired old $OLD_VERSION -> $RETIRED (kept, not deleted: a live daemon may still be running it)"
fi

# --- swap in ---------------------------------------------------------------------
mv "$STAGE" "$DEST"
note "installed $VERSION at $DEST"

# --- lock it ----------------------------------------------------------------------
# The guardrail. Undone only by this script, on the next install.
chflags -R uchg "$DEST"
note "locked (chflags uchg): rm/ditto/cp/rsync/mv against $DEST now fail with EPERM"

# --- did anything die? --------------------------------------------------------------
LOST=""
for pid in $BEFORE_DAEMONS; do
    kill -0 "$pid" 2>/dev/null || LOST="$LOST $pid"
done
if [[ -n "$LOST" ]]; then
    echo "ERROR: session daemon(s)$LOST died during the install — running panes were lost." >&2
    echo "       That is a bug in this script; the whole point of it is that they survive." >&2
    exit 1
fi
[[ -n "$BEFORE_DAEMONS" ]] && note "session daemon(s) still alive: $(echo "$BEFORE_DAEMONS" | tr '\n' ' ')— panes intact"

# --- prune the attic ---------------------------------------------------------------
# Only bundles nothing is executing, and never the newest few — an old daemon can
# outlive several installs, and a bundle deleted out from under it is the exact
# accident this script exists to prevent.
if [[ -d "$ATTIC" ]]; then
    # Newest first, so the "keep the newest few" rule is just a counter.
    # (No mapfile: stock macOS still ships bash 3.2.)
    retired=()
    while IFS= read -r line; do retired+=("$line"); done \
        < <(/bin/ls -1d "$ATTIC"/*.app 2>/dev/null | sort -r || true)
    i=0
    for r in ${retired+"${retired[@]}"}; do
        i=$((i + 1))
        (( i <= KEEP_RETIRED )) && continue
        if pgrep -f -- "$r/" >/dev/null 2>&1; then
            note "attic: $r still has a live process — kept"
            continue
        fi
        chflags -R nouchg "$r" 2>/dev/null || true
        rm -rf "$r"
        note "attic: pruned $r (nothing running from it)"
    done
fi

cat <<EOF

Done. $VERSION is installed and locked.

The running session daemon still executes the PREVIOUS build from the attic, and
your panes are untouched. It picks up this build only when it is deliberately
restarted — and restarting it kills every program running in a pane, so that is
your call, not this script's:

  pkill -f -- '--session-daemon'    # deliberate: ends every running pane process
EOF
