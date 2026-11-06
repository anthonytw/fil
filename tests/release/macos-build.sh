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
# lipo drops the linker's signatures, and an arm64 binary without one is killed on Apple silicon
# ("code object is not signed at all": found by the first dry run's smoke-macos job). Sign it
# ad hoc, with a fixed identifier: no certificate, no timestamp and no secret, so the signature is a
# pure function of the file (adhoc_sign_macos signs from the canonical unsigned form, so that the
# verifier can reproduce the signature after a Developer ID signature is stripped).
# (Developer ID signing and notarization are done after CI by `just release-sign-macos`:
# docs/m4/release.md section 10.3.)
adhoc_sign_macos "$out/$UNIVERSAL/fil" >&2
codesign --verify --strict --verbose=2 "$out/$UNIVERSAL/fil" >&2
just no-test-hooks "$out/$UNIVERSAL/fil"
sha256 "$out/$UNIVERSAL/fil" | sed "s|\$|  $UNIVERSAL/fil|" >&2
