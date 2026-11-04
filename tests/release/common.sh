#!/usr/bin/env bash
# shellcheck disable=SC2034 # the variables are used by the scripts that source this file
# Shared by the release scripts in this directory (docs/m4/release.md). Source it; it sets nothing
# but functions and the variables below. Written for bash 3.2 (the Mac's /bin/bash) and GNU bash.
#
#   FIL_RELEASE_ROOT   the tree the scripts read (default: this checkout). The self-test points it
#                      at scratch copies to mutate one file at a time.

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
ROOT=${FIL_RELEASE_ROOT:-$REPO_ROOT}
RELEASE_DIR=$REPO_ROOT/tests/release

# The target names. `LINUX_TARGETS` are built by `cross`; the macOS pair is built natively and
# joined with lipo (docs/m4/release.md section 3).
LINUX_TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl"
DARWIN_TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
UNIVERSAL=universal-apple-darwin
# The archive label of each binary target (section 4.1).
label_of() {
  case $1 in
    x86_64-unknown-linux-musl) echo x86_64-linux-musl ;;
    aarch64-unknown-linux-musl) echo aarch64-linux-musl ;;
    "$UNIVERSAL") echo "$UNIVERSAL" ;;
    *) die "no archive label for $1" ;;
  esac
}
# The directory name of each Linux binary inside the remote bundle (section 4.3).
bundle_dir_of() {
  case $1 in
    x86_64-unknown-linux-musl) echo x86_64-linux ;;
    aarch64-unknown-linux-musl) echo aarch64-linux ;;
    *) die "no bundle directory for $1" ;;
  esac
}

die() {
  echo "release: $*" >&2
  exit 1
}
log() { echo "release: $*" >&2; }

# sha256 FILE: the hex digest, with whichever tool the machine has.
sha256() {
  if command -v sha256sum > /dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# sums_check DIR FILE: every line of FILE (inside DIR) is "<64 lowercase hex>  <name>", at least one
# line, and each named file there has that digest. Not `sha256sum -c`: some builds of it (macOS's
# /sbin/sha256sum) exit 0 for a file with no well-formed line at all.
sums_check() {
  local dir=$1 file=$2 h n count=0
  while IFS= read -r line || [ -n "$line" ]; do
    case $line in
      [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]"  "*) ;;
      *) return 1 ;;
    esac
    h=${line%%  *}
    n=${line#*  }
    case $n in "" | /* | .. | ../* | */../* | */..) return 1 ;; esac
    [ -f "$dir/$n" ] || return 1
    [ "$(sha256 "$dir/$n")" = "$h" ] || return 1
    count=$((count + 1))
  done < "$dir/$file"
  [ "$count" -gt 0 ]
}

# The one package version: [workspace.package] version in the root Cargo.toml (section 1).
cargo_version() {
  awk '
    /^\[workspace\.package\]/ { p = 1; next }
    /^\[/ { p = 0 }
    p && /^version[ ]*=/ { v = $0; sub(/^version[ ]*=[ ]*"/, "", v); sub(/".*$/, "", v); print v; exit }
  ' "$ROOT/Cargo.toml"
}
# rust-version of [workspace.package], and the channel of rust-toolchain.toml.
cargo_rust_version() {
  awk '
    /^\[workspace\.package\]/ { p = 1; next }
    /^\[/ { p = 0 }
    p && /^rust-version[ ]*=/ { v = $0; sub(/^rust-version[ ]*=[ ]*"/, "", v); sub(/".*$/, "", v); print v; exit }
  ' "$ROOT/Cargo.toml"
}
toolchain_channel() {
  sed -n 's/^channel[ ]*=[ ]*"\(.*\)"/\1/p' "$ROOT/rust-toolchain.toml" | head -n 1
}

# The wire this tree speaks (crates/fil-proto/src/limits.rs) and the last line of the golden's
# [history] (crates/fil-proto/tests/golden/wire.txt): the revision the golden ends at.
wire_major() { sed -n 's/^pub const PROTOCOL_MAJOR: u16 = \([0-9][0-9]*\);.*/\1/p' "$ROOT/crates/fil-proto/src/limits.rs"; }
wire_revision() { sed -n 's/^pub const PROTOCOL_REVISION: u16 = \([0-9][0-9]*\);.*/\1/p' "$ROOT/crates/fil-proto/src/limits.rs"; }
golden_last_revision() {
  awk '/^\[history\]/ { h = 1; next } h && /^[0-9]+ = / { last = $1 } END { print last }' "${1:-$ROOT/crates/fil-proto/tests/golden/wire.txt}"
}

# cross_image TARGET: the image Cross.toml pins for TARGET (the digest-pinned reference).
cross_image() {
  awk -v want="[target.$1]" '
    $0 == want { p = 1; next }
    /^\[/ { p = 0 }
    p && /^image[ ]*=/ { v = $0; sub(/^image[ ]*=[ ]*"/, "", v); sub(/".*$/, "", v); print v; exit }
  ' "$ROOT/Cross.toml"
}
# The image the archives are assembled in: the x86_64 image (section 4.4).
tools_image() { cross_image x86_64-unknown-linux-musl; }

# The committer time of REF (default HEAD): the SOURCE_DATE_EPOCH of a release (section 4.4).
source_date_epoch() { git -C "$ROOT" log -1 --format=%ct "${1:-HEAD}"; }

# The path remapping that keeps builder paths out of the binary. cargo's profile key for this,
# `trim-paths`, is not stable in the pinned cargo (see [profile.release] in Cargo.toml), so the
# release builds pass the same thing as rustc flags. Each root is mapped to a FIXED name, so that
# `just release-check` can also require that the remap was applied: a dependency's panic location is
# always in the binary, and it must read /cargo-home/registry/..., not the builder's CARGO_HOME.
REMAP_WORKSPACE=/build
REMAP_CARGO_HOME=/cargo-home
REMAP_RUSTUP_HOME=/rustup-home
# remap_flags WORKSPACE CARGO_HOME RUSTUP_HOME
remap_flags() {
  local p
  for p in "$1" "$2" "$3"; do
    case $p in
      *[[:space:]]*) die "path '$p' contains a space; RUSTFLAGS cannot carry it" ;;
    esac
  done
  printf '%s' "--remap-path-prefix=$1=$REMAP_WORKSPACE --remap-path-prefix=$2=$REMAP_CARGO_HOME --remap-path-prefix=$3=$REMAP_RUSTUP_HOME"
}
# Inside a `cross` container the roots are fixed: the project is /project, cargo's home /cargo, the
# mounted toolchain /rust.
remap_flags_cross() { remap_flags /project /cargo /rust; }

# A clean environment for a release build: nothing from the caller's shell may change the bytes.
# (cargo reads these on top of the profile.)
clean_build_env() {
  unset CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_RUSTFLAGS RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER \
    CARGO_PROFILE_RELEASE_LTO CARGO_PROFILE_RELEASE_CODEGEN_UNITS CARGO_PROFILE_RELEASE_STRIP \
    CARGO_PROFILE_RELEASE_DEBUG CARGO_INCREMENTAL
}
