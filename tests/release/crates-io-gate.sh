#!/usr/bin/env bash
# The checks release.yml's `crates-io` job makes before it asks crates.io for a token (N8;
# docs/m4/release.md sections 9.2 and 10.1). Run AFTER prepare.sh, which has checked the tag
# (v<version>, annotated, on main, signed, and still this run's commit) and fetched main.
#
#   crates-io-gate.sh TAG
#   crates-io-gate.sh --environment-only [--warn]
#
#   1. the `crates-io` environment's required reviewers include the owner (the user OWNER, below).
#      GitHub creates an environment that a
#      workflow names and that does not exist, with no protection at all; trusted publishing pins
#      the workflow file and the environment, not the ref, so an unprotected environment would let
#      any writer's branch mint a token. A misconfiguration fails here, loudly;
#      and its deployment refs are exactly the tag pattern v* (deployment-branch-policies);
#   2. the GitHub Release TAG is published (not a draft);
#   3. its SHA256SUMS.sig is a valid fil-release signature of its SHA256SUMS by a key in
#      .github/release-signers as it is on main (ssh-keygen -Y verify).
#
# --environment-only runs 1 alone: release.yml's `prepare` job runs it on every run, so a dry run
# proves that a job token with `actions: read` can read the environment before any tag exists.
# --warn (a dry run) makes a failure of 1 a warning, while the environment may not exist yet.
#
# Environment: GITHUB_REPOSITORY, GH_TOKEN (the job's token, which needs `actions: read` for the
# environment). FIL_GATE_GH replaces `gh` in the tests.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

# The one person who may approve a crates.io release (decided, owner: OQ-2, the release identity).
OWNER=anthonytw

env_only=0 warn=0 tag=''
while [ $# -gt 0 ]; do
  case $1 in
    --environment-only) env_only=1; shift ;;
    --warn) warn=1; shift ;;
    -*) die "unknown option: $1" ;;
    *) tag=$1; shift ;;
  esac
done
[ "$env_only" = 1 ] || [ -n "$tag" ] || die "usage: crates-io-gate.sh TAG | --environment-only [--warn]"
repo=${GITHUB_REPOSITORY:?}
gh=${FIL_GATE_GH:-gh}

env_fail() {
  if [ "$warn" = 1 ]; then
    echo "::warning::$*" >&2
    exit 0
  fi
  die "$*"
}
reviewers=$("$gh" api "repos/$repo/environments/crates-io" --jq '[.protection_rules[]? | select(.type == "required_reviewers") | .reviewers[]? | select(.type == "User") | .reviewer.login] | join(" ")' 2> "${TMPDIR:-/tmp}/crates-io-gate.$$") \
  || { e=$(head -c 300 "${TMPDIR:-/tmp}/crates-io-gate.$$" 2> /dev/null); rm -f "${TMPDIR:-/tmp}/crates-io-gate.$$"; env_fail "cannot read the crates-io environment of $repo ($e); it must exist with $OWNER as a required reviewer, and the job needs actions: read (docs/m4/release.md section 10.1)"; }
rm -f "${TMPDIR:-/tmp}/crates-io-gate.$$"
case " $reviewers " in
  *" $OWNER "*) log "the crates-io environment requires $OWNER's review" ;;
  *) env_fail "the crates-io environment's required reviewers ('$reviewers') do not include $OWNER: protect it first (docs/m4/release.md section 10.1)" ;;
esac
# Which refs may deploy to the environment: v* tags only. Trusted publishing does not pin the ref,
# so without this a branch's copy of release.yml (with its `if:` dropped) could be dispatched and
# ask for the owner's approval of a deployment that looks like any other.
policies=$("$gh" api "repos/$repo/environments/crates-io/deployment-branch-policies" --jq '[.branch_policies[]? | "\(.type):\(.name)"] | sort | join(" ")' 2> /dev/null) \
  || env_fail "cannot read the crates-io environment's deployment branch policies of $repo (or it has no custom policy; docs/m4/release.md section 10.1)"
[ "$policies" = 'tag:v*' ] \
  || env_fail "the crates-io environment's deployment refs are '${policies:-any}', not exactly the tag pattern v*: restrict them first (docs/m4/release.md section 10.1)"
log "the crates-io environment deploys from v* tags only"
[ "$env_only" = 0 ] || exit 0

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
