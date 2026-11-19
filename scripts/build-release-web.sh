#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Deterministic build of the browser GUI bundle (the page, its glue and
# the wasm module that `[web] dir` serves). Its own entry point: the
# native script stays free of wasm tools and of a second window-scale
# compile. See docs_archive/build/reproducible-builds.md, "The web bundle".
#
#   scripts/build-release-web.sh               # build and pack
#   scripts/build-release-web.sh --check-only  # tools and versions, no compile
set -euo pipefail

# Every name the web crate decides, in one place.
WEB_CRATE=molt-web                      # cargo package
PAGE_DIR=crates/molt-web/page          # the hand-written page around the window
PAGE_FILES=(index.html page.js page.css)  # with the two bound files: the door's list
WASM_NAME=molt_web                      # molt_web.wasm -> molt_web.js + molt_web_bg.wasm
WASM_TARGET=wasm32-unknown-unknown
PROFILE=release-web
FONT_DIR=crates/molt-ui-window/ui/assets/fonts
FONT_LICENCES=(DejaVuSansMono.LICENSE Twemoji.Mozilla.LICENSE)
TARBALL=dist/moltrepublic-web.tar.zst
PEAK_GIB=26                             # the window rustc, 16 codegen units

case "${1:-}" in
  "") check_only=0 ;;
  --check-only) check_only=1 ;;
  *) echo "usage: scripts/build-release-web.sh [--check-only]" >&2; exit 2 ;;
