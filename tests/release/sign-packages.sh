#!/usr/bin/env bash
# `just release-sign-packages VERSION`: the owner signs the release's two .rpm packages on their own
# machine, never in CI (decided, owner 2026-11-04: D3, under N4), then rewrites SHA256SUMS and signs
# it with OpenPGP too (docs/m4/release.md section 10.4).
#
#   sign-packages.sh VERSION [DIR]
#
#   DIR   the release files that `just release-verify VERSION --pre-sign` verified (default
#         target/release-verify/VERSION/files). They are changed IN PLACE: the two .rpm files gain
#         a signature, SHA256SUMS is rewritten and SHA256SUMS.asc is written. Nothing is uploaded.
#         Before it changes anything it writes DIR/.signed-locally (a dotfile: release-check and
#         the upload commands do not see it), so that a later `just release-verify` in download
#         mode moves DIR aside instead of deleting these not-yet-uploaded files (verify.sh).
#
# The key is the one .github/package-signing-key.asc holds (its fingerprint is used to pick it from
# the owner's gpg keyring; gpg asks for the passphrase). Needs gpg, rpmsign and rpmkeys (Homebrew:
# `brew install gnupg rpm`), and python3.
#
# For each rpm it checks, before keeping the result, that signing changed nothing but the signature
# header (the header and payload digest, `body` of pkg_inspect.py, is the same: that is what makes
# the signed rpm verifiable against the reproducible unsigned one), that the signature is there, and
# that rpmkeys verifies it with the repository's key in a scratch database. The .deb files are not
# signed: dpkg and apt verify no signature inside a .deb (section 10.4).
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

[ $# -ge 1 ] && [ $# -le 2 ] || die "usage: sign-packages.sh VERSION [DIR]"
version=${1#v}
dir=${2:-$REPO_ROOT/target/release-verify/$version/files}
[ -d "$dir" ] || die "no directory $dir (run just release-verify $version --pre-sign first)"
dir=$(cd "$dir" && pwd)
gpg=${FIL_GPG:-gpg}
rpmsign=${FIL_RPMSIGN:-rpmsign}
rpmkeys=${FIL_RPMKEYS:-rpmkeys}
for t in "$gpg" "$rpmsign" "$rpmkeys" python3; do
  command -v "$t" > /dev/null || die "needs $t (on macOS: brew install gnupg rpm)"
done
keyfile=${FIL_PACKAGE_KEY_FILE:-$ROOT/.github/package-signing-key.asc}
grep -q 'BEGIN PGP PUBLIC KEY BLOCK' "$keyfile" 2> /dev/null || die "$keyfile holds no public key yet (the owner adds it: docs/m4/release.md section 10.4)"
fpr=$(package_key_fingerprint "$keyfile") || die "$keyfile is not exactly one OpenPGP public key"
log "package-signing key $fpr (from $keyfile)"
sums_check "$dir" SHA256SUMS || die "the files in $dir do not match their SHA256SUMS: verify them first"

# Marks DIR as holding locally signed files that may not be uploaded yet (verify.sh keeps such a directory).
printf 'signed by sign-packages.sh, not necessarily uploaded; release-verify moves this directory aside, never deletes it\n' > "$dir/.signed-locally"

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir -m 700 "$scratch/db"
"$rpmkeys" --dbpath "$scratch/db" --import "$keyfile"

for a in x86_64 aarch64; do
  p=$(package_file rpm "$a" "$version")
  f=$dir/$p
  [ -f "$f" ] || die "no $p in $dir"
  before=$(python3 -I "$RELEASE_DIR/pkg_inspect.py" "$f" "$scratch/x-before-$a" | sed -n 's/^body //p')
  cp "$f" "$scratch/$p"
  "$rpmsign" --addsign --define "_gpg_name $fpr" --define "__gpg $(command -v "$gpg")" "$scratch/$p" > "$scratch/sign.log" 2>&1 \
    || { cat "$scratch/sign.log" >&2; die "rpmsign failed for $p"; }
  python3 -I "$RELEASE_DIR/pkg_inspect.py" "$scratch/$p" "$scratch/x-after-$a" > "$scratch/after"
  [ "$(sed -n 's/^signed //p' "$scratch/after")" = yes ] || die "$p carries no signature after rpmsign"
  [ "$(sed -n 's/^body //p' "$scratch/after")" = "$before" ] || die "rpmsign changed more than the signature header of $p"
  "$rpmkeys" --dbpath "$scratch/db" --checksig "$scratch/$p" | grep -q 'signatures OK' \
    || { "$rpmkeys" --dbpath "$scratch/db" --checksig -v "$scratch/$p" >&2; die "$p's signature does not verify with $keyfile"; }
  mv "$scratch/$p" "$f"
  log "signed $p (body $before unchanged)"
done

rewrite_sums "$dir" "$version"
rm -f "$dir/SHA256SUMS.asc"
"$gpg" --armor --detach-sign --local-user "$fpr" --output "$dir/SHA256SUMS.asc" "$dir/SHA256SUMS"
"$gpg" --no-default-keyring --keyring "$scratch/k.kbx" --import "$keyfile" 2> /dev/null
"$gpg" --no-default-keyring --keyring "$scratch/k.kbx" --verify "$dir/SHA256SUMS.asc" "$dir/SHA256SUMS" 2> /dev/null \
  || die "SHA256SUMS.asc does not verify with $keyfile"
log "rewrote SHA256SUMS and wrote SHA256SUMS.asc in $dir; next: sign SHA256SUMS with the release key (docs/m4/release.md section 9.2, step 4)"
