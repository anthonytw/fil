#!/usr/bin/env bash
# `just release-sign-macos X.Y.Z`: the owner signs and notarizes the macOS binary on their own Mac,
# never in CI (decided, owner 2026-11-04: N4; docs/m4/release.md section 10.3).
#
#   sign-macos.sh VERSION [--repo OWNER/REPO] [--work DIR] [--force]
#
# --force signs again a release this script has already signed (it says so in DIR/verified); a second
# run costs a notarization and gives an equivalent result.
#
# CI builds the macOS binary (ad hoc signed) into a DRAFT, which carries two copies of its archive:
# fil-X-universal-apple-darwin.ci.tar.gz, the one CI attested, which stays as it is, and
# fil-X-universal-apple-darwin.tar.gz, which this script replaces by a signed one. It works in the
# directory the full `release-verify X.Y.Z --pre-sign` verified (DIR, default
# target/release-verify/VERSION), never in a fresh download. In order:
#   1. checks that directory was verified in full (the rebuild included) and still holds what was
#      verified, and runs the local checks again on it (verify.sh --from-dir: the tag, SHA256SUMS, the
#      attestations, release-check; nothing is downloaded);
#   2. refuses a release that is not a draft, and a draft whose assets differ from the verified files
#      (a half-finished earlier run of this script is the one difference allowed: it is resumed);
#   3. takes the macOS binary from the .ci archive, and checks that this machine packs the archive
#      exactly as CI did (the unchanged binary repacks to the same bytes) BEFORE the notarization,
#      which is slow and cannot be taken back;
#   4. signs the binary with the Developer ID (hardened runtime, secure timestamp) and checks, from the
#      signature itself, the identifier, exactly the runtime flag, the Team ID, no entitlements;
#   5. requires the signed binary and the CI binary to normalize (macho_norm.py) to the same bytes:
#      the signature is the only thing that changed;
#   6. zips it with ditto and submits it to Apple with notarytool (keychain profile fil-notary),
#      waiting for the verdict. notarytool can exit 0 for an Invalid submission, so the verdict
#      itself ("status": "Accepted") is required. The submission id and Apple's log are kept in
#      DIR/macsign-logs;
#   7. only then: repacks the .ci archive around the signed binary (the CI packing), rewrites
#      SHA256SUMS and signs it with the release key (ssh-keygen -Y sign -n fil-release);
#   8. verifies that signature against the signers;
#   9. uploads the archive, SHA256SUMS and SHA256SUMS.sig to the draft (gh release upload --clobber).
# The results are staged in DIR/macsign-staged and copied over the verified files (so that
# `just release-sign-packages` continues from them) only after the upload succeeded. If the upload
# stops half way, run this script again: the draft still holds CI's .ci archive, which is all it needs.
#
# Environment: FIL_CODESIGN_IDENTITY (the certificate's name, as in the keychain; default below; `-`
# is ad hoc, for trying the script), FIL_RELEASE_KEY (default ~/.ssh/fil-release), FIL_NOTARY_PROFILE
# (default fil-notary), FIL_MACOS_TEAM_ID (default .github/macos-team-id). Test-only overrides, each
# of which prints a NOTE and none of which is accepted unless the self-test's FIL_RELEASE_SELFTEST=1
# is set (a real signing and upload with a faked step is refused): FIL_RELEASE_VERIFY, FIL_NOTARYTOOL, FIL_GH, FIL_CODESIGN, FIL_DITTO,
# FIL_RELEASE_PACK_CMD, FIL_RELEASE_SIGNERS. Needs Docker (the CI packing), codesign, ditto, xcrun, gh.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

version='' repo=anthonytw/fil work='' force=0
while [ $# -gt 0 ]; do
  case $1 in
    --repo) repo=$2; shift 2 ;;
    --work) work=$2; shift 2 ;;
    --force) force=1; shift ;;
    -*) die "unknown option: $1" ;;
    *) version=$1; shift ;;
  esac
