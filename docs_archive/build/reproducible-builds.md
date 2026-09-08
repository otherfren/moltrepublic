# Reproducible builds

MoltRepublic's Linux release tarball is **byte-reproducible**: anyone with the
source tree at a tagged commit can independently rebuild the artifact and verify
that its SHA-256 matches the one published in the release notes. You do not have
to trust the upload — you can confirm the binary you are about to run
corresponds exactly to the source you (or someone you trust) audited.

## The promise (Linux)

For any tagged release `vX.Y.Z`, the file
`dist/moltrepublic-linux-x86_64.tar.zst` produced by `bash
scripts/build-release.sh` from the source tree at that tag has a stable SHA-256,
**provided** the build host falls inside the reproducibility envelope below.
Build the same tag twice on the same host, or on two hosts that match the
envelope, and you get bit-identical tarballs.

## How to verify

```bash
git clone https://github.com/<TBD>/moltrepublic
cd moltrepublic
git checkout vX.Y.Z            # the tag you want to verify
bash scripts/build-release.sh
sha256sum dist/moltrepublic-linux-x86_64.tar.zst
```

Compare the printed hash against the SHA-256 in the release notes for `vX.Y.Z`.
If they match, the published binary corresponds exactly to the source at this
tag. You do not need to run the binary you just built — the point is the hash
comparison.

If the hashes do **not** match, stop and do not run either binary. Open an issue
with both hashes and the output of `uname -a`, `cargo --version`,
`rustc --version`, `tar --version`, and `zstd --version`.

## What is in the reproducibility envelope

These inputs determine the artifact bytes:

- **Source tree** at the tagged commit.
- **`Cargo.lock`** at the tagged commit. The release script invokes
  `cargo build --locked`, which refuses to build if `Cargo.lock` and
  `Cargo.toml` have drifted.
- **Rust toolchain version**, pinned exactly by `rust-toolchain.toml`
  (`channel = "1.95.0"`), so the toolchain is part of the source tree rather
  than a release-note footnote.
- **The feature set**: the script builds `molt-app` with `--features
  embedded-tor` (release decision 2026-09-06: the published binary carries
  the in-process arti client and needs no system Tor; its
  `onion-service-client` feature is what reaches an onion relay - the live
  smoke test `crates/molt-net/tests/tor_embedded_smoke.rs` proves both a
  clearnet and an onion relay through it). Measured 2026-09-06 on the
  15.9 GiB box with 9 GiB swap: the whole script took 29m34s, of which the
  release-profile window rustc (`molt-ui-window`, opt-level 3, one codegen
  unit, thin LTO) ~24 min with a sampled peak of 13.97 GiB RSS and 8 of the
  9 GiB swap in use at the peak - do not run it beside anything else, and
  add swap before a bigger window. **2026-09-08 (v0.0.4): the same step was
  OOM-killed twice with 15.9 GiB + 10 GiB swap (two GUI dev nodes, an
  editor and a browser held ~6 GiB), once beside a second rustc and once
  alone - the script now builds `-j 1` - and completed with 8 GiB of swap
  added (15.9 + 18 GiB; swap use peaked above 14 GiB), 35 min end to end.**
  The stripped `moltd` is 90.2 MiB, the tarball 21.9 MiB (22.9 MiB at
  v0.0.4). That pulls arti's
  `tor-dirmgr → rusqlite → libsqlite3-sys`, i.e. a bundled C SQLite compiled
  by `cc` at build time - the C compiler and its flags therefore join the
  envelope (see below).
- **Deterministic compile flags** from `[profile.release]`
  (`codegen-units = 1`, `lto = "thin"`, `panic = "abort"`,
  `overflow-checks = true`) and `--remap-path-prefix` (set by the script), so no
  absolute `/home/<user>/...` paths are baked into the output.
- **`SOURCE_DATE_EPOCH`**, derived in the script from `git log -1 --pretty=%ct`
  (the commit's author timestamp), so every builder uses the same build clock.
- **`LC_ALL=C` and `TZ=UTC`**, set by the script so build-script output is
  stable.

## Known sources of drift (NOT in the envelope)

These vary across machines and can cause hash mismatches at the same commit.
None affect the *correctness* of the binary, only its byte representation.

- **The C compiler behind `libsqlite3-sys`** (since the embedded-Tor release):
  `cc`'s version and default flags shape the bundled SQLite object code. Two
  hosts with different `cc` versions can differ here even with an identical
  Rust toolchain; record `cc --version` beside the other versions in an issue.

- **Host glibc version.** The binary dynamically links the host's glibc; build
  on the same Ubuntu LTS the official build used (named in the release notes). A
  static musl build that removes this dependency is a later option.
- **Host `tar` / `zstd` versions.** The script passes deterministic flags
  (`--sort=name`, `--owner=0`, `--mtime=@…`, fixed compression level), but the
  tools themselves are not in the source tree. Modern GNU `tar` (≥ 1.30) and
  `zstd` (≥ 1.5) produce identical output for these flags.
- **`binutils` `strip` version.** The script runs `strip --strip-unneeded`;
  behavior varies subtly across `binutils`. If a rebuild fails reproducibility,
  this is suspect number one — drop the `strip` call in a local copy and
  re-compare.
- **CPU microarchitecture.** `-C target-cpu=native` is intentionally absent, so
  the artifact is portable across x86_64 CPUs and microarchitecture differences
  do not affect the bytes.

## Non-Linux hosts

The script assumes GNU `tar`. macOS ships `bsdtar`; use `gtar` (Homebrew) or
build inside a Linux container. Windows is out of scope — build under WSL.

## Disclaimer

MoltRepublic is pre-alpha and has not undergone external cryptographic audit.