esac
[ $# -le 1 ] || { echo "usage: scripts/build-release-web.sh [--check-only]" >&2; exit 2; }

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"
TARGET_DIR="$(realpath -m -- "${CARGO_TARGET_DIR:-$REPO_ROOT/target}")"
export CARGO_TARGET_DIR="$TARGET_DIR"

failed=0
ok()   { printf 'ok    %s\n' "$1"; }
warn() { printf 'warn  %s\n' "$1"; }
bad()  { printf 'FAIL  %s\n' "$1"; failed=1; }

# --- tools and versions ------------------------------------------------------
for tool in cargo rustc tar zstd sha256sum; do
  command -v "$tool" >/dev/null || bad "$tool not found"
done

want_rustc="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)"
have_rustc="$(rustc --version 2>/dev/null | awk '{print $2}')" || have_rustc=
if [ -n "$want_rustc" ] && [ "$have_rustc" = "$want_rustc" ]; then
  ok "rustc $have_rustc"
else
  bad "rustc ${have_rustc:-none}, rust-toolchain.toml has ${want_rustc:-none}"
fi

libdir="$(rustc --print target-libdir --target "$WASM_TARGET" 2>/dev/null)" || libdir=
if [ -n "$libdir" ] && [ -d "$libdir" ]; then
  ok "target $WASM_TARGET"
else
  bad "target $WASM_TARGET not installed"
fi

# The CLI must be the wasm-bindgen the module was compiled against.
want_cli="$(awk '$0 == "name = \"wasm-bindgen\"" { getline; gsub(/[^0-9.]/, ""); print; exit }' Cargo.lock)"
bindgen="${WASM_BINDGEN:-$(command -v wasm-bindgen || true)}"
if [ -z "$bindgen" ] && [ -x "${CARGO_HOME:-$HOME/.cargo}/bin/wasm-bindgen" ]; then
  bindgen="${CARGO_HOME:-$HOME/.cargo}/bin/wasm-bindgen"
fi
have_cli=
[ -z "$bindgen" ] || have_cli="$("$bindgen" --version 2>/dev/null | awk '{print $2}')" || have_cli=
if [ -z "$want_cli" ]; then
  bad "wasm-bindgen is not in Cargo.lock"
elif [ "$have_cli" = "$want_cli" ]; then
  ok "wasm-bindgen $have_cli"
else
  bad "wasm-bindgen ${have_cli:-not found}, Cargo.lock has $want_cli"
  echo "      cargo install --locked wasm-bindgen-cli --version =$want_cli"
fi

if tar --version 2>/dev/null | grep -q 'GNU tar'; then ok "GNU tar"; else bad "tar is not GNU tar"; fi

# --- the tree ----------------------------------------------------------------
if grep -q "^\[profile\.$PROFILE\]" Cargo.toml; then ok "profile $PROFILE"; else bad "no [profile.$PROFILE] in Cargo.toml"; fi
inputs=(COPYING)
for f in "${PAGE_FILES[@]}"; do inputs+=("$PAGE_DIR/$f"); done
for f in "${FONT_LICENCES[@]}"; do inputs+=("$FONT_DIR/$f"); done
missing=0
for f in "${inputs[@]}"; do
  [ -f "$f" ] || { bad "missing $f"; missing=1; }
done
[ "$missing" -ne 0 ] || ok "page and licences"

# --- the machine -------------------------------------------------------------
if [ -r /proc/meminfo ]; then
  have_gib="$(awk '/^(MemTotal|SwapTotal):/ { kb += $2 } END { printf "%d", kb / 1048576 }' /proc/meminfo)"
  if [ "$have_gib" -ge "$PEAK_GIB" ]; then
    ok "memory $have_gib GiB with swap"
  else
    warn "memory $have_gib GiB with swap, the window compile peaks at $PEAK_GIB"
  fi
fi

# A cargo or rustc already on this target dir: two window-scale compiles
# side by side are the documented OOM kill, and cargo's build lock does not
# serialize across profile directories.
live_builds() {
  local proc pid comm cmd cwd env_dir
  for proc in /proc/[0-9]*; do
    pid="${proc#/proc/}"
    comm="$(cat "$proc/comm" 2>/dev/null)" || continue
    case "$comm" in cargo|rustc|rustdoc|clippy-driver) ;; *) continue ;; esac
    cmd="$(tr '\0' ' ' <"$proc/cmdline" 2>/dev/null)" || continue
    cwd="$(readlink "$proc/cwd" 2>/dev/null)" || cwd=
    env_dir="$(tr '\0' '\n' <"$proc/environ" 2>/dev/null | sed -n 's/^CARGO_TARGET_DIR=//p' | head -n 1)" || env_dir=
    if [ -n "$env_dir" ]; then
      case "$env_dir" in /*) ;; *) env_dir="$cwd/$env_dir" ;; esac
      env_dir="$(realpath -m -- "$env_dir")"
    fi
    if [ -n "$env_dir" ] && case "$env_dir/" in "$TARGET_DIR/"*) true ;; *) false ;; esac; then
      echo "$pid"
    elif case "$cmd" in *"$TARGET_DIR/"*|*"$TARGET_DIR "*) true ;; *) false ;; esac; then
      echo "$pid"
    elif [ -z "$env_dir" ] && [ "$TARGET_DIR" = "$REPO_ROOT/target" ] \
        && case "$cwd/" in "$REPO_ROOT/"*) true ;; *) false ;; esac \
        && case "$cmd" in *--target-dir*) false ;; *) true ;; esac; then
      echo "$pid"
    fi
  done
}
busy="$(live_builds | tr '\n' ' ')"
busy="${busy% }"
if [ -z "$busy" ]; then
  ok "target dir idle"
elif [ "$check_only" -eq 1 ]; then
  warn "target dir busy: pid $busy"
else
  bad "target dir busy: pid $busy"
fi

[ "$failed" -eq 0 ] || exit 1
[ "$check_only" -eq 0 ] || exit 0

# --- build -------------------------------------------------------------------
# Pin the build clock to the commit's author date - stable across machines
# and across re-runs at the same commit. Override by exporting
# SOURCE_DATE_EPOCH.
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --pretty=%ct)}"

# Path remapping: no /home/<user>/... baked into the module. The last
# matching prefix wins, so a target dir outside the tree reads the same as
# the default one.
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="${RUSTFLAGS:-} \
  --remap-path-prefix=$REPO_ROOT=/build/moltrepublic \
  --remap-path-prefix=$cargo_home/registry=/build/cargo-registry \
  --remap-path-prefix=$cargo_home/git=/build/cargo-git \
  --remap-path-prefix=$TARGET_DIR=/build/moltrepublic/target"

# Locale and timezone pinned so build-script output is stable.
export LC_ALL=C
export TZ=UTC

# ONE job: the window module's rustc alone needs about $PEAK_GIB GiB of
# memory and swap for this target.
cargo build -j 1 -p "$WEB_CRATE" --target "$WASM_TARGET" --profile "$PROFILE" --locked

# The module keeps its name section in the target dir (the profile does not
# strip); the bundle's copy loses it here, with the producers section, in
# the tool whose version Cargo.lock pins.
stage="$TARGET_DIR/$WASM_TARGET/$PROFILE/web-bundle"
rm -rf "$stage"
mkdir -p "$stage"
"$bindgen" --target web --no-typescript \
  --remove-name-section --remove-producers-section \
  --out-dir "$stage" --out-name "$WASM_NAME" \
  "$TARGET_DIR/$WASM_TARGET/$PROFILE/$WASM_NAME.wasm"
for f in "${PAGE_FILES[@]}"; do cp "$PAGE_DIR/$f" "$stage/$f"; done

# Pack flat (it unpacks into `web/` beside the config) with deterministic
# flags.
mkdir -p dist
tar \
  --sort=name \
  --owner=0 --group=0 --numeric-owner \
  --mtime="@${SOURCE_DATE_EPOCH}" \
  --no-acls --no-selinux --no-xattrs --mode=u=rw,go=r \
  -cf - -C "$stage" "${PAGE_FILES[@]}" "$WASM_NAME.js" "${WASM_NAME}_bg.wasm" \
  -C "$REPO_ROOT" COPYING \
  -C "$REPO_ROOT/$FONT_DIR" "${FONT_LICENCES[@]}" \
| zstd -19 -f --no-progress -o "$TARBALL"

sha256sum "$TARBALL"
