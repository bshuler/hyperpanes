#!/usr/bin/env bash
# Entrypoint for work done inside the harness container.
#
# It exists so ptah.sh never has to send a quoted shell command through ssh and
# then through docker: two layers of re-parsing that silently dropped the quotes
# and ran `cargo` on the ssh host instead of in the container. Here the remote
# command is only ever bare words.
set -euo pipefail

case "${1:-}" in
    build)
        cd /work/rs/crates/app
        exec cargo build --release --locked
        ;;
    test)
        exec /work/scripts/gui-harness/gui-test.sh "${@:2}"
        ;;
    shot)
        exec /work/scripts/gui-harness/shot.sh "${@:2}"
        ;;
    shell)
        exec bash
        ;;
    *)
        echo "usage: in-container.sh [build|test|shot|shell]" >&2
        exit 2
        ;;
esac
