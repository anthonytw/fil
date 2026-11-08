#!/usr/bin/env bash
# Build the macOS release binary: aarch64 and x86_64 on the same Apple-silicon machine, joined with
# lipo into one universal file at OUT/universal-apple-darwin/fil (docs/m4/release.md section 3).
# Best effort for reproducibility (section 4.4): the Xcode that links is recorded in the log, and
# the deployment target is fixed. Run it on macOS.
#
#   macos-build.sh OUT
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

[ $# -eq 1 ] || die "usage: macos-build.sh OUT"
out=$1
[ "$(uname -s)" = Darwin ] || die "macos-build.sh runs on macOS"

clean_build_env
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(source_date_epoch)}
export SOURCE_DATE_EPOCH
# The oldest macOS the binary asks for. Fixed, so a newer SDK on the builder does not change it.
MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}
export MACOSX_DEPLOYMENT_TARGET

cargo_home=${CARGO_HOME:-$HOME/.cargo}
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
RUSTFLAGS=$(remap_flags "$ROOT" "$cargo_home" "$rustup_home")
export RUSTFLAGS

cd "$ROOT"
{
  echo "--- macOS build environment"
  sw_vers
  xcode-select -p
  xcodebuild -version 2> /dev/null || true
  clang --version | head -n 1
  rustc -vV
  echo "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH MACOSX_DEPLOYMENT_TARGET=$MACOSX_DEPLOYMENT_TARGET"
  echo "RUSTFLAGS=$RUSTFLAGS"
} >&2

for t in $DARWIN_TARGETS; do
  rustup target add "$t" >&2
  cargo build --locked --release -p fil-shell --target "$t"
done

tdir=${CARGO_TARGET_DIR:-$ROOT/target}
mkdir -p "$out/$UNIVERSAL"
lipo -create -output "$out/$UNIVERSAL/fil" \
  "$tdir/aarch64-apple-darwin/release/fil" "$tdir/x86_64-apple-darwin/release/fil"
lipo -info "$out/$UNIVERSAL/fil" >&2
# lipo's output is not reliably signed (the first dry run's smoke-macos job found an arm64 binary
# killed as "not signed at all"; on a recent lipo the arm64 slice keeps its linker signature and
# the x86_64 slice has none). Sign the whole file ad hoc, with a fixed identifier: no
# certificate, no timestamp and no secret, so the signature is a pure function of the file on one
# runner. It need not match anyone else's `codesign`: the release-verify of a signed release
# compares the binaries through macho_norm.py, which normalizes the signature away
# (docs/m4/release.md section 10.3). (Developer ID signing and notarization are done after CI by
# `just release-sign-macos`.)
codesign --force --sign - --identifier fil "$out/$UNIVERSAL/fil" >&2
codesign --verify --strict --verbose=2 "$out/$UNIVERSAL/fil" >&2
just no-test-hooks "$out/$UNIVERSAL/fil"
sha256 "$out/$UNIVERSAL/fil" | sed "s|\$|  $UNIVERSAL/fil|" >&2