done
[ -n "$version" ] || die "usage: sign-macos.sh VERSION [--repo OWNER/REPO] [--work DIR] [--force]"
version=${version#v}
tag=v$version

# ---- the test-only overrides: a NOTE each, and no upload for real while any of them replaces a step
test_override_notes
if [ "${FIL_RELEASE_SELFTEST:-}" != 1 ]; then
  for v in FIL_RELEASE_VERIFY FIL_NOTARYTOOL FIL_GH FIL_CODESIGN FIL_DITTO FIL_RELEASE_PACK_CMD FIL_RELEASE_SIGNERS FIL_MACOS_TEAM_ID FIL_RELEASE_ROOT; do
    [ -z "${!v:-}" ] || die "STOP: $v is set (a test-only override): a real signing and upload with a faked step is refused. Unset $v. (The self-test sets FIL_RELEASE_SELFTEST=1; nothing else should.)"
  done
fi

identity=${FIL_CODESIGN_IDENTITY:-Developer ID Application: Anthony Wertz (6X3PT3FXGA)}
profile=${FIL_NOTARY_PROFILE:-fil-notary}
key=${FIL_RELEASE_KEY:-$HOME/.ssh/fil-release}
gh=${FIL_GH:-gh}
ditto=${FIL_DITTO:-ditto}
team=${FIL_MACOS_TEAM_ID:-$(tr -d '[:space:]' < "$ROOT/.github/macos-team-id" 2> /dev/null || true)}
[ -n "$team" ] || die "STOP: no Team ID (.github/macos-team-id)"
read -r -a notarytool <<< "${FIL_NOTARYTOOL:-xcrun notarytool}"
[ -n "$work" ] || work=$REPO_ROOT/target/release-verify/$version
mkdir -p "$work"
work=$(cd "$work" && pwd)
dl=$work/files
arch=fil-$version-universal-apple-darwin
archive=$arch.tar.gz
ci_archive=$(ci_archive_name "$version")
logs=$work/macsign-logs
staged=$work/macsign-staged
scratch=$work/macsign
mkdir -p "$logs"

# ---- 1. what was verified, and the local checks again
[ -f "$work/verified" ] || die "STOP: $work/verified is missing: run the full verification first: just release-verify $version --pre-sign"
for l in rebuild=1 dry=0 "sums=$(sha256 "$dl/SHA256SUMS" 2> /dev/null || echo none)"; do
  grep -qx "$l" "$work/verified" || die "STOP: $work/verified does not say '$l': the files in $dl are not the ones the full 'just release-verify $version --pre-sign' verified. Run it again."
done
if grep -qx 'macos=signed' "$work/verified" && [ "$force" = 0 ]; then
  die "STOP: this release is already signed (macos=signed in $work/verified); pass --force to sign and notarize again"
fi
[ -f "$dl/$archive" ] && [ -f "$dl/$ci_archive" ] || die "STOP: $dl has no $archive and $ci_archive (a release built before the .ci archive existed cannot be signed)"
verifier=${FIL_RELEASE_VERIFY:-$RELEASE_DIR/verify.sh}
"$verifier" "$version" --pre-sign --no-rebuild --from-dir "$dl" --work "$work" --repo "$repo" || die "STOP: release-verify failed: nothing was signed"
signers=$work/signers
[ -z "${FIL_RELEASE_SIGNERS:-}" ] || cp "$FIL_RELEASE_SIGNERS" "$signers"
[ -s "$signers" ] || die "STOP: no signers file ($signers): the release signature could not be verified afterwards"
[ -f "$key" ] || die "STOP: no release key at $key (FIL_RELEASE_KEY)"

# ---- 2. a draft only, and the draft is what was verified
{
  for f in "$dl"/*; do [ -f "$f" ] && printf '%s %s\n' "${f##*/}" "$(sha256 "$f")"; done
  for f in "$staged"/*; do [ -f "$f" ] && printf '%s %s\n' "${f##*/}" "$(sha256 "$f")"; done
} 2> /dev/null > "$work/allowed" || true
view=$("$gh" release view "$tag" --repo "$repo" --json isDraft,assets) || die "STOP: cannot look up release $tag"
listing=$(printf '%s' "$view" | python3 -I -c 'import json,sys
d = json.load(sys.stdin)
print("draft=%s" % str(d["isDraft"]).lower())
for a in d["assets"]:
    print("asset %s %s" % (a["name"], (a.get("digest") or "-").replace("sha256:", "")))') || die "STOP: cannot read the release's assets"
[ "$(printf '%s\n' "$listing" | head -n 1)" = draft=true ] || die "STOP: release $tag is not a draft: a published release is not changed"
log "$tag is a draft"
seen=$work/draft-assets
: > "$seen"
while read -r kind name digest; do
  [ "$kind" = asset ] || continue
  if [ "$digest" = - ]; then
    # this gh does not report digests: download the asset and hash it
    rm -rf "$work/dlcheck" && mkdir "$work/dlcheck"
    "$gh" release download "$tag" --repo "$repo" -p "$name" -D "$work/dlcheck" || die "STOP: cannot download $name"
    digest=$(sha256 "$work/dlcheck/$name")
  fi
  grep -qx "$name $digest" "$work/allowed" || die "STOP: the draft's $name is not what was verified (digest $digest): the draft changed since 'just release-verify $version --pre-sign'. Verify it again."
  echo "$name" >> "$seen"
done <<< "$listing"
for f in "$dl"/*; do
  [ -f "$f" ] || continue
  grep -qx "${f##*/}" "$seen" || die "STOP: the draft lacks ${f##*/}, which was verified: the draft changed. Verify it again."
done
log "the draft holds the verified files (or an earlier run of this script's results)"
# an earlier attempt's results are only needed for the check above
rm -rf "$staged" "$scratch"
mkdir -p "$staged" "$scratch"

# ---- 3. the CI binary, and the CI packing
mkdir "$scratch/ci"
tar -xzpf "$dl/$ci_archive" -C "$scratch/ci" "$arch/bin/fil"
ci_bin=$scratch/ci/$arch/bin/fil
repack_macos_archive "$dl/$ci_archive" "$ci_bin" "$scratch/repack-check.tar.gz"
[ "$(sha256 "$scratch/repack-check.tar.gz")" = "$(sha256 "$dl/$ci_archive")" ] \
  || die "STOP: repacking CI's macOS archive here does not give the same bytes: this machine does not pack as CI does, so the signed archive would not verify"
log "CI's macOS archive repacks identically"

# ---- 4. sign
signed=$scratch/fil
cp "$ci_bin" "$signed"
chmod 755 "$signed"
ts=--timestamp
[ "$identity" != - ] || ts=--timestamp=none
cs --force --options runtime "$ts" --identifier fil --sign "$identity" "$signed" || die "STOP: codesign failed"
cs --verify --strict --verbose=2 "$signed" || die "STOP: the signature on $signed does not verify"
info=$(macho info "$signed") || die "STOP: the signature of $signed cannot be read"
n_slices=$(printf '%s\n' "$info" | grep -c .)
want() { [ "$(printf '%s\n' "$info" | grep -c -- "$1")" = "$n_slices" ] || die "STOP: the signature on $signed: every slice must have $2:
$info"; }
want ' identifier=fil ' "the identifier fil"
want ' entitlements=no' "no entitlements"
if [ "$identity" = - ]; then
  want ' kind=adhoc ' "an ad hoc signature (the identity is -)"
  for fl in $(printf '%s\n' "$info" | sed -n 's/.* flags=\(0x[0-9a-f]*\) .*/\1/p'); do
    [ $((fl & 0x10000)) != 0 ] || die "STOP: the signature on $signed does not enable the hardened runtime"
  done
else
  want ' kind=signed ' "a Developer ID signature"
  want ' flags=0x10000 ' "exactly the hardened runtime flag (0x10000)"
  want " team=$team " "the Team ID $team"
  cs --verify --strict -R="$(dev_id_requirement "$team")" "$signed" || die "STOP: $signed does not satisfy 'Developer ID Application certificate whose OU is $team': the certificate named '$identity' is not that team's"
  dvv=$(cs -dvv "$signed" 2>&1) || die "STOP: cannot read the signature of $signed"
  grep -qxF "Authority=$identity" <<< "$dvv" || die "STOP: $signed is not signed by '$identity' (a certificate is chosen by its name; a SHA-1 hash is not accepted here):
$dvv"
  grep -q '^Timestamp=' <<< "$dvv" || die "STOP: the signature on $signed has no secure timestamp"
fi
signed_sum=$(sha256 "$signed")
[ "$signed_sum" != "$(sha256 "$ci_bin")" ] || die "STOP: signing left $signed unchanged"
log "signed $signed ($signed_sum) as '$identity'"

# ---- 5. the signature is the only change
macho same "$signed" "$ci_bin" > "$scratch/same.log" 2>&1 \
  || { cat "$scratch/same.log" >&2; die "STOP: the signed binary and the CI binary differ by more than the signature (macho_norm.py)"; }
log "the signed binary and CI's normalize to the same bytes: $(cat "$scratch/same.log")"

# ---- 6. notarize
(cd "$scratch" && "$ditto" -c -k fil fil.zip) || die "STOP: ditto failed"
mkdir "$scratch/zip-check"
"$ditto" -x -k "$scratch/fil.zip" "$scratch/zip-check" || die "STOP: ditto cannot unpack the zip"
cmp -s "$scratch/zip-check/fil" "$signed" || die "STOP: the zip to notarize does not hold the signed binary"
log "submitting to Apple (profile $profile); waiting for the verdict"
verdict=$logs/notary-$(date +%Y%m%d-%H%M%S).json
"${notarytool[@]}" submit --wait --keychain-profile "$profile" --output-format json "$scratch/fil.zip" > "$verdict" || { cat "$verdict" >&2; die "STOP: notarytool failed"; }
cat "$verdict" >&2
# Apple's log for the submission (issues are listed even for an Accepted one), kept with the verdict
sub_id=$(sed -n 's/.*"id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$verdict" | head -n 1)
if [ -n "$sub_id" ]; then
  "${notarytool[@]}" log --keychain-profile "$profile" "$sub_id" "${verdict%.json}.log.json" > /dev/null 2>&1 \
    || log "note: could not fetch Apple's log for submission $sub_id"
  log "notarization submission $sub_id; verdict and log kept in $logs"
fi
grep -Eq '"status"[[:space:]]*:[[:space:]]*"Accepted"' "$verdict" || die "STOP: Apple did not accept the submission (notarytool can exit 0 for an Invalid one): nothing was changed or uploaded"
[ "$(sha256 "$signed")" = "$signed_sum" ] || die "STOP: the signed binary changed during notarization"
log "notarization accepted"

# ---- 7. the archive and SHA256SUMS around the signed binary
repack_macos_archive "$dl/$ci_archive" "$signed" "$staged/$archive"
tar -xzpOf "$staged/$archive" "$arch/bin/fil" > "$scratch/from-archive"
[ "$(sha256 "$scratch/from-archive")" = "$signed_sum" ] || die "STOP: the new archive does not hold the notarized binary"
view_dir=$scratch/view
mkdir "$view_dir"
for f in "$dl"/*; do
  case ${f##*/} in "$archive" | SHA256SUMS | SHA256SUMS.sig) ;; *) ln -s "$f" "$view_dir/" ;; esac
