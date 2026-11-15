#!/usr/bin/env bash
# `tests/release/just.sh release-repos VERSION`: fil's own signed apt and dnf repositories, built from
# a published release's verified packages and signed with the package key on the owner's machine
# (docs/m4/packaging.md section 7; never in CI: N4). The output is a directory that the project site
# serves; copying it there is a task in the site's repository.
#
#   repos.sh VERSION [--work DIR] [--out DIR] [--base-url URL] [--arch amd64|arm64|all] [--no-install-test]
#
#   --work  the release's work directory (default target/release-verify/VERSION): its files/ must be
#           the signed release that `tests/release/just.sh release-verify VERSION` verified in full
#           (WORK/verified: the signature included, the SHA256SUMS that is there now)
#   --out   where the trees go (default WORK/repos): OUT/apt and OUT/rpm, replaced on each run
#   --base-url  the site's address, written into rpm/fil.repo (default https://fil.anthonywertz.com:
#           the trees are served at BASE/apt and BASE/rpm)
#   --arch  the install test's architecture (default this machine's; the other needs emulation)
#   --no-install-test  skip the install test (it needs Docker)
#
# In order, stopping at the first failure:
#   1. the inputs: WORK/verified says files/ was verified with its signature (presign=0) and names the
#      SHA256SUMS that is there; the files match it; the four packages are there and both rpms carry
#      the package key's signature (pkg_inspect.py);
#   2. repos.py writes the unsigned trees (every index binds every file by SHA-256);
#   3. gpg signs apt's Release (InRelease, clearsigned; Release.gpg, detached) and dnf's
#      repodata/repomd.xml (repomd.xml.asc) with the package key of .github/package-signing-key.asc
#      (gpg-agent has the passphrase: `tests/release/just.sh release-keys unlock --need gpg`), and
#      each signature is verified with that public key alone in a scratch keyring; the public key is
#      copied to apt/ and rpm/ as fil-package-key.asc;
#   4. the install test (Docker, the pinned Debian and Fedora images of install-packages.sh, no
#      network): apt and dnf install fil from the trees as file:// repositories with every signature
#      check on, `fil --version` names VERSION, and the package is removed again.
# Nothing is uploaded or published. FIL_GPG, FIL_PACKAGE_KEY_FILE, FIL_DOCKER and FIL_RELEASE_ROOT are test-only
# overrides, refused unless the self-test's FIL_RELEASE_SELFTEST=1 is set.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

if [ "${FIL_RELEASE_SELFTEST:-}" != 1 ]; then
  for v in FIL_GPG FIL_PACKAGE_KEY_FILE FIL_DOCKER FIL_RELEASE_ROOT; do
    [ -z "${!v:-}" ] || die "STOP: $v is set (a test-only override): unset it (only the self-test sets it, with FIL_RELEASE_SELFTEST=1)"
  done
fi
test_override_notes

