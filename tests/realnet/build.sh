#!/usr/bin/env bash
# Build the real `fil` binary and the test programs (the stand-ins, the PTY driver) as static musl
# binaries into $OUT/bin, so they drop into any container (and run unchanged in network
# namespaces). Idempotent; cargo does the caching.
set -euo pipefail
# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

case $(uname -m) in
  x86_64) TARGET=x86_64-unknown-linux-musl ;;
  aarch64 | arm64) TARGET=aarch64-unknown-linux-musl ;;
  *) echo "realnet: unsupported architecture $(uname -m)" >&2; exit 2 ;;
esac

# CARGO_TARGET_DIR is honored (the VM recipes give each worktree its own, docs/dev/vm.md).
TARGET_DIR=${CARGO_TARGET_DIR:-$REPO_ROOT/target}

log "building fil and fil-testbins for $TARGET"
(cd "$REPO_ROOT" && cargo build --release --locked --target "$TARGET" -p fil -p fil-testbins)

mkdir -p "$BIN_DIR"
for b in fil pipe-echo pipe-probe quic-echo quiclink-probe ptydrive sockprobe fwdprobe keyprobe; do
  # Copy to a temporary name and rename over the old file: a rename works while a process left over
  # from a killed run still executes the old binary (a plain cp fails with "Text file busy").
  cp "$TARGET_DIR/$TARGET/release/$b" "$BIN_DIR/$b.new"
  mv -f "$BIN_DIR/$b.new" "$BIN_DIR/$b"
  # Static means no dynamic loader: ldd says so. Anything else would not run in a bare container.
  # Capture ldd's output first: under pipefail, `ldd | grep -q` fails when grep exits before ldd
  # has written (SIGPIPE), which showed up on aarch64.
  ldd_out=$(ldd "$BIN_DIR/$b" 2>&1 || true)
  if grep -qE 'not a dynamic|statically linked' <<< "$ldd_out"; then
    log "$b: static ($(du -h "$BIN_DIR/$b" | cut -f1))"
  else
    echo "realnet: $b is not statically linked" >&2
    exit 1
  fi
done
