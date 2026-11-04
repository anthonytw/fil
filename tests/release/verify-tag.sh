#!/usr/bin/env bash
# Check a release tag the way the owner's `just release-verify` does:
#
#   verify-tag.sh REPO TAG [MAINREF]
#
#   REPO     a git repository that has the tag and MAINREF
#   MAINREF  the ref that is `main` there (default refs/remotes/origin/main)
#
# 1. TAG is an annotated tag.
# 2. Its SSH signature verifies against the allowed-signers file read from MAINREF
#    (`.github/release-signers`), NEVER from the tag: whoever can push a tag can put any key in the
#    tag's own copy of the file. The verifier may instead name its own copy in FIL_RELEASE_SIGNERS.
# 3. The tagged commit is on main (an ancestor of MAINREF).
#
# A signers file with no key (the placeholder, docs/m4/release.md OQ-2) fails: nothing verifies.
set -euo pipefail

die() {
  echo "verify-tag: $*" >&2
  exit 1
}

[ $# -ge 2 ] || die "usage: verify-tag.sh REPO TAG [MAINREF]"
repo=$1
tag=$2
mainref=${3:-refs/remotes/origin/main}

git -C "$repo" rev-parse -q --verify "refs/tags/$tag" > /dev/null || die "no tag $tag in $repo"
[ "$(git -C "$repo" cat-file -t "refs/tags/$tag")" = tag ] || die "$tag is a lightweight tag; a release tag is annotated and signed"
git -C "$repo" rev-parse -q --verify "$mainref" > /dev/null || die "no $mainref in $repo"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
signers=$work/signers
if [ -n "${FIL_RELEASE_SIGNERS:-}" ]; then
  cp "$FIL_RELEASE_SIGNERS" "$signers"
else
  git -C "$repo" show "$mainref:.github/release-signers" > "$signers" 2> /dev/null || die "$mainref has no .github/release-signers"
fi
# A key line is anything that is not blank or a comment.
if ! grep -Evq '^[[:space:]]*(#|$)' "$signers"; then
  die "release-signers has no key (it is still the placeholder): nothing can be verified"
fi

out=$(git -C "$repo" -c gpg.format=ssh -c "gpg.ssh.allowedSignersFile=$signers" verify-tag "$tag" 2>&1) \
  || die "the signature of $tag does not verify against the signers on $mainref: $out"
echo "verify-tag: $tag is signed by a release signer on $mainref" >&2

commit=$(git -C "$repo" rev-parse "refs/tags/$tag^{commit}")
git -C "$repo" merge-base --is-ancestor "$commit" "$mainref" || die "$tag ($commit) is not on $mainref"
echo "verify-tag: $tag is on main" >&2
