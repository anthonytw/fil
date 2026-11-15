#!/usr/bin/env bash
# The 1.0 wire baseline (crates/fil-proto/tests/golden/wire-1.0.txt) is frozen byte for byte at
# the v1.0.0 tag (docs/v1/wire.md): once the tag exists, the file must be the tagged one. Skips
# while there is no such tag. linux-check runs it in the PR gate (`lint`, after fetching the tag
# alone) and in `compat` (tests/compat/run.sh).
set -euo pipefail

cd "$(dirname "$0")/../.."
baseline=crates/fil-proto/tests/golden/wire-1.0.txt
if ! git rev-parse -q --verify 'refs/tags/v1.0.0^{commit}' > /dev/null; then
  echo "anchor: no v1.0.0 tag yet: $baseline is not anchored to it yet"
  exit 0
fi
if ! git show "v1.0.0:$baseline" | cmp -s - "$baseline"; then
  echo "anchor: $baseline differs from v1.0.0's: the 1.0 wire baseline never changes after the tag" >&2
  exit 1
fi
echo "anchor: $baseline is v1.0.0's"
