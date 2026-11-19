#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Tests for scripts/build-release-web.sh. Hermetic: a throwaway git repo
# with stub cargo / rustc / wasm-bindgen on PATH, so nothing is compiled.
#
#   bash scripts/test-build-release-web.sh
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")" && pwd)/build-release-web.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

LOCK_VERSION=0.2.126
failed=0
pass() { printf 'ok    %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1"; failed=1; }
# expect <name> <condition...>
expect() { local name="$1"; shift; if "$@"; then pass "$name"; else fail "$name"; fi; }
has() { grep -qF -- "$2" "$1"; }
lacks() { ! grep -qF -- "$2" "$1"; }
refuses() { ! "$@"; }

# --- the fixture repo -------------------------------------------------------
new_repo() {
  local repo="$1"
  mkdir -p "$repo/scripts" "$repo/crates/molt-web/page" "$repo/crates/molt-ui-window/ui/assets/fonts"
  cp "$SCRIPT" "$repo/scripts/build-release-web.sh"
  printf '[toolchain]\nchannel = "1.95.0"\ntargets = ["wasm32-unknown-unknown"]\n' >"$repo/rust-toolchain.toml"
  printf '[workspace]\n\n[profile.release-web]\ninherits = "release"\n' >"$repo/Cargo.toml"
  printf '[[package]]\nname = "wasm-bindgen"\nversion = "%s"\n\n[[package]]\nname = "wasm-bindgen-macro"\nversion = "9.9.9"\n' \
    "$LOCK_VERSION" >"$repo/Cargo.lock"
  echo licence >"$repo/COPYING"
  echo page >"$repo/crates/molt-web/page/index.html"
  echo script >"$repo/crates/molt-web/page/page.js"
  echo style >"$repo/crates/molt-web/page/page.css"
  echo dejavu >"$repo/crates/molt-ui-window/ui/assets/fonts/DejaVuSansMono.LICENSE"
  echo twemoji >"$repo/crates/molt-ui-window/ui/assets/fonts/Twemoji.Mozilla.LICENSE"
  git -C "$repo" init -q
  git -C "$repo" add -A
  GIT_AUTHOR_DATE='2026-01-02T03:04:05Z' GIT_COMMITTER_DATE='2026-01-02T03:04:05Z' \
    git -C "$repo" -c user.name=t -c user.email=t@example.invalid commit -q -m fixture
}

# --- the stub tools ---------------------------------------------------------
# new_bin <dir> <wasm-bindgen version | none>
new_bin() {
  local bin="$1" cli="$2"
  mkdir -p "$bin"
  cat >"$bin/cargo" <<'EOF'
#!/usr/bin/env bash
echo "$*" >>"$STUB_LOG/cargo.log"
echo "$RUSTFLAGS" >"$STUB_LOG/rustflags"
if [ "$1" = build ]; then
  out="${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/release-web"
  mkdir -p "$out"
  echo module >"$out/molt_web.wasm"
fi
EOF
  # /bin/bash directly: the kernel names the process after the script
  # only while no second exec follows, and the busy check goes by that name
  cat >"$bin/rustc" <<'EOF'
#!/bin/bash
case "$*" in
  --version) echo "rustc ${STUB_RUSTC:-1.95.0} (stub 2026-01-01)" ;;
  *target-libdir*) echo "${STUB_LIBDIR:-/}" ;;
  # stands in for a live compile: stays up until the test ends it
  --out-dir*) read -r _ <"$STUB_LOG/hold" ;;
esac
EOF
  if [ "$cli" != none ]; then
    cat >"$bin/wasm-bindgen" <<EOF
#!/usr/bin/env bash
if [ "\$1" = --version ]; then echo "wasm-bindgen $cli"; exit 0; fi
echo "\$*" >>"\$STUB_LOG/bindgen.log"
out=; name=
while [ \$# -gt 0 ]; do
  case "\$1" in
    --out-dir) out="\$2"; shift ;;
    --out-name) name="\$2"; shift ;;
  esac
  shift
