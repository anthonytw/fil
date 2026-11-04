#!/usr/bin/env bash
# Install-test the .deb and .rpm packages of a release (docs/m4/packaging.md): each one is installed
# in a fresh container of its distribution, with no network, then checked and removed again.
#
#   install-packages.sh [--arch amd64|arm64|all] DIR
#
#   DIR     the release files (fil-shell_<ver>_<arch>.deb and fil-shell-<ver>-1.<arch>.rpm)
#   --arch  which architecture's packages to test (default: this machine's). The other one runs only
#           where Docker can run it (Rosetta in the VM, docs/m4/release.md section 9.3; QEMU on a
#           GitHub runner is not set up)
#
# In each container (as the container's root, never the host's):
#   - dpkg -i / rpm -i the package, with nothing else installed or fetched;
#   - `fil --version` prints `fil <ver> (wire <major>.<rev>)`, the first line of section 2;
#   - the remote bundle is where the client looks for it, ../libexec/fil/remote from /usr/bin, and
#     its SHA256SUMS checks;
#   - the package manager's own verification passes (dpkg --verify, rpm -V);
#   - removing the package leaves no file of fil behind.
# This RUNS the packaged binary: use it on packages built here or by CI, as release-check does.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

# The distributions, pinned by the digest of their multi-architecture index (bump like Cross.toml's).
# Not the -slim image: it excludes /usr/share/doc from every package it installs.
DEBIAN_IMAGE=docker.io/library/debian:13@sha256:2dc419c6d7fe3dad0a2f9fd469c34cc90b0f9ba642d8fde837fe3d7d34a0e7f1
FEDORA_IMAGE=docker.io/library/fedora:45@sha256:a7d72ba2745b100a3351087472e5ee7b2450af7cf331e7673007e3817612839c

arch='' dir=''
while [ $# -gt 0 ]; do
  case $1 in
    --arch) arch=$2; shift 2 ;;
    -*) die "unknown option: $1" ;;
    *) dir=$1; shift ;;
  esac
done
[ -n "$dir" ] || die "usage: install-packages.sh [--arch amd64|arm64|all] DIR"
dir=$(cd "$dir" && pwd)
if [ -z "$arch" ]; then
  case $(uname -m) in
    x86_64 | amd64) arch=amd64 ;;
    aarch64 | arm64) arch=arm64 ;;
    *) die "no package for this machine ($(uname -m))" ;;
  esac
fi
case $arch in all) arches="amd64 arm64" ;; amd64 | arm64) arches=$arch ;; *) die "--arch is amd64, arm64 or all" ;; esac

V=$(cargo_version)
expect="fil $V (wire $(wire_major).$(wire_revision))"
fails=0

# The test inside the container: $1 is the package manager (deb or rpm), $2 the package's path.
# shellcheck disable=SC2016 # expanded inside the container
inner='
set -eu
kind=$1 pkg=$2 expect=$3
if [ "$kind" = deb ]; then dpkg -i "$pkg" > /dev/null; else rpm -i --nosignature "$pkg"; fi
got=$(fil --version | head -n 1)
[ "$got" = "$expect" ] || { echo "fil --version printed \"$got\", not \"$expect\"" >&2; exit 1; }
[ "$(readlink -f "$(command -v fil)")" = /usr/bin/fil ] || { echo "fil on PATH is not /usr/bin/fil" >&2; exit 1; }
(cd /usr/bin/../libexec/fil/remote && sha256sum -c --quiet SHA256SUMS) || { echo "the bundle does not check" >&2; exit 1; }
here=$(sha256sum < /usr/bin/fil)
[ "$here" = "$(sha256sum < "/usr/libexec/fil/remote/$(uname -m | sed "s/^arm64$/aarch64/")-linux/fil")" ] || { echo "the bundle copy of this architecture is not bin/fil" >&2; exit 1; }
test -f /usr/share/doc/fil-shell/LICENSE || { echo "no LICENSE in /usr/share/doc/fil-shell" >&2; exit 1; }
if [ "$kind" = deb ]; then dpkg --verify fil-shell; dpkg -r fil-shell > /dev/null; else rpm -V --nosignature fil-shell; rpm -e fil-shell; fi
left=$(find /usr/bin/fil /usr/libexec/fil /usr/share/doc/fil-shell 2> /dev/null || true)
[ -z "$left" ] || { echo "removing the package left: $left" >&2; exit 1; }
echo "ok: $kind $(basename "$pkg"): installed, ran ($got), verified and removed"
'

for a in $arches; do
  case $a in amd64) rpmarch=x86_64 ;; arm64) rpmarch=aarch64 ;; esac
  for kind in deb rpm; do
    if [ "$kind" = deb ]; then f=fil-shell_${V}_$a.deb img=$DEBIAN_IMAGE; else f=fil-shell-$V-1.$rpmarch.rpm img=$FEDORA_IMAGE; fi
    if [ ! -f "$dir/$f" ]; then
      echo "install-packages: FAIL: no $f in $dir" >&2
      fails=$((fails + 1))
      continue
    fi
    if docker run --rm --network none --platform "linux/$a" -v "$dir/$f:/pkg/$f:ro" "$img" \
      sh -c "$inner" sh "$kind" "/pkg/$f" "$expect"; then
      :
    else
      echo "install-packages: FAIL: $f" >&2
      fails=$((fails + 1))
    fi
  done
done
[ "$fails" = 0 ] || { echo "install-packages: $fails package(s) FAILED" >&2; exit 1; }
echo "install-packages: all passed" >&2
