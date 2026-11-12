#!/usr/bin/env bash
# The first job of release.yml: what the version is, and, on a tag push, whether the tag is a
# plausible release. These checks catch MISTAKES. They are not the gate: on a tag push Actions runs
# the workflow file of the tagged commit, so whoever can push a `v*` tag controls this script and
# the signers file it reads. The gates are the tag ruleset, the draft that a person publishes and
# `tests/release/just.sh release-verify` with the owner's signature on SHA256SUMS (docs/m4/plan.md, REL).
#
# Environment (set by Actions): GITHUB_EVENT_NAME, GITHUB_REF, GITHUB_REF_NAME, GITHUB_OUTPUT.
# Prints `version`, `epoch` and `release` (true on a tag push) to GITHUB_OUTPUT.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

event=${GITHUB_EVENT_NAME:-}
ref=${GITHUB_REF:-}
version=$(cargo_version)
epoch=$(source_date_epoch)
release=false
log "version $version, SOURCE_DATE_EPOCH $epoch (commit $(git -C "$ROOT" rev-parse HEAD))"

if [ "$event" = push ] && [[ $ref == refs/tags/v* ]]; then
  release=true
  tag=${GITHUB_REF_NAME:?}
  [ "$tag" = "v$version" ] || die "tag $tag is not v$version (Cargo.toml)"
  # actions/checkout fetches a tag by commit, which turns an annotated tag into a lightweight one;
  # fetch the tag object itself, and main, anonymously (the repository is public).
  git -C "$ROOT" fetch --force --no-tags origin "+refs/tags/$tag:refs/tags/$tag" "+refs/heads/main:refs/remotes/origin/main"
  [ "$(git -C "$ROOT" cat-file -t "refs/tags/$tag")" = tag ] || die "$tag is not an annotated tag"
  # The tag must still name the commit this run checked out. A job that waits (the crates-io job
  # waits for the owner's approval) or is re-run after the tag was deleted and pushed again on
  # another commit would otherwise check the new tag and build or publish the old commit.
  tagged=$(git -C "$ROOT" rev-parse "refs/tags/$tag^{commit}")
  head=$(git -C "$ROOT" rev-parse HEAD)
  [ "$tagged" = "$head" ] || die "$tag is now commit $tagged, not this run's commit $head"
  git -C "$ROOT" merge-base --is-ancestor "$(git -C "$ROOT" rev-parse "refs/tags/$tag^{commit}")" refs/remotes/origin/main \
    || die "$tag is not on main"
  # The signature, against the signers on main. While the file is the placeholder (no key yet) there
  # is nothing to check against: say so loudly and go on, because the owner's offline check decides.
  if git -C "$ROOT" show refs/remotes/origin/main:.github/release-signers 2> /dev/null | grep -Evq '^[[:space:]]*(#|$)'; then
    "$RELEASE_DIR/verify-tag.sh" "$ROOT" "$tag" refs/remotes/origin/main
  else
    echo "::warning::.github/release-signers on main has no key (placeholder): the tag signature was NOT checked here. The owner verifies it with 'tests/release/just.sh release-verify' before publishing."
  fi
elif [ "$event" = push ]; then
  die "a push that is not a v* tag started the release workflow"
fi

if [ -n "${GITHUB_OUTPUT:-}" ]; then
  {
    echo "version=$version"
    echo "epoch=$epoch"
    echo "release=$release"
  } >> "$GITHUB_OUTPUT"
fi