done
echo glue >"\$out/\$name.js"
echo bound >"\$out/\${name}_bg.wasm"
EOF
  fi
  chmod +x "$bin"/*
}

# run <case dir> <args...>: the script in the case's repo, stubs first on PATH
run() {
  local dir="$1"; shift
  ( cd "$dir/repo" \
    && env -u CARGO_TARGET_DIR -u RUSTFLAGS -u SOURCE_DATE_EPOCH -u WASM_BINDGEN \
       PATH="$dir/bin:$PATH" CARGO_HOME="$dir/no-cargo-home" STUB_LOG="$dir/log" \
       ${STUB_RUSTC:+STUB_RUSTC="$STUB_RUSTC"} ${STUB_LIBDIR:+STUB_LIBDIR="$STUB_LIBDIR"} \
       bash scripts/build-release-web.sh "$@" ) >"$dir/out" 2>&1
}

new_case() {
  local dir="$WORK/$1"
  mkdir -p "$dir/log"
  : >"$dir/log/cargo.log"
  new_repo "$dir/repo"
  new_bin "$dir/bin" "${2:-$LOCK_VERSION}"
  echo "$dir"
}

# --- check-only passes and compiles nothing ---------------------------------
c="$(new_case check)"
expect "check-only: exit 0 on a complete host" run "$c" --check-only
expect "check-only: no cargo build" lacks "$c/log/cargo.log" build
expect "check-only: names the pinned CLI" has "$c/out" "wasm-bindgen $LOCK_VERSION"
expect "check-only: no tarball" test ! -e "$c/repo/dist/moltrepublic-web.tar.zst"

# --- a CLI that is not Cargo.lock's fails with the install line --------------
c="$(new_case mismatch 0.2.129)"
expect "CLI mismatch: check-only fails" refuses run "$c" --check-only
expect "CLI mismatch: prints the install line" has "$c/out" \
  "cargo install --locked wasm-bindgen-cli --version =$LOCK_VERSION"
expect "CLI mismatch: the build refuses too" refuses run "$c"
expect "CLI mismatch: no cargo build" lacks "$c/log/cargo.log" build

c="$(new_case missing none)"
# shellcheck disable=SC2030,SC2031
( PATH="$c/bin:/usr/bin:/bin"; export PATH; ! run "$c" --check-only ) \
  && pass "CLI missing: check-only fails" || fail "CLI missing: check-only fails"
expect "CLI missing: prints the install line" has "$c/out" \
  "cargo install --locked wasm-bindgen-cli --version =$LOCK_VERSION"

# --- the other tool checks ---------------------------------------------------
c="$(new_case rustc)"
STUB_RUSTC=1.94.0
expect "rustc off the pinned channel: fails" refuses run "$c" --check-only
unset STUB_RUSTC
expect "rustc off the pinned channel: named" has "$c/out" "1.94.0"

c="$(new_case target)"
STUB_LIBDIR="$c/absent"
expect "wasm32 target missing: fails" refuses run "$c" --check-only
unset STUB_LIBDIR

for f in index.html page.js page.css; do
  c="$(new_case "page-$f")"
  rm "$c/repo/crates/molt-web/page/$f"
  expect "$f missing: fails" refuses run "$c" --check-only
  expect "$f missing: named" has "$c/out" "crates/molt-web/page/$f"
done

c="$(new_case profile)"
printf '[workspace]\n' >"$c/repo/Cargo.toml"
expect "profile missing: fails" refuses run "$c" --check-only

c="$(new_case usage)"
expect "unknown argument: fails" refuses run "$c" --frobnicate
expect "unknown argument: no cargo build" lacks "$c/log/cargo.log" build

# --- a live compiler on the target dir stops the build -----------------------
c="$(new_case busy)"
mkfifo "$c/log/hold"
STUB_LOG="$c/log" "$c/bin/rustc" --out-dir "$c/repo/target/debug/deps" &
busy_pid=$!
for _ in $(seq 50); do
  [ "$(cat "/proc/$busy_pid/comm" 2>/dev/null)" = rustc ] && break
  read -r -t 0.1 _ <><(:) || true
done
expect "busy target dir: the build refuses" refuses run "$c"
expect "busy target dir: no cargo build" lacks "$c/log/cargo.log" build
expect "busy target dir: names the pid" has "$c/out" "$busy_pid"
expect "busy target dir: check-only still passes" run "$c" --check-only
echo done >"$c/log/hold"
wait "$busy_pid"
expect "idle again: the build runs" run "$c"

# --- the build: commands, contents, determinism ------------------------------
c="$(new_case build)"
expect "build: exit 0" run "$c"
expect "build: one job, the web crate, the web profile, locked" has "$c/log/cargo.log" \
  "build -j 1 -p molt-web --target wasm32-unknown-unknown --profile release-web --locked"
expect "build: bindgen for the web, no typescript" has "$c/log/bindgen.log" "--target web --no-typescript"
expect "build: the repo path is remapped" has "$c/log/rustflags" \
  "--remap-path-prefix=$c/repo=/build/moltrepublic"
expect "build: the effective cargo home is remapped" has "$c/log/rustflags" \
  "--remap-path-prefix=$c/no-cargo-home/registry=/build/cargo-registry"
expect "build: its git checkouts too" has "$c/log/rustflags" \
  "--remap-path-prefix=$c/no-cargo-home/git=/build/cargo-git"
expect "build: prints the hash" has "$c/out" "dist/moltrepublic-web.tar.zst"

tarball="$c/repo/dist/moltrepublic-web.tar.zst"
want="COPYING
DejaVuSansMono.LICENSE
Twemoji.Mozilla.LICENSE
index.html
molt_web.js
molt_web_bg.wasm
page.css
page.js"
got="$(tar --zstd -tf "$tarball" | LC_ALL=C sort)"
expect "tarball: exactly the eight files" test "$got" = "$want"
listing="$(TZ=UTC tar --zstd --numeric-owner -tvf "$tarball")"
expect "tarball: owner 0/0, mode 644, the commit's time" bash -c \
  '! grep -qv "^-rw-r--r-- 0/0 .* 2026-01-02 03:04 " <<<"$1"' _ "$listing"
for pair in index.html:page page.js:script page.css:style; do
  expect "tarball: ${pair%%:*} is the crate's" bash -c \
    '[ "$(tar --zstd -xOf "$1" "$2")" = "$3" ]' _ "$tarball" "${pair%%:*}" "${pair##*:}"
done
expect "tarball: the bound module, not cargo's" bash -c \
  '[ "$(tar --zstd -xOf "$1" molt_web_bg.wasm)" = bound ]' _ "$tarball"

first="$(sha256sum <"$tarball")"
# a second builder: other file times, other modes, another umask, an
# external target dir
touch -d '2031-05-06' "$c/repo/COPYING" "$c/repo/crates/molt-web/page/page.js"
chmod 600 "$c/repo/COPYING"
( umask 077
  cd "$c/repo" && env -u RUSTFLAGS -u SOURCE_DATE_EPOCH -u WASM_BINDGEN \
    PATH="$c/bin:$PATH" CARGO_HOME="$c/no-cargo-home" STUB_LOG="$c/log" \
    CARGO_TARGET_DIR="$c/elsewhere" bash scripts/build-release-web.sh ) >"$c/out2" 2>&1 \
  && pass "rebuild: exit 0" || fail "rebuild: exit 0"
expect "rebuild: byte-identical tarball" test "$first" = "$(sha256sum <"$tarball")"
expect "rebuild: an external target dir is remapped" has "$c/log/rustflags" \
  "--remap-path-prefix=$c/elsewhere=/build/moltrepublic/target"
expect "build: the page stays out of the tree's root" test ! -e "$c/repo/molt_web.js"

# --- the documented install survives a umask of 002 --------------------------
# the door refuses a group-writable bundle directory, so every recipe
# pins the mode instead of trusting the caller's umask
DOCS_ROOT="$(dirname "$(dirname "$SCRIPT")")"
for doc in README.md docs_archive/build/reproducible-builds.md; do
  recipe="$(grep -E '^mkdir .*moltrepublic-web\.tar\.zst' "$DOCS_ROOT/$doc" | sed 's/ *#.*//')"
  inst="$WORK/install-${doc##*/}"
  mkdir -p "$inst/dist"
  cp "$tarball" "$inst/dist/" && cp "$tarball" "$inst/"
  ( umask 002; cd "$inst" && [ -n "$recipe" ] && eval "$recipe" ) >/dev/null 2>&1 || true
  expect "install ($doc): the bundle directory is not group-writable" \
    test "$(stat -c %a "$inst/web" 2>/dev/null)" = 755
  expect "install ($doc): the files are not group-writable" bash -c \
    '[ -d "$1" ] && [ -z "$(find "$1" -type f -perm /022)" ]' _ "$inst/web"
done

if [ "$failed" -ne 0 ]; then
  echo "FAILED"
  exit 1
fi
echo "all passed"
