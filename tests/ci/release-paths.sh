#!/usr/bin/env bash
# Does the change touch the release paths? Shared by linux-check's and macos-check's `release-test`
# jobs, which run the release scripts' own tests (tests/release/just.sh release-test) per PR only
# when it does: a change elsewhere cannot change their result.
#
#   release-paths.sh [--mac] [BASE]
#                              prints the changed files and `run=true|false`; appends `run=...` to
#                              $GITHUB_OUTPUT when set. BASE defaults to the merge base of HEAD and
#                              origin/main (needs the history: checkout's fetch-depth: 0). With no
#                              merge base it says run=true.
#
# --mac: macos-check's `release-test (arm64)` (about 21 min) asks a narrower question: does the change
# touch what the macOS run adds over Linux's (the BSD userland of the scripts the owner runs on a Mac,
# and the macOS signing fakes)? That is tests/release/, packaging/ and release.yml. A change only to
# the justfile, a check workflow, a manifest or the golden is covered by linux-check's run on the
# wide list below, and macOS sees it on the next release-path PR or in suite=all.
#
# The release paths: what CODEOWNERS owns as deciding a release, the justfile (selftest copies it),
# this script and the other files selftest.sh reads (crates/fil/Cargo.toml, the wire limits and
# golden, the fixed remote commands, README.md, LICENSE, THIRD-PARTY-NOTICES.md). `--no-renames`, so
# a file moved out of a release path is listed under its old name too.
set -euo pipefail
mac=false
if [ "${1:-}" = --mac ]; then
  mac=true
  shift
fi
base=${1:-$(git merge-base HEAD origin/main 2> /dev/null || true)}
run=true
if [ -n "$base" ]; then
  changed=$(git diff --no-renames --name-only "$base" HEAD)
  printf '%s\n' "$changed"
  # A here-string, not a pipe: with pipefail, grep -q's early exit would kill printf (SIGPIPE) on a
  # list longer than the pipe buffer and turn a match into run=false.
  if $mac; then
    paths='^(tests/release/|packaging/|\.github/workflows/release\.yml$)'
  else
    paths='^(tests/release/|tests/ci/release-paths\.sh$|packaging/|docs/man/|\.github/|\.cargo/|Cargo\.toml$|Cross\.toml$|rust-toolchain|deny\.toml$|about\.(toml|hbs)$|flake\.(nix|lock)$|\.gitattributes$|justfile$|crates/fil/Cargo\.toml$|crates/fil-proto/src/limits\.rs$|crates/fil-proto/tests/golden/|crates/fil-transport/src/pipe/remote-commands\.txt$|README\.md$|LICENSE$|THIRD-PARTY-NOTICES\.md$)'
  fi
  if ! grep -Eq "$paths" <<< "$changed"; then
    run=false
  fi
else
  echo "no merge base with origin/main: running the release scripts' tests"
fi
if $mac; then echo "release-test (macOS): $run"; else echo "release-test: $run"; fi
if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "run=$run" >> "$GITHUB_OUTPUT"; fi
