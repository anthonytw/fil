#!/usr/bin/env bash
# The .deb and .rpm packages of a release (docs/m4/release.md section 4.1; docs/m4/packaging.md),
# made from the two Linux archives that package.sh has just assembled, so that every file in a
# package is byte for byte the file of the archive it comes from:
#
#   fil-shell_<ver>_amd64.deb    fil-shell-<ver>-1.x86_64.rpm     from fil-<ver>-x86_64-linux-musl
#   fil-shell_<ver>_arm64.deb    fil-shell-<ver>-1.aarch64.rpm    from fil-<ver>-aarch64-linux-musl
#
#   linux-packages.sh --archives DIR --out DIR
#
#   --archives  the directory with fil-<ver>-{x86_64,aarch64}-linux-musl.tar.gz
#   --out       where the four packages go (may be the same directory)
#
# The packages are built by nfpm in its digest-pinned image (NFPM_IMAGE in common.sh), with no
# network, from a configuration this script writes. They are reproducible: nfpm stamps every file and
# the package itself with `mtime` (SOURCE_DATE_EPOCH, as for the archives), the rpm's build host is a
# fixed name, and the file order is nfpm's own (sorted). Needs Docker.
#
# The installed layout is the archive's under /usr (package_path in common.sh), so the client finds
# its remote bundle where it looks, ../libexec/fil/remote from its own directory (section 4.3):
#   bin/fil                     /usr/bin/fil
#   libexec/fil/**              /usr/libexec/fil/**
#   share/doc/fil/*             /usr/share/doc/fil-shell/*
#   share/zsh/site-functions/*  /usr/share/zsh/vendor-completions/* (deb), the same path (rpm)
#   share/**                    /usr/share/**
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

archives='' out=''
while [ $# -gt 0 ]; do
  case $1 in
    --archives) archives=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    *) die "unknown argument: $1" ;;
  esac
done
[ -n "$archives" ] && [ -n "$out" ] || die "usage: linux-packages.sh --archives DIR --out DIR"
mkdir -p "$out"
archives=$(cd "$archives" && pwd)
out=$(cd "$out" && pwd)

V=$(cargo_version)
[ -n "$V" ] || die "no version in Cargo.toml"
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(source_date_epoch)}
export SOURCE_DATE_EPOCH
case $SOURCE_DATE_EPOCH in '' | *[!0-9]*) die "SOURCE_DATE_EPOCH is not a number: '$SOURCE_DATE_EPOCH'" ;; esac
# nfpm takes the time as RFC 3339, in UTC.
mtime=$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y-%m-%dT%H:%M:%SZ 2> /dev/null || date -u -r "$SOURCE_DATE_EPOCH" +%Y-%m-%dT%H:%M:%SZ)

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

# yaml_str TEXT: TEXT as a YAML double-quoted scalar (the values here are file names and fixed text).
yaml_str() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"; }

for t in $LINUX_TARGETS; do
  label=$(label_of "$t")
  arc=$archives/fil-$V-$label.tar.gz
  [ -f "$arc" ] || die "missing archive $arc"
  case $t in
    x86_64-*) goarch=amd64 rpmarch=x86_64 ;;
    aarch64-*) goarch=arm64 rpmarch=aarch64 ;;
  esac
  d=$stage/$label
  mkdir -p "$d"
  tar -xzf "$arc" -C "$d"
  root=fil-$V-$label
  [ -f "$d/$root/bin/fil" ] || die "$arc has no bin/fil"
  for kind in deb rpm; do
    cfg=$stage/$label.$kind.yaml
    {
      echo "name: fil-shell"
      echo "arch: $goarch"
      echo "platform: linux"
      echo "version: $(yaml_str "$V")"
      echo "version_schema: semver"
      echo "release: \"1\""
      echo "section: net"
      echo "priority: optional"
      echo "maintainer: \"Anthony Wertz <https://github.com/anthonytw/fil/issues>\""
      echo "homepage: https://github.com/anthonytw/fil"
      echo "license: GPL-3.0-or-later"
      echo "description: |-"
      echo "  A remote shell for development over SSH"
      echo "  fil keeps your place on a remote host: sessions survive reconnects, roaming and"
      echo "  sleep, you can take one over from another machine, and port forwards outlive the"
      echo "  link. It runs over ssh, or directly over QUIC. The package installs the static"
      echo "  fil binary and the remote bundle (both Linux architectures) that fil install uploads."
      echo "mtime: $(yaml_str "$mtime")"
      echo "contents:"
      (
        cd "$d/$root"
        find . -type f | sed 's|^\./||' | LC_ALL=C sort
      ) | while IFS= read -r f; do
        dst=$(package_path "$kind" "$f") || die "no place in the package for $f"
        if [ -x "$d/$root/$f" ]; then mode=0755; else mode=0644; fi
        echo "  - src: $(yaml_str "/stage/$label/$root/$f")"
        echo "    dst: $(yaml_str "$dst")"
        echo "    file_info:"
        echo "      mode: $mode"
        echo "      mtime: $(yaml_str "$mtime")"
        echo "      owner: root"
        echo "      group: root"
      done
      if [ "$kind" = rpm ]; then
        # An rpm owns only what it lists: list fil's own directories, so that removing the
        # package removes them (a .deb owns every directory of its files by itself).
        (
          cd "$d/$root"
          find libexec/fil share/doc/fil -type d | LC_ALL=C sort
        ) | while IFS= read -r dir; do
          case $dir in
            share/doc/fil) dst=/usr/share/doc/fil-shell ;;
            *) dst=/usr/$dir ;;
          esac
          echo "  - dst: $(yaml_str "$dst")"
          echo "    type: dir"
          echo "    file_info:"
          echo "      mode: 0755"
          echo "      mtime: $(yaml_str "$mtime")"
          echo "      owner: root"
          echo "      group: root"
        done
      fi
      echo "deb:"
      echo "  compression: gzip"
      echo "rpm:"
      echo "  compression: gzip"
      echo "  buildhost: fil-release"
      echo "  group: Applications/Internet"
    } > "$cfg"
  done
  echo "$goarch $rpmarch $label" >> "$stage/targets"
done

# The container sees the stage read-only and no network; the files it writes belong to the caller.
docker run --rm --network none --user "$(id -u):$(id -g)" \
  -e SOURCE_DATE_EPOCH -e V="$V" \
  -v "$stage:/stage:ro" -v "$out:/out" \
  --entrypoint /bin/sh "$NFPM_IMAGE" -ec '
    nfpm --version 2>&1 | grep -E "GitVersion" >&2 || true
    while read -r goarch rpmarch label; do
      nfpm package --config "/stage/$label.deb.yaml" --packager deb --target "/out/fil-shell_${V}_${goarch}.deb" >&2
      nfpm package --config "/stage/$label.rpm.yaml" --packager rpm --target "/out/fil-shell-${V}-1.${rpmarch}.rpm" >&2
    done < /stage/targets
  '
for f in "fil-shell_${V}_amd64.deb" "fil-shell_${V}_arm64.deb" "fil-shell-$V-1.x86_64.rpm" "fil-shell-$V-1.aarch64.rpm"; do
  [ -s "$out/$f" ] || die "nfpm did not write $f"
  log "$(sha256 "$out/$f")  $f"
done
