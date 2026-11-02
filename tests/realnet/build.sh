#!/usr/bin/env bash
# Build the stand-in programs as static musl binaries into $OUT/bin, so they drop into any
# container (and run unchanged in network namespaces). Idempotent; cargo does the caching.
set -euo pipefail
# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

case $(uname -m) in
  x86_64) TARGET=x86_64-unknown-linux-musl ;;
  aarch64 | arm64) TARGET=aarch64-unknown-linux-musl ;;
  *) echo "realnet: unsupported architecture $(uname -m)" >&2; exit 2 ;;
esac

log "building fil-testbins for $TARGET"
(cd "$REPO_ROOT" && cargo build --release --locked --target "$TARGET" -p fil-testbins)

mkdir -p "$BIN_DIR"
for b in pipe-echo quic-echo; do
  cp "$REPO_ROOT/target/$TARGET/release/$b" "$BIN_DIR/$b"
  # Static means no dynamic loader: ldd says so. Anything else would not run in a bare container.
  if ldd "$BIN_DIR/$b" 2>&1 | grep -qE 'not a dynamic|statically linked'; then
    log "$b: static ($(du -h "$BIN_DIR/$b" | cut -f1))"
  else
    echo "realnet: $b is not statically linked" >&2
    exit 1
  fi
done
