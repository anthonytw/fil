#!/usr/bin/env bash
# `just release-sign-macos VERSION`: the owner signs and notarizes the macOS binary on their own Mac,
# never in CI (decided, owner 2026-11-04: N4; docs/m4/release.md section 10.3).
#
#   sign-macos.sh VERSION [--repo OWNER/REPO] [--work DIR]
#
# CI builds the macOS binary (ad hoc signed, reproducibly) into a DRAFT. This script, in order:
#   1. runs `release-verify VERSION --pre-sign --no-rebuild` (verify.sh: the tag, SHA256SUMS, the
#      attestations, release-check) and stops if it fails, before anything is signed;
#   2. refuses a release that is not a draft (a published release is immutable);
#   3. extracts the macOS binary from the draft's archive, and checks that this machine packs the
#      archive exactly as CI did (the unchanged binary repacks to the same bytes) BEFORE the
#      notarization, which is slow and cannot be taken back;
#   4. signs the binary with the Developer ID (hardened runtime, secure timestamp), and checks the
#      identity, the runtime flag and the timestamp are on the file that was signed;
#   5. requires the signed binary, once the signature is stripped, to equal the CI binary stripped
#      the same way: the signature is the only thing that changed;
#   6. zips it with ditto and submits it to Apple with notarytool (keychain profile fil-notary),
#      waiting for the verdict. notarytool can exit 0 for an Invalid submission, so the verdict
#      itself ("status": "Accepted") is required;
#   7. only then: repacks the macOS archive around the signed binary (the CI packing), rewrites
#      SHA256SUMS, and signs it with the release key (ssh-keygen -Y sign -n fil-release);
#   8. verifies that signature against the signers (those of main, as verify.sh fetched them);
#   9. uploads the archive, SHA256SUMS and SHA256SUMS.sig to the draft (gh release upload --clobber).
# The files are changed IN PLACE in DIR/files (default target/release-verify/VERSION/files) only
# after step 6, so a rejection leaves them as downloaded. The packages are signed afterwards
# (`just release-sign-packages`, which rewrites SHA256SUMS: sign it again then; section 9.2).
#
# Environment: FIL_CODESIGN_IDENTITY (default below; `-` is ad hoc, for trying the script),
# FIL_RELEASE_KEY (default ~/.ssh/fil-release), FIL_NOTARY_PROFILE (default fil-notary). Test-only:
# FIL_RELEASE_VERIFY, FIL_NOTARYTOOL, FIL_GH, FIL_CODESIGN, FIL_DITTO, FIL_RELEASE_PACK_CMD,
# FIL_RELEASE_SIGNERS. Needs Docker (the CI packing), codesign, ditto, xcrun and gh.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

version='' repo=anthonytw/fil work=''
while [ $# -gt 0 ]; do
  case $1 in
    --repo) repo=$2; shift 2 ;;
    --work) work=$2; shift 2 ;;
    -*) die "unknown option: $1" ;;
    *) version=$1; shift ;;
  esac
