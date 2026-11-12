#!/usr/bin/env bash
# Fill in a packaging template with a release's version and checksums (docs/m4/packaging.md).
#
#   render.sh brew|aur VERSION SUMS OUTDIR [URL_BASE]
#
#   brew      packaging/homebrew/fil.rb.in       -> OUTDIR/fil.rb
#   aur       packaging/aur/PKGBUILD.in, SRCINFO.in -> OUTDIR/PKGBUILD, OUTDIR/.SRCINFO
#   SUMS      the release's SHA256SUMS (the one the owner signed: check it against SHA256SUMS.sig,
#             or take it from `tests/release/just.sh release-verify`'s work directory, before rendering)
#   URL_BASE  where the archives are (default: the GitHub Release of vVERSION); a test may point it
#             at a local directory (file://...)
#
# Every checksum comes from SUMS, never from a download made here, and a template field left
# unfilled is an error.
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
die() { echo "render: $*" >&2; exit 1; }

[ $# -ge 4 ] && [ $# -le 5 ] || die "usage: render.sh brew|aur VERSION SUMS OUTDIR [URL_BASE]"
kind=$1 version=${2#v} sums=$3 out=$4
url_base=${5:-https://github.com/anthonytw/fil/releases/download/v$version}
printf '%s' "$version" | grep -Eq '^[0-9][0-9A-Za-z.-]{0,63}$' || die "version '$version' is not [0-9][0-9A-Za-z.-]{0,63} (docs/m4/release.md section 2)"
[ -f "$sums" ] || die "no file $sums"
# URL_BASE goes into a sed replacement and a Ruby and bash string: an https or file URL of plain
# characters only.
printf '%s' "$url_base" | grep -Eqx '(https|file)://[A-Za-z0-9._~/:-]+' || die "URL_BASE '$url_base' is not an https:// or file:// URL of [A-Za-z0-9._~/:-]"

sum_of() { # LABEL: the checksum of fil-VERSION-LABEL.tar.gz in SUMS, exactly one well-formed line
  local n=fil-$version-$1.tar.gz h
  h=$(awk -v n="$n" '$2 == n && length($1) == 64 && $1 ~ /^[0-9a-f]+$/ { print $1; c++ } END { if (c != 1) exit 1 }' "$sums") \
    || die "$sums has no single checksum for $n"
  printf '%s' "$h"
}
x86=$(sum_of x86_64-linux-musl)
arm=$(sum_of aarch64-linux-musl)
mac=$(sum_of universal-apple-darwin)
# pacman's pkgver allows no `-`: a pre-release 0.1.0-rc.1 is 0.1.0rc.1, which sorts before 0.1.0.
pkgver=$(printf '%s' "$version" | sed 's/-//g')

fill() { # TEMPLATE OUTPUT
  sed -e "s|@VERSION@|$version|g" -e "s|@PKGVER@|$pkgver|g" -e "s|@URL_BASE@|$url_base|g" \
    -e "s|@SHA256_X86_64_LINUX_MUSL@|$x86|g" -e "s|@SHA256_AARCH64_LINUX_MUSL@|$arm|g" \
    -e "s|@SHA256_UNIVERSAL_APPLE_DARWIN@|$mac|g" "$1" > "$2"
  if grep -n '@[A-Z0-9_]*@' "$2" >&2; then die "$2: a field was not filled in"; fi
}
mkdir -p "$out"
case $kind in
  brew) fill "$here/homebrew/fil.rb.in" "$out/fil.rb" ;;
  aur)
    fill "$here/aur/PKGBUILD.in" "$out/PKGBUILD"
    fill "$here/aur/SRCINFO.in" "$out/.SRCINFO"
    ;;
  *) die "kind '$kind' is neither brew nor aur" ;;
esac
echo "render: wrote $kind for $version into $out" >&2
