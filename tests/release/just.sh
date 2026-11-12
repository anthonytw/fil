#!/usr/bin/env bash
# The release path's entry point: `just` with tests/release/release.just and no other justfile.
#
#   tests/release/just.sh RECIPE [ARGS...]     e.g. tests/release/just.sh release 0.2.0
#
# release.just has a code owner (.github/CODEOWNERS); the top-level justfile has none, and just runs
# whatever a justfile's assignments say (a backtick, shell()) as soon as it parses it, before any
# recipe. So nothing on the release path reads the top-level justfile: release.yml, the release
# scripts and the owner's release commands all come here (docs/m4/release.md section 9). release.just
# is self-contained (no import or mod; tests/release/recipes.sh) and sets its working directory to
# the repository root, so its recipes run there wherever this is run from. The owner's release
# commands also refuse to run under any other justfile (entry-check.sh).
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
command -v just > /dev/null || { echo "release: needs just 1.59.0 (brew install just)" >&2; exit 1; }
# No --working-directory: release.just's `set working-directory` is relative to the justfile's
# directory only without one.
exec just --justfile "$here/release.just" "$@"