version='' work='' out='' base_url=https://fil.anthonywertz.com arch='' itest=1
while [ $# -gt 0 ]; do
  case $1 in
    --work) work=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    --base-url) base_url=$2; shift 2 ;;
    --arch) arch=$2; shift 2 ;;
    --no-install-test) itest=0; shift ;;
    -*) die "unknown option: $1" ;;
    *) [ -z "$version" ] || die "one version only"; version=${1#v}; shift ;;
  esac
done
[ -n "$version" ] || die "usage: repos.sh VERSION [--work DIR] [--out DIR] [--base-url URL] [--arch amd64|arm64|all] [--no-install-test]"
package_version_ok "$version" || die "'$version' is not X.Y.Z or X.Y.Z-PRE"
case $base_url in https://*) ;; *) die "--base-url must be an https:// URL" ;; esac
work=${work:-$REPO_ROOT/target/release-verify/$version}
files=$work/files
out=${out:-$work/repos}
gpg=${FIL_GPG:-gpg}
docker=${FIL_DOCKER:-docker}
command -v "$gpg" > /dev/null || die "needs gpg (on macOS: brew install gnupg)"
command -v python3 > /dev/null || die "needs python3"

# ---- 1. the inputs
[ -d "$files" ] || die "no $files: run tests/release/just.sh release-verify $version first"
sums=$(sha256 "$files/SHA256SUMS" 2> /dev/null || true)
[ -n "$sums" ] || die "no SHA256SUMS in $files"
grep -qx "sums=$sums" "$work/verified" 2> /dev/null && grep -qx 'presign=0' "$work/verified" \
  || die "$files is not the signed release as verified: run tests/release/just.sh release-verify $version (after signing; WORK/verified must name presign=0 and this SHA256SUMS)"
[ -f "$files/SHA256SUMS.sig" ] || die "no SHA256SUMS.sig in $files"
sums_check "$files" SHA256SUMS || die "the files in $files do not match their SHA256SUMS"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
for p in $(package_names "$version"); do
  [ -f "$files/$p" ] || die "no $p in $files"
  case $p in
    *.rpm)
      python3 -I "$RELEASE_DIR/pkg_inspect.py" "$files/$p" "$scratch/x-$p" > "$scratch/inspect"
      [ "$(sed -n 's/^signed //p' "$scratch/inspect")" = yes ] || die "$p carries no signature: the repository serves only signed rpms (tests/release/just.sh release-sign-packages)"
      ;;
  esac
done
keyfile=${FIL_PACKAGE_KEY_FILE:-$ROOT/.github/package-signing-key.asc}
grep -q 'BEGIN PGP PUBLIC KEY BLOCK' "$keyfile" 2> /dev/null || die "$keyfile holds no public key"
fpr=$(package_key_fingerprint "$keyfile") || die "$keyfile is not exactly one OpenPGP public key"
log "release $version: the verified files in $files; package key $fpr"

# ---- 2. the trees
mkdir -p "$out"
out=$(cd "$out" && pwd)
python3 -I "$RELEASE_DIR/repos.py" --files "$files" --version "$version" --out "$out" --base-url "$base_url"

# ---- 3. the signatures, each checked with the public key alone
apt_rel=$out/apt/dists/stable/Release
repomd=$out/rpm/repodata/repomd.xml
rm -f "$apt_rel.gpg" "${apt_rel%/Release}/InRelease" "$repomd.asc"
"$gpg" --batch --yes --local-user "$fpr" --digest-algo SHA512 --clearsign --output "${apt_rel%/Release}/InRelease" "$apt_rel"
"$gpg" --batch --yes --local-user "$fpr" --digest-algo SHA512 --armor --detach-sign --output "$apt_rel.gpg" "$apt_rel"
"$gpg" --batch --yes --local-user "$fpr" --digest-algo SHA512 --armor --detach-sign --output "$repomd.asc" "$repomd"
mkdir -m 700 "$scratch/gnupg"
kg() { "$gpg" --homedir "$scratch/gnupg" --batch --no-autostart "$@"; }
kg --import "$keyfile" 2> /dev/null
kg --verify "$apt_rel.gpg" "$apt_rel" 2> /dev/null || die "Release.gpg does not verify with $keyfile"
kg --verify "${apt_rel%/Release}/InRelease" 2> /dev/null || die "InRelease does not verify with $keyfile"
kg --decrypt "${apt_rel%/Release}/InRelease" > "$scratch/inrelease.txt" 2> /dev/null || die "InRelease: no signed text"
cmp -s "$scratch/inrelease.txt" "$apt_rel" || die "InRelease does not carry exactly Release"
kg --verify "$repomd.asc" "$repomd" 2> /dev/null || die "repomd.xml.asc does not verify with $keyfile"
cp "$keyfile" "$out/apt/fil-package-key.asc"
cp "$keyfile" "$out/rpm/fil-package-key.asc"
chmod -R u=rwX,go=rX "$out/apt" "$out/rpm"
log "signed apt's Release (InRelease, Release.gpg) and dnf's repomd.xml (repomd.xml.asc) with $fpr; each verifies"

