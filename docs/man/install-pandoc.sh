#!/usr/bin/env bash
# Installs the pinned pandoc (docs/man/build.sh) into DEST/bin/pandoc, for a Linux x86_64 or
# aarch64 machine: the release workflow's package job and the rebuild VM. The tarball is checked
# against the SHA-256 below before it is unpacked. Pandoc is a build tool for the man pages and is
# in no binary. To change the version, change it here and in docs/man/build.sh together.
#
#   docs/man/install-pandoc.sh DEST
set -euo pipefail

version=3.13
sha_amd64=38aa33b325a2bc96a4c39409631212db55702f564250bcdbead180b3bf646389
sha_arm64=21e1d4ba09bd0b334590d204580fd4f6629b21be4ab6a32b26cc98982e9e0c68

dest=${1:?usage: docs/man/install-pandoc.sh DEST}
[ "$(uname -s)" = Linux ] || { echo "install-pandoc: Linux only (install pandoc $version yourself elsewhere)" >&2; exit 1; }
case $(uname -m) in
  x86_64) arch=amd64 sha=$sha_amd64 ;;
  aarch64 | arm64) arch=arm64 sha=$sha_arm64 ;;
  *) echo "install-pandoc: no pandoc $version for $(uname -m)" >&2; exit 1 ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/pandoc.tar.gz" "https://github.com/jgm/pandoc/releases/download/$version/pandoc-$version-linux-$arch.tar.gz"
echo "$sha  $tmp/pandoc.tar.gz" | sha256sum -c - > /dev/null || { echo "install-pandoc: the download does not match the pinned SHA-256" >&2; exit 1; }
mkdir -p "$dest"
tar -xzf "$tmp/pandoc.tar.gz" -C "$dest" --strip-components=1
