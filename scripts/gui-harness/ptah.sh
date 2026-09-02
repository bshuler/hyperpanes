#!/usr/bin/env bash
# Hyperpanes — drive the GUI harness on a remote Linux box (default: ptah).
#
#   scripts/gui-harness/ptah.sh sync    # push the working tree over
#   scripts/gui-harness/ptah.sh image   # build the harness image
#   scripts/gui-harness/ptah.sh build   # cargo build --release inside it
#   scripts/gui-harness/ptah.sh test    # run gui-test.sh inside it
#   scripts/gui-harness/ptah.sh shot    # photograph the window, fetch the PNGs
#   scripts/gui-harness/ptah.sh all     # all four, in order
#
# Why remote and not this Mac: the harness drives a GUI with synthetic mouse and
# keyboard events. On the local machine those land on whatever the human is
# doing. On a box with no seat they land nowhere but the Xvfb display the
# container just created, which is the entire point — see
# docs/live-session-safety.md.
#
# ptah is shared. Take a lease before a long run and release it after:
#   ssh ptah '~/.local/bin/ptah-lease acquire hyperpanes shared "gui harness"'
#   ssh ptah '~/.local/bin/ptah-lease release <token>'
set -euo pipefail

HOST="${HYPERPANES_HARNESS_HOST:-ptah}"
REMOTE="${HYPERPANES_HARNESS_DIR:-/home/ubuntu/hyperpanes-harness}"
IMAGE=hyperpanes-harness
REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"

# Named volumes, so a rebuild is minutes rather than the whole dependency tree
# (Slint comes from git and is not cheap).
CARGO_VOL=hyperpanes-cargo
TARGET_VOL=hyperpanes-target

note() { echo "==> $*"; }
rsh() { ssh -o BatchMode=yes "$HOST" "$@"; }

# --workdir /work with the tree bind-mounted, target/ on a volume so it survives.
docker_run() {
    rsh docker run --rm \
        -v "$REMOTE:/work" \
        -v "$CARGO_VOL:/usr/local/cargo/registry" \
        -v "$TARGET_VOL:/work/rs/crates/app/target" \
        -w /work "$IMAGE" "$@"
}

cmd_sync() {
    note "syncing $REPO_ROOT -> $HOST:$REMOTE"
    rsh "mkdir -p '$REMOTE'"
    rsync -a --delete \
        --exclude '.git/' \
        --exclude 'target/' \
        --exclude 'rs/packaging/out/' \
        --exclude 'node_modules/' \
        "$REPO_ROOT/" "$HOST:$REMOTE/"
}

cmd_image() {
    note "building image $IMAGE on $HOST"
    rsh "docker build -t '$IMAGE' -f '$REMOTE/scripts/gui-harness/Dockerfile' '$REMOTE/scripts/gui-harness'"
}

cmd_build() {
    note "cargo build --release (app crate) inside $IMAGE"
    docker_run bash /work/scripts/gui-harness/in-container.sh build
}

cmd_test() {
    note "running gui-test.sh inside $IMAGE"
    docker_run bash /work/scripts/gui-harness/in-container.sh test
}

# The container writes into the bind-mounted tree, so the PNGs come back over the
# same path the sources went out on. They land in $SHOT_DIR locally (default
# ./shots, which is gitignored) — a rendering change is argued from these.
cmd_shot() {
    note "photographing the window inside $IMAGE"
    docker_run bash /work/scripts/gui-harness/in-container.sh shot /work/shots
    local dest="${HYPERPANES_SHOT_DIR:-$REPO_ROOT/shots}"
    mkdir -p "$dest"
    note "fetching PNGs -> $dest"
    rsync -a "$HOST:$REMOTE/shots/" "$dest/"
    ls -1 "$dest"
}

case "${1:-all}" in
    sync)  cmd_sync ;;
    image) cmd_image ;;
    build) cmd_build ;;
    test)  cmd_test ;;
    shot)  cmd_shot ;;
    all)   cmd_sync; cmd_image; cmd_build; cmd_test ;;
    *) echo "usage: ptah.sh [sync|image|build|test|shot|all]" >&2; exit 2 ;;
esac