done
cp "$staged/$archive" "$view_dir/$archive"
rewrite_sums "$view_dir" "$version"
sums_check "$view_dir" SHA256SUMS || die "STOP: the files do not match the rewritten SHA256SUMS"
cp "$view_dir/SHA256SUMS" "$staged/SHA256SUMS"
ssh-keygen -Y sign -f "$key" -n fil-release "$staged/SHA256SUMS" || die "STOP: ssh-keygen could not sign SHA256SUMS with $key"

# ---- 8. the signature must verify against the signers
principal=$(ssh-keygen -Y find-principals -f "$signers" -s "$staged/SHA256SUMS.sig" 2> /dev/null | head -n 1) || true
[ -n "$principal" ] && ssh-keygen -Y verify -f "$signers" -I "$principal" -n fil-release -s "$staged/SHA256SUMS.sig" < "$staged/SHA256SUMS" > /dev/null 2>&1 \
  || die "STOP: SHA256SUMS.sig does not verify against the release signers: nothing was uploaded"
log "SHA256SUMS.sig is a valid fil-release signature by $principal"

# ---- 9. upload to the draft (the results are in $staged; the verified files are not changed yet)
if ! (cd "$staged" && "$gh" release upload "$tag" --repo "$repo" --clobber "$archive" SHA256SUMS SHA256SUMS.sig); then
  die "the upload stopped part way. The draft still holds CI's $ci_archive, which is all this needs: run 'just release-sign-macos $version' again (it signs and notarizes once more)."
fi
cp "$staged/$archive" "$staged/SHA256SUMS" "$staged/SHA256SUMS.sig" "$dl/"
printf 'sums=%s\nrebuild=1\npresign=1\ndry=0\nmacos=signed\n' "$(sha256 "$dl/SHA256SUMS")" > "$work/verified"
log "uploaded $archive, SHA256SUMS and SHA256SUMS.sig to the draft $tag (SHA256SUMS.sig now covers unsigned rpms until 'just release-sign-packages' and the signing after it)"
log "next: just release-sign-packages $version (docs/m4/release.md section 9.2)"