# ---- 4. the install test
if [ "$itest" = 1 ]; then
  if [ -z "$arch" ]; then
    case $(uname -m) in
      x86_64 | amd64) arch=amd64 ;;
      aarch64 | arm64) arch=arm64 ;;
      *) die "no package for this machine ($(uname -m)); pass --arch or --no-install-test" ;;
    esac
  fi
  case $arch in all) arches="amd64 arm64" ;; amd64 | arm64) arches=$arch ;; *) die "--arch is amd64, arm64 or all" ;; esac
  command -v "$docker" > /dev/null || die "the install test needs Docker (or pass --no-install-test)"
  # shellcheck disable=SC2016 # expanded inside the container
  apt_inner='
set -eu
v=$1
export DEBIAN_FRONTEND=noninteractive
log() { "$@" > /tmp/log 2>&1 || { cat /tmp/log >&2; echo "failed: $*" >&2; exit 1; }; }
rm -f /etc/apt/sources.list /etc/apt/sources.list.d/*
echo "deb [signed-by=/repo/fil-package-key.asc] file:/repo stable main" > /etc/apt/sources.list.d/fil.list
log apt-get -q update
if grep -Eq "^(W|E|Err):" /tmp/log; then cat /tmp/log >&2; echo "apt-get update warned or failed" >&2; exit 1; fi
log apt-get -q install -y fil
fil --version | head -n 1 | grep -q "^fil $v " || { echo "fil --version: $(fil --version | head -n 1)" >&2; exit 1; }
log apt-get -q remove -y fil
[ ! -e /usr/bin/fil ] || { echo "removing fil left /usr/bin/fil" >&2; exit 1; }
echo "ok: apt installed fil $v from the signed repository and removed it"
'
  # shellcheck disable=SC2016 # expanded inside the container
  dnf_inner='
set -eu
v=$1
log() { "$@" > /tmp/log 2>&1 || { cat /tmp/log >&2; echo "failed: $*" >&2; exit 1; }; }
printf "[fil]\nname=fil\nbaseurl=file:///repo\nenabled=1\ngpgcheck=1\nrepo_gpgcheck=1\ngpgkey=file:///repo/fil-package-key.asc\n" > /etc/yum.repos.d/fil.repo
log dnf -y --disablerepo="*" --enablerepo=fil install fil
fil --version | head -n 1 | grep -q "^fil $v " || { echo "fil --version: $(fil --version | head -n 1)" >&2; exit 1; }
log rpm -V fil
log dnf -y --disablerepo="*" remove fil
[ ! -e /usr/bin/fil ] || { echo "removing fil left /usr/bin/fil" >&2; exit 1; }
echo "ok: dnf installed fil $v from the signed repository (repomd and rpm signatures checked) and removed it"
'
  fails=0
  for a in $arches; do
    "$docker" run --rm --network none --platform "linux/$a" -v "$out/apt:/repo:ro" "$DEBIAN_IMAGE" sh -c "$apt_inner" sh "$version" \
      || { echo "repos: FAIL: the apt install test ($a)" >&2; fails=$((fails + 1)); }
    "$docker" run --rm --network none --platform "linux/$a" -v "$out/rpm:/repo:ro" "$FEDORA_IMAGE" sh -c "$dnf_inner" sh "$version" \
      || { echo "repos: FAIL: the dnf install test ($a)" >&2; fails=$((fails + 1)); }
  done
  [ "$fails" = 0 ] || die "$fails install test(s) failed: do not copy $out to the site"
else
  log "--no-install-test: apt and dnf were not run against the trees"
fi

cat >&2 << EOF
release: the repositories are in $out:
  $out/apt  ->  $base_url/apt   (apt: deb [signed-by=/etc/apt/keyrings/fil.asc] $base_url/apt stable main)
  $out/rpm  ->  $base_url/rpm   (dnf: $base_url/rpm/fil.repo)
Copy each directory's whole contents to that path of the site, replacing what is there (docs/m4/packaging.md section 7).
EOF
