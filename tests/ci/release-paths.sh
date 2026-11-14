#!/usr/bin/env bash
# Does the change touch the release paths? Shared by linux-check's and macos-check's `release-test`
# jobs, which run the release scripts' own tests (tests/release/just.sh release-test) per PR only
# when it does: a change elsewhere cannot change their result.
#
#   release-paths.sh [BASE]    prints the changed files and `run=true|false`; appends `run=...` to
#                              $GITHUB_OUTPUT when set. BASE defaults to the merge base of HEAD and
#                              origin/main (needs the history: checkout's fetch-depth: 0). With no
#                              merge base it says run=true.
#
# The release paths: what CODEOWNERS owns as deciding a release, the justfile (selftest copies it),
# this script and the other files selftest.sh reads (crates/fil/Cargo.toml, the wire limits and
# golden, the fixed remote commands, README.md, LICENSE, THIRD-PARTY-NOTICES.md). `--no-renames`, so
# a file moved out of a release path is listed under its old name too.
set -euo pipefail
base=${1:-$(git merge-base HEAD origin/main 2> /dev/null || true)}
run=true
if [ -n "$base" ]; then
  changed=$(git diff --no-renames --name-only "$base" HEAD)
  printf '%s\n' "$changed"
  # A here-string, not a pipe: with pipefail, grep -q's early exit would kill printf (SIGPIPE) on a
  # list longer than the pipe buffer and turn a match into run=false.
  if ! grep -Eq '^(tests/release/|tests/ci/release-paths\.sh$|packaging/|docs/man/|\.github/|\.cargo/|Cargo\.toml$|Cross\.toml$|rust-toolchain|deny\.toml$|about\.(toml|hbs)$|flake\.(nix|lock)$|\.gitattributes$|justfile$|crates/fil/Cargo\.toml$|crates/fil-proto/src/limits\.rs$|crates/fil-proto/tests/golden/|crates/fil-transport/src/pipe/remote-commands\.txt$|README\.md$|LICENSE$|THIRD-PARTY-NOTICES\.md$)' <<< "$changed"; then
    run=false
  fi
else
  echo "no merge base with origin/main: running the release scripts' tests"
fi
echo "release-test: $run"
if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "run=$run" >> "$GITHUB_OUTPUT"; fi
