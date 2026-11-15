#!/usr/bin/env bash
# The cases of tests/ci/release-paths.sh: which changes run linux-check's `release-test` (the wide
# list) and macos-check's `release-test (arm64)` (`--mac`, the narrow one). Each case is one commit
# in a scratch repository, diffed against its parent. Part of `just ci-workflow-test`; needs git.
set -euo pipefail
script="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/release-paths.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.invalid GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.invalid
unset GITHUB_OUTPUT
git -C "$tmp" init -q
git -C "$tmp" commit -q --allow-empty -m base
failed=0 passed=0

# case FILE LINUX MAC: a commit that touches FILE gives run=LINUX, and run=MAC with --mac.
case_() {
  local file=$1 want_linux=$2 want_mac=$3 base got_linux got_mac
  base=$(git -C "$tmp" rev-parse HEAD)
  mkdir -p "$tmp/$(dirname "$file")"
  echo "$RANDOM" >> "$tmp/$file"
  git -C "$tmp" add -A
  git -C "$tmp" commit -q -m "$file"
  got_linux=$(cd "$tmp" && "$script" "$base" | sed -n 's/^release-test: //p')
  got_mac=$(cd "$tmp" && "$script" --mac "$base" | sed -n 's/^release-test (macOS): //p')
  if [ "$got_linux" = "$want_linux" ] && [ "$got_mac" = "$want_mac" ]; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    echo "  FAIL $file: linux run=$got_linux (want $want_linux), --mac run=$got_mac (want $want_mac)"
  fi
}

case_ justfile true false
case_ .github/workflows/linux-check.yml true false
case_ .github/workflows/macos-check.yml true false
case_ .github/workflows/release.yml true true
case_ Cargo.toml true false
case_ crates/fil-proto/tests/golden/wire.txt true false
case_ README.md true false
case_ tests/ci/release-paths.sh true false
case_ packaging/install.sh true true
case_ tests/release/selftest.sh true true
case_ crates/fil-client/src/engine.rs false false
case_ docs/user/README.md false false
# a file whose name only starts like a release path
case_ packaging.md false false

echo "release-paths: $passed passed, $failed failed"
[ "$failed" = 0 ]
