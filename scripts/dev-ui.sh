#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# GUI dev cycle WITHOUT the ~400k-line Slint build (~4 min / ~6 GiB per .slint
# edit). SLINT_LIVE_PREVIEW=1 + the `live-preview` feature make slint-build emit
# small interpreter-backed stubs with the identical API (~2 s / <1 GiB), and the
# running app hot-reloads .slint saves (properties/models/callbacks preserved).
#
#   scripts/dev-ui.sh build [cargo args…]   # compile window + GUI logic (stubs)
#   scripts/dev-ui.sh run   [moltd args…]   # build + start moltd, live UI reload
#
# Uses its own cache dir (target/dev-ui) so the different feature set never
# invalidates the normal build's cache. First build fills that cache once.
# Dev-only: the release build stays fully compiled (scripts/build-release.sh).
set -euo pipefail
cd "$(dirname "$0")/.."

: "${CARGO_TARGET_DIR:=target/dev-ui}"
export CARGO_TARGET_DIR
export SLINT_LIVE_PREVIEW=1

mode="${1:-build}"
shift || true
case "$mode" in
    build) exec cargo build -p molt-ui-window -p molt-ui --features molt-ui/live-preview "$@" ;;
    run)   exec cargo run -p molt-app --features live-preview -- "$@" ;;
    *)     echo "usage: scripts/dev-ui.sh [build|run] [args…]" >&2; exit 2 ;;
esac
