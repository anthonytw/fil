#!/usr/bin/env bash
# Build the `fil` release binary of one Linux target, in the digest-pinned image of Cross.toml, and
# put it at OUT/TARGET/fil. This is the one place that says how; release.yml's build jobs,
# `just release-build` and `just release-verify`'s rebuild all call it.
#
#   build-binary.sh TARGET OUT
#
# The environment: SOURCE_DATE_EPOCH (default: HEAD's committer time), CARGO_TARGET_DIR (default
# target/; the build uses its own subdirectory release-TARGET of it). The host needs `cross` 0.2.5 and Docker; on a non-x86_64 host the x86_64 toolchain too
# (rebuild.sh sets that up).
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

[ $# -eq 2 ] || die "usage: build-binary.sh TARGET OUT"
target=$1
out=$2
case " $LINUX_TARGETS " in *" $target "*) ;; *) die "not a Linux release target: $target" ;; esac

clean_build_env
# One target directory per target: the host-side artifacts (build scripts) of one `cross` image must
# never be reused by another. Sharing one directory between the two targets made the second build
# run the first one's build scripts, linked against a newer glibc than the second image has
# ("GLIBC_2.30 not found", found by the first release-verify run in the VM).
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}/release-$target
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(source_date_epoch)}
export SOURCE_DATE_EPOCH
# The cross images are linux/amd64 only (they hold the cross compilers); on an arm64 host Docker
# must run them emulated.
export DOCKER_DEFAULT_PLATFORM=linux/amd64
RUSTFLAGS=$(remap_flags_cross)
export RUSTFLAGS

cd "$ROOT"
log "cross $(cross --version | head -n 1 | tr '\n' ' ')for $target; image $(cross_image "$target")"
log "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH RUSTFLAGS=$RUSTFLAGS"
cross build --locked --release -p fil-shell --target "$target"

bin=$CARGO_TARGET_DIR/$target/release/fil
[ -f "$bin" ] || die "cross did not produce $bin"
mkdir -p "$out/$target"
cp "$bin" "$out/$target/fil"

# Static: the file command says so, and so does the absence of an interpreter.
desc=$(file "$out/$target/fil")
echo "$desc" >&2
case $target in
  x86_64-*) arch='ELF 64-bit.*x86-64' ;;
  aarch64-*) arch='ELF 64-bit.*ARM aarch64' ;;
esac
grep -Eq "$arch.*(statically linked|static-pie linked)" <<< "$desc" || die "$out/$target/fil is not a static binary for $target"
just no-test-hooks "$out/$target/fil"
sha256 "$out/$target/fil" | sed "s|\$|  $target/fil|" >&2
