#!/usr/bin/env bash
# The checks release.yml's `crates-io` job makes before it asks crates.io for a token (N8;
# docs/m4/release.md sections 9.2 and 10.1). Run AFTER prepare.sh, which has checked the tag
# (v<version>, annotated, on main, signed, and still this run's commit) and fetched main.
#
#   crates-io-gate.sh TAG
#
#   1. the `crates-io` environment has a required reviewer. GitHub creates an environment that a
#      workflow names and that does not exist, with no protection at all; trusted publishing pins
#      the workflow file and the environment, not the ref, so an unprotected environment would let
#      any writer's branch mint a token. A misconfiguration fails here, loudly;
#   2. the GitHub Release TAG is published (not a draft);
#   3. its SHA256SUMS.sig is a valid fil-release signature of its SHA256SUMS by a key in
#      .github/release-signers as it is on main (ssh-keygen -Y verify).
#
# Environment: GITHUB_REPOSITORY, GH_TOKEN (the job's read-only token). FIL_GATE_GH replaces `gh`
# in the tests.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

[ $# -eq 1 ] || die "usage: crates-io-gate.sh TAG"
tag=$1
repo=${GITHUB_REPOSITORY:?}
gh=${FIL_GATE_GH:-gh}

reviewers=$("$gh" api "repos/$repo/environments/crates-io" --jq '[.protection_rules[]? | select(.type == "required_reviewers")] | length' 2> /dev/null) \
  || die "cannot read the crates-io environment of $repo; it must exist with a required reviewer (docs/m4/release.md section 10.1)"
[ "${reviewers:-0}" -ge 1 ] 2> /dev/null || die "the crates-io environment has no required reviewer: protect it first (docs/m4/release.md section 10.1)"
log "the crates-io environment has a required reviewer"

draft=$("$gh" release view "$tag" --repo "$repo" --json isDraft --jq .isDraft 2> /dev/null) \
  || die "$tag has no GitHub Release this job can see (a draft is invisible to it): publish it first (docs/m4/release.md section 9.2), then re-run this job"
[ "$draft" = false ] || die "$tag is still a draft: publish it first, then re-run this job"

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
"$gh" release download "$tag" --repo "$repo" -p SHA256SUMS -p SHA256SUMS.sig -D "$scratch" > /dev/null 2>&1 \
  || die "$tag has no SHA256SUMS and SHA256SUMS.sig to download"
git -C "$ROOT" show refs/remotes/origin/main:.github/release-signers > "$scratch/signers" \
  || die "no .github/release-signers on main (prepare.sh fetches main first)"
grep -Evq '^[[:space:]]*(#|$)' "$scratch/signers" || die ".github/release-signers on main has no key"
principal=$(ssh-keygen -Y find-principals -f "$scratch/signers" -s "$scratch/SHA256SUMS.sig" 2> /dev/null | head -n 1) || true
[ -n "$principal" ] && ssh-keygen -Y verify -f "$scratch/signers" -I "$principal" -n fil-release -s "$scratch/SHA256SUMS.sig" < "$scratch/SHA256SUMS" > /dev/null 2>&1 \
  || die "$tag's SHA256SUMS.sig is not a valid fil-release signature by a key on main"
log "$tag is published, and its SHA256SUMS.sig verifies ($principal)"