done
[ -n "$version" ] || die "usage: sign-macos.sh VERSION [--repo OWNER/REPO] [--work DIR]"
version=${version#v}
tag=v$version
identity=${FIL_CODESIGN_IDENTITY:-Developer ID Application: Anthony Wertz (6X3PT3FXGA)}
profile=${FIL_NOTARY_PROFILE:-fil-notary}
key=${FIL_RELEASE_KEY:-$HOME/.ssh/fil-release}
gh=${FIL_GH:-gh}
ditto=${FIL_DITTO:-ditto}
read -r -a notarytool <<< "${FIL_NOTARYTOOL:-xcrun notarytool}"
[ -n "$work" ] || work=$REPO_ROOT/target/release-verify/$version
mkdir -p "$work"
work=$(cd "$work" && pwd)
dl=$work/files
arch=fil-$version-universal-apple-darwin
archive=$arch.tar.gz
scratch=$work/macsign
rm -rf "$scratch"
mkdir -p "$scratch"

# ---- 1. verify the draft, before anything is signed
verifier=${FIL_RELEASE_VERIFY:-$RELEASE_DIR/verify.sh}
"$verifier" "$version" --pre-sign --no-rebuild --work "$work" --repo "$repo" || die "STOP: release-verify failed: nothing was signed"
[ -f "$dl/$archive" ] && [ -f "$dl/SHA256SUMS" ] || die "STOP: $dl has no $archive or SHA256SUMS after the verification"
signers=$work/signers
[ -z "${FIL_RELEASE_SIGNERS:-}" ] || cp "$FIL_RELEASE_SIGNERS" "$signers"
[ -s "$signers" ] || die "STOP: no signers file ($signers): the release signature could not be verified afterwards"
[ -f "$key" ] || die "STOP: no release key at $key (FIL_RELEASE_KEY)"

# ---- 2. a draft only
is_draft=$("$gh" release view "$tag" --repo "$repo" --json isDraft --jq .isDraft) || die "STOP: cannot look up release $tag"
[ "$is_draft" = true ] || die "STOP: release $tag is not a draft (isDraft=$is_draft): a published release is not changed"
log "$tag is a draft"

# ---- 3. the CI binary, and the CI packing
mkdir "$scratch/ci"
tar -xzpf "$dl/$archive" -C "$scratch/ci" "$arch/bin/fil"
ci_bin=$scratch/ci/$arch/bin/fil
SOURCE_DATE_EPOCH=$(perl -e 'print((stat $ARGV[0])[9])' "$ci_bin")
export SOURCE_DATE_EPOCH
repack_macos_archive "$dl/$archive" "$ci_bin" "$scratch/repack-check.tar.gz"
[ "$(sha256 "$scratch/repack-check.tar.gz")" = "$(sha256 "$dl/$archive")" ] \
  || die "STOP: repacking the draft's own macOS archive here does not give the same bytes: this machine does not pack as CI does, so the signed archive would not verify"
log "the draft's macOS archive repacks identically"
strip_macos_signature "$ci_bin" "$scratch/ci-stripped"

# ---- 4. sign
signed=$scratch/fil
cp "$ci_bin" "$signed"
chmod 755 "$signed"
ts=--timestamp
[ "$identity" != - ] || ts=--timestamp=none
cs --force --options runtime "$ts" --identifier fil --sign "$identity" "$signed" || die "STOP: codesign failed"
cs --verify --strict --verbose=2 "$signed" || die "STOP: the signature on $signed does not verify"
info=$(cs -dvv "$signed" 2>&1) || die "STOP: cannot read the signature of $signed"
if [ "$identity" = - ]; then
  grep -q '^Signature=adhoc' <<< "$info" || die "STOP: $signed is not signed ad hoc, as the identity asked"
else
  grep -qxF "Authority=$identity" <<< "$info" || die "STOP: $signed is not signed by '$identity':
$info"
  grep -q '^Timestamp=' <<< "$info" || die "STOP: the signature on $signed has no secure timestamp"
fi
grep -Eq '^CodeDirectory .*flags=[^ ]*runtime' <<< "$info" || die "STOP: the signature on $signed does not enable the hardened runtime"
grep -qxF 'Identifier=fil' <<< "$info" || die "STOP: the signed binary's identifier is not fil"
signed_sum=$(sha256 "$signed")
[ "$signed_sum" != "$(sha256 "$ci_bin")" ] || die "STOP: signing left $signed unchanged"
log "signed $signed ($signed_sum) as '$identity'"

# ---- 5. the signature is the only change
strip_macos_signature "$signed" "$scratch/signed-stripped"
cmp -s "$scratch/signed-stripped" "$scratch/ci-stripped" \
  || die "STOP: the signed binary with its signature stripped is not the CI binary with its signature stripped: signing changed more than the signature"
log "stripped of its signature, the signed binary is byte-identical to CI's"

# ---- 6. notarize
(cd "$scratch" && "$ditto" -c -k fil fil.zip) || die "STOP: ditto failed"
mkdir "$scratch/zip-check"
"$ditto" -x -k "$scratch/fil.zip" "$scratch/zip-check" || die "STOP: ditto cannot unpack the zip"
cmp -s "$scratch/zip-check/fil" "$signed" || die "STOP: the zip to notarize does not hold the signed binary"
log "submitting to Apple (profile $profile); waiting for the verdict"
verdict=$scratch/notary.json
"${notarytool[@]}" submit --wait --keychain-profile "$profile" --output-format json "$scratch/fil.zip" > "$verdict" || { cat "$verdict" >&2; die "STOP: notarytool failed"; }
cat "$verdict" >&2
grep -Eq '"status"[[:space:]]*:[[:space:]]*"Accepted"' "$verdict" || die "STOP: Apple did not accept the submission (notarytool can exit 0 for an Invalid one): nothing was changed or uploaded"
[ "$(sha256 "$signed")" = "$signed_sum" ] || die "STOP: the signed binary changed during notarization"
log "notarization accepted"

# ---- 7. the archive and SHA256SUMS around the signed binary
repack_macos_archive "$dl/$archive" "$signed" "$scratch/$archive"
tar -xzpOf "$scratch/$archive" "$arch/bin/fil" > "$scratch/from-archive"
[ "$(sha256 "$scratch/from-archive")" = "$signed_sum" ] || die "STOP: the new archive does not hold the notarized binary"
cp "$dl/$archive" "$scratch/ci-$archive"
cp "$dl/SHA256SUMS" "$scratch/ci-SHA256SUMS"
mv "$scratch/$archive" "$dl/$archive"
rewrite_sums "$dl" "$version"
sums_check "$dl" SHA256SUMS || die "STOP: the files in $dl do not match the rewritten SHA256SUMS"
rm -f "$dl/SHA256SUMS.sig"
ssh-keygen -Y sign -f "$key" -n fil-release "$dl/SHA256SUMS" || die "STOP: ssh-keygen could not sign SHA256SUMS with $key"

# ---- 8. the signature must verify against the signers
principal=$(ssh-keygen -Y find-principals -f "$signers" -s "$dl/SHA256SUMS.sig" 2> /dev/null | head -n 1) || true
[ -n "$principal" ] && ssh-keygen -Y verify -f "$signers" -I "$principal" -n fil-release -s "$dl/SHA256SUMS.sig" < "$dl/SHA256SUMS" > /dev/null 2>&1 \
  || die "STOP: SHA256SUMS.sig does not verify against the release signers: nothing was uploaded (the new files are in $dl; the draft's are in $scratch/ci-*)"
log "SHA256SUMS.sig is a valid fil-release signature by $principal"

# ---- 9. upload to the draft
(cd "$dl" && "$gh" release upload "$tag" --repo "$repo" --clobber "$archive" SHA256SUMS SHA256SUMS.sig) || die "STOP: upload failed"
log "uploaded $archive, SHA256SUMS and SHA256SUMS.sig to the draft $tag; next: just release-sign-packages $version (section 9.2)"
