#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Deterministic Linux release build for MoltRepublic.
# See docs_archive/build/reproducible-builds.md for the verification recipe and the
# documented reproducibility envelope.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

# Pin the build clock to the commit's author date — stable across machines and
# across re-runs at the same commit. Override by exporting SOURCE_DATE_EPOCH.
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --pretty=%ct)}"

# Path remapping → no /home/<user>/... baked into the compiled output.
export RUSTFLAGS="${RUSTFLAGS:-} \
  --remap-path-prefix=$REPO_ROOT=/build/moltrepublic \
  --remap-path-prefix=$HOME/.cargo/registry=/build/cargo-registry \
  --remap-path-prefix=$HOME/.cargo/git=/build/cargo-git"

# Locale and timezone pinned so build-script output is stable.
export LC_ALL=C
export TZ=UTC

# --locked refuses to build if Cargo.lock and Cargo.toml have drifted.
cargo build --workspace --release --locked

# Strip after the build → debug-info stays in target/debug for local use.
strip --strip-unneeded target/release/moltd

# Pack as a zstd-compressed tarball with deterministic flags.
mkdir -p dist
tar \
  --sort=name \
  --owner=0 --group=0 --numeric-owner \
  --mtime="@${SOURCE_DATE_EPOCH}" \
  --no-acls --no-selinux --no-xattrs \
  -cf - -C target/release moltd \
| zstd -19 --no-progress -o dist/moltrepublic-linux-x86_64.tar.zst

sha256sum dist/moltrepublic-linux-x86_64.tar.zst
