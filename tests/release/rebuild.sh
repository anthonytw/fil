#!/usr/bin/env bash
# The rebuild half of `just release-verify`. It runs INSIDE the Linux VM and is piped there by
# verify.sh. The COMPARISON with the release is the verifier's own (verify.sh runs on the owner's
# machine); the BUILD RECIPE is not: build-binary.sh, package.sh and Cross.toml's image pins come
# from the commit that is rebuilt. That is acceptable because release-verify has first checked that
# the commit is on main (reviewed code), and because a recipe that built something else would not
# reproduce what CI attested for that same commit.
#
#   bash -s -- REPO_URL COMMIT WORKDIR_UNDER_HOME < rebuild.sh
#
# It clones REPO_URL, checks out the COMMIT (a SHA: never a name that can move), builds both Linux
# binaries in the digest-pinned `cross` images, takes the macOS binary from
# WORKDIR/in/universal-apple-darwin/fil (sent by verify.sh: it comes from the release, so the
# macOS binary is the one input not rebuilt) and assembles the four archives. The result is in
# WORKDIR/out. Written for Ubuntu on aarch64 with Docker, rustup and amd64 emulation (Rosetta:
# docs/m4/release.md section 9.3).
set -euo pipefail

repo=$1 ref=$2
# The third argument is relative to $HOME (the VM user's), so the caller need not expand it.
work=$HOME/$3
export PATH="$HOME/.cargo/bin:$PATH"
log() { echo "rebuild: $*" >&2; }
die() { log "$*"; exit 1; }

# ---- amd64 containers must run, and run rustc (qemu-user 8.2 segfaults on rustc at startup)
docker run --rm --platform linux/amd64 busybox:stable true 2> /dev/null \
  || die "Docker cannot run linux/amd64 images in this VM: enable Rosetta (docs/m4/release.md section 9.3)"

rm -rf "$work/src" "$work/out" "$work/bins" "$work/target"
mkdir -p "$work"
git clone -q "$repo" "$work/src"
git -C "$work/src" checkout -q --detach "$ref"
cd "$work/src"
chan=$(sed -n 's/^channel[ ]*=[ ]*"\(.*\)"/\1/p' rust-toolchain.toml | head -n 1)
[ -n "$chan" ] || die "no channel in rust-toolchain.toml"

# ---- tools, at the versions the workflow pins (rustup toolchain: the host's for cargo vendor, and
# the x86_64 one that `cross` mounts into the amd64 image)
rustup show active-toolchain > /dev/null 2>&1 || rustup toolchain install
default_before=$(rustup default 2> /dev/null || true)
x86tc=$chan-x86_64-unknown-linux-gnu
rustup toolchain list | grep -q "^$x86tc" \
  || rustup toolchain install "$x86tc" --profile minimal --force-non-host -t x86_64-unknown-linux-musl,aarch64-unknown-linux-musl
# installing a first toolchain makes it the default; put back what was there
[ -n "$default_before" ] || rustup default none > /dev/null 2>&1 || true
# ---- the pinned toolchain must RUN in the pinned image under this VM's emulation. qemu-user (Ubuntu
# 24.04's 8.2) runs `true`, then crashes rustc at startup ("QEMU internal SIGSEGV") or hangs it;
# Rosetta runs it. Hence the time limit.
img=$(awk '$0 == "[target.x86_64-unknown-linux-musl]" { p = 1; next } /^\[/ { p = 0 } p && /^image/ { v = $0; sub(/^image[ ]*=[ ]*"/, "", v); sub(/".*$/, "", v); print v; exit }' Cross.toml)
x86root=$(dirname "$(dirname "$(rustup which --toolchain "$x86tc" rustc)")")
cid=$(docker run -d --platform linux/amd64 -v "$x86root:/rust:ro" "$img" /rust/bin/rustc --version)
code=$(timeout 120 docker wait "$cid" 2> /dev/null || echo timeout)
docker rm -f "$cid" > /dev/null 2>&1 || true
[ "$code" = 0 ] || die "rustc does not run in the amd64 image under this VM's emulation ($code; qemu-user crashes or hangs it): enable Rosetta (docs/m4/release.md section 9.3)"
cross --version 2> /dev/null | grep -q '^cross 0.2.5' || cargo install --locked cross --version 0.2.5
command -v just > /dev/null || cargo install --locked just --version 1.59.0

# ---- build and package
export CARGO_TARGET_DIR=$work/target
export SOURCE_DATE_EPOCH
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct HEAD)
for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
  tests/release/build-binary.sh "$t" "$work/bins"
done
mkdir -p "$work/bins/universal-apple-darwin"
cp "$work/in/universal-apple-darwin/fil" "$work/bins/universal-apple-darwin/fil"
extra=()
[ -d "$work/in/extra" ] && extra=(--extra "$work/in/extra")
tests/release/package.sh --bins "$work/bins" --out "$work/out" ${extra[@]+"${extra[@]}"}
log "done: $work/out"
