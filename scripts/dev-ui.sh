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
#   scripts/dev-ui.sh wasm  [cargo args…]   # type-check the browser build (molt-web)
#
# Uses its own cache dir (target/dev-ui) so the different feature set never
# invalidates the normal build's cache. First build fills that cache once.
# Dev-only: the release build stays fully compiled (scripts/build-release.sh).
#
# `wasm` is CHECK-only: the stubs type-check for wasm32 but cannot run in a
# browser (docs/web_gui.md §8.3). It compiles every wasm-cfg line of molt-ui
# and molt-web in seconds, in its own cache (target/dev-ui-wasm), and needs
# the wasm32-unknown-unknown target installed.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${CARGO_TARGET_DIR:=target/dev-ui}"
export CARGO_TARGET_DIR
export SLINT_LIVE_PREVIEW=1

mode="${1:-build}"
shift || true
if [ "$mode" = wasm ] && [ "$CARGO_TARGET_DIR" = target/dev-ui ]; then
    # the wasm graph has its own feature set: never share the native stub cache
    export CARGO_TARGET_DIR=target/dev-ui-wasm
fi
case "$mode" in
    build) exec cargo build -p molt-ui-window -p molt-ui --features molt-ui/live-preview "$@" ;;
    run)   exec cargo run -p molt-app --features live-preview -- "$@" ;;
    wasm)  exec cargo check -p molt-web --features live-preview --target wasm32-unknown-unknown "$@" ;;
    *)     echo "usage: scripts/dev-ui.sh [build|run|wasm] [args…]" >&2; exit 2 ;;
esac
