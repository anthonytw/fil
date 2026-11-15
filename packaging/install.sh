#!/bin/sh
# fil's installer (docs/user/install.md; docs/m4/release.md section 4.1). Published with every
# release as fil-install.sh, which the release's SHA256SUMS lists and its owner signs.
#
#   curl -fsSL https://github.com/anthonytw/fil/releases/download/vX.Y.Z/fil-install.sh | sh -s -- --version X.Y.Z
#   sh fil-install.sh [--version X.Y.Z] [--dir DIR] [--help]
#
# What it does, and nothing else:
#   1. downloads SHA256SUMS and SHA256SUMS.sig of the latest release (or of --version) from GitHub,
#      with curl, over HTTPS only (redirects included), TLS 1.2 or later;
#   2. verifies SHA256SUMS.sig with `ssh-keygen -Y verify` against the release key written below,
#      the `fil-release` key of the repository's .github/release-signers, and stops if it does not
#      verify;
#   3. picks this machine's archive from the verified SHA256SUMS, downloads it from that release's
#      own directory (download/vVERSION, never "latest" a second time) and checks its SHA-256
#      against that list, and stops if it differs;
#   4. unpacks it in a temporary directory, stages bin/fil beside ~/.local/bin/fil (or --dir, or
#      $FIL_INSTALL_DIR) and the Linux programs `fil install` uploads beside ../libexec/fil/remote
#      (both resolved physically, as fil resolves its own path), then renames both into place, and
#      prints PATH advice if that directory is not on your PATH.
# No root, no sudo, no shell profile edited, nothing run from the download but the installed
# `fil --version` at the end. Every failure stops with nothing installed: a failed rename puts the
# old bundle back.
#
# Environment: FIL_VERSION (as --version), FIL_INSTALL_DIR (as --dir), FIL_RELEASE_URL (the
# releases base, default https://github.com/anthonytw/fil/releases; a mirror or a local copy as
# file:///path, laid out as GitHub's: latest/download/FILE and download/vX.Y.Z/FILE). Whatever the
# URL, what this script installs must verify against the key below. That proves who signed it, not
# that it is the newest: without --version a mirror (or a release marked latest) can hand over any
# older release the key signed, so name the version. And this script itself is checked by nothing
# before `curl | sh` runs it: the one-liner's trust is GitHub's HTTPS and the repository's settings
# (only the owner can create a tag or a release, and a published release never changes), which is
# why the docs give it at a version (docs/m4/packaging.md section 8).
#
# Needs: sh, curl, ssh-keygen (OpenSSH 8.1 or later, for -Y verify), tar, gzip, and sha256sum or
# shasum. Each is checked before anything is downloaded. (No wget: GNU wget's --https-only does not
# stop a redirect to plain HTTP, and busybox wget lacks it.)
#
# Everything is inside main(), called on the last line, so a download cut short runs nothing.
set -eu

# The release key: the `fil-release` lines of .github/release-signers on main (release-check
# requires them to be the same). A key rotation changes both in one PR; this copy, in each release,
# verifies only releases signed by a key it lists.
FIL_RELEASE_SIGNERS='release namespaces="fil-release" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILrP95b5kPn5fGzawh/SRaiMekWFgRDDqHjEfnDbuUFY'
FIL_RELEASE_PRINCIPAL=release
FIL_RELEASE_NAMESPACE=fil-release

say() { printf 'fil-install: %s\n' "$*" >&2; }
die() {
  say "STOP: $*"
  say "nothing was installed"
  exit 1
}

usage() {
  cat << 'EOF'
usage: sh fil-install.sh [--version X.Y.Z] [--dir DIR]

Downloads fil's release for this machine from GitHub, verifies its signature and checksum, and
installs `fil` into ~/.local/bin (or DIR). No root needed. https://github.com/anthonytw/fil
EOF
}

# fetch URL FILE: download URL into FILE. https:// (curl, HTTPS for every redirect too, TLS 1.2 or
# later) and file:// only.
fetch() {
  case $1 in
    https://*) curl -fsSL --proto '=https' --proto-redir '=https' --tlsv1.2 --retry 2 -o "$2" "$1" ;;
    file://*) cp "${1#file://}" "$2" ;;
    *) die "not an https:// or file:// URL: $1" ;;
  esac
}

# sha256 FILE: the file's SHA-256 in lowercase hex.
sha256() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum > /dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    die "needs sha256sum or shasum"
  fi
}

# target: this machine's archive label (docs/m4/release.md section 3).
target() {
  os=$(uname -s)
  arch=$(uname -m)
  case $os-$arch in
    Linux-x86_64 | Linux-amd64) echo x86_64-linux-musl ;;
    Linux-aarch64 | Linux-arm64) echo aarch64-linux-musl ;;
    Darwin-*) echo universal-apple-darwin ;;
    *) die "no fil release for $os $arch (Linux x86_64 and aarch64, and macOS, only; on Windows use WSL)" ;;
  esac
}

main() {
  version=${FIL_VERSION:-}
  dir=${FIL_INSTALL_DIR:-}
  while [ $# -gt 0 ]; do
    case $1 in
      --version) [ $# -ge 2 ] || die "--version needs a version"; version=$2; shift 2 ;;
      --dir) [ $# -ge 2 ] || die "--dir needs a directory"; dir=$2; shift 2 ;;
      -h | --help) usage; exit 0 ;;
      *) usage >&2; die "unknown argument: $1" ;;
    esac
  done
  version=${version#v}
  case $version in
    '') ;;
    *[!0-9A-Za-z.-]* | -* | .*) die "not a version: $version" ;;
  esac
  if [ -z "$dir" ]; then
    [ -n "${HOME:-}" ] || die "HOME is not set; name the directory with --dir"
    dir=$HOME/.local/bin
  fi
  case $dir in /*) ;; *) dir=$(pwd)/$dir ;; esac

  base=${FIL_RELEASE_URL:-https://github.com/anthonytw/fil/releases}
  base=${base%/}
  case $base in
    https://*) command -v curl > /dev/null 2>&1 || die "needs curl (install it with your package manager, then run this again)" ;;
    file://*) ;;
    *) die "not an https:// or file:// URL: $base" ;;
  esac
  command -v ssh-keygen > /dev/null 2>&1 || die "needs ssh-keygen (OpenSSH 8.1 or later) to check the release signature"
  # OpenSSH before 8.1 has no -Y: say so plainly, rather than as a signature that does not verify.
  case $(ssh-keygen -Y verify < /dev/null 2>&1) in
    *"illegal option"* | *"unknown option"* | *"invalid option"*) die "needs OpenSSH 8.1 or later: this ssh-keygen has no -Y, which checks the release signature" ;;
  esac
  command -v sha256sum > /dev/null 2>&1 || command -v shasum > /dev/null 2>&1 || die "needs sha256sum or shasum"
  command -v tar > /dev/null 2>&1 || die "needs tar"
  command -v gzip > /dev/null 2>&1 || die "needs gzip"
  label=$(target)
  if [ -n "$version" ]; then from=$base/download/v$version; else from=$base/latest/download; fi

  tmp=$(mktemp -d 2> /dev/null || mktemp -d -t fil-install)
  [ -n "$tmp" ] && [ -d "$tmp" ] || die "could not make a temporary directory"
  newbin='' newlib='' oldlib=''
  trap 'rm -rf "$tmp"; [ -z "$newbin" ] || rm -f "$newbin"; [ -z "$newlib" ] || rm -rf "$newlib"' EXIT
  trap 'exit 1' HUP INT TERM

  say "downloading SHA256SUMS and its signature from $from"
  fetch "$from/SHA256SUMS" "$tmp/SHA256SUMS" || die "could not download $from/SHA256SUMS"
  fetch "$from/SHA256SUMS.sig" "$tmp/SHA256SUMS.sig" || die "could not download $from/SHA256SUMS.sig"

  printf '%s\n' "$FIL_RELEASE_SIGNERS" > "$tmp/signers"
  if ! ssh-keygen -Y verify -f "$tmp/signers" -I "$FIL_RELEASE_PRINCIPAL" -n "$FIL_RELEASE_NAMESPACE" \
    -s "$tmp/SHA256SUMS.sig" < "$tmp/SHA256SUMS" > "$tmp/verify.log" 2>&1; then
    sed 's/^/  ssh-keygen: /' "$tmp/verify.log" >&2
    die "SHA256SUMS.sig is not a valid signature by fil's release key: the download is not trusted (an installer from an older release may predate a key change: get the current one)"
  fi
  say "SHA256SUMS is signed by fil's release key"

  # This machine's archive: exactly one `HASH  fil-VERSION-LABEL.tar.gz` line of the verified list
  # (the .ci copy of the macOS archive ends in .ci.tar.gz, so it never matches).
  lines=$(grep -E "^[0-9a-f]{64}  fil-[0-9A-Za-z.-]+-$label\\.tar\\.gz\$" "$tmp/SHA256SUMS" || true)
  [ -n "$lines" ] || die "SHA256SUMS lists no archive for $label"
  [ "$(printf '%s\n' "$lines" | wc -l | tr -d ' ')" = 1 ] || die "SHA256SUMS lists more than one archive for $label"
  want=${lines%% *}
  archive=${lines#*  }
  got_version=${archive#fil-}
  got_version=${got_version%-"$label".tar.gz}
  if [ -n "$version" ] && [ "$got_version" != "$version" ]; then
    die "the signed SHA256SUMS at $from is for fil $got_version, not $version"
  fi

  # The archive comes from that release's own directory: "latest" could move between two downloads.
  from=$base/download/v$got_version
  say "downloading $archive"
  fetch "$from/$archive" "$tmp/$archive" || die "could not download $from/$archive"
  got=$(sha256 "$tmp/$archive")
  [ "$got" = "$want" ] || die "$archive has SHA-256 $got, but the signed SHA256SUMS says $want"
  say "$archive matches the signed SHA256SUMS"

  mkdir "$tmp/x"
  (cd "$tmp/x" && gzip -dc "../$archive" | tar -xf -) || die "could not unpack $archive"
  top=$tmp/x/${archive%.tar.gz}
  [ -f "$top/bin/fil" ] && [ ! -h "$top/bin/fil" ] || die "$archive has no bin/fil"

  # Install: bin/fil into DIR and the remote bundle into DIR/../libexec/fil/remote, where fil looks
  # for it: fil resolves its own path physically (docs/m4/release.md section 4.3), so DIR and its
  # parent are resolved with pwd -P (a symbolic link ~/.local/bin -> ~/dotfiles/bin puts the bundle
  # in ~/dotfiles/libexec). Both are staged under temporary names (mktemp) in their directories,
  # then renamed into place, the bundle first; a running fil is never overwritten in place. Never a
  # symbolic link (docs/m4/install.md).
  mkdir -p "$dir" || die "could not make $dir"
  pdir=$(cd "$dir" && pwd -P) || die "could not resolve $dir"
  if [ -h "$pdir/fil" ]; then die "$pdir/fil is a symbolic link; remove it first (fil must be a real file)"; fi
  if [ -e "$pdir/fil" ] && [ ! -f "$pdir/fil" ]; then die "$pdir/fil exists and is not a regular file; remove it first"; fi
  newbin=$(mktemp "$pdir/.fil.new.XXXXXX") || die "could not write in $pdir"
  cp "$top/bin/fil" "$newbin" && chmod 755 "$newbin" || die "could not stage $pdir/fil"
  lib=''
  if [ -d "$top/libexec/fil/remote" ]; then
    lib=$(cd "$pdir/.." && pwd -P)/libexec/fil
    mkdir -p "$lib" || die "could not make $lib"
    newlib=$(mktemp -d "$lib/.remote.new.XXXXXX") || die "could not write in $lib"
    cp -R "$top/libexec/fil/remote/." "$newlib/" && chmod 755 "$newlib" || die "could not stage the remote bundle in $lib"
    if [ -e "$lib/remote" ] || [ -h "$lib/remote" ]; then
      oldlib=$(mktemp -d "$lib/.remote.old.XXXXXX") || die "could not write in $lib"
      mv "$lib/remote" "$oldlib/remote" || { rm -rf "$oldlib"; die "could not move the old bundle aside"; }
    fi
    if ! mv "$newlib" "$lib/remote"; then
      [ -n "$oldlib" ] || die "could not install $lib/remote"
      mv "$oldlib/remote" "$lib/remote" || die "could not install $lib/remote, nor put the old bundle back: it is in $oldlib/remote"
      rm -rf "$oldlib"
      die "could not install $lib/remote (the old bundle is back)"
    fi
    newlib=''
  fi
  if ! mv -f "$newbin" "$pdir/fil"; then
    [ -z "$lib" ] || rm -rf "$lib/remote"
    [ -n "$oldlib" ] || die "could not install $pdir/fil"
    mv "$oldlib/remote" "$lib/remote" || die "could not install $pdir/fil, nor put the old bundle back: it is in $oldlib/remote"
    rm -rf "$oldlib"
    die "could not install $pdir/fil (the old bundle is back)"
  fi
  newbin=''
  [ -z "$oldlib" ] || rm -rf "$oldlib"
  say "installed fil $got_version: $pdir/fil"
  "$pdir/fil" --version < /dev/null || say "note: $pdir/fil --version failed"

  case :${PATH:-}: in
    *:"$dir":* | *:"$pdir":*) ;;
    *)
      say "$dir is not on your PATH. Add it, for example:"
      say "  bash: echo 'export PATH=\"$dir:\$PATH\"' >> ~/.bashrc"
      say "  zsh:  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.zshrc"
      say "  fish: fish_add_path $dir"
      ;;
  esac
  say "the man pages and completions are in the archive (share/); a package manager installs them: docs/user/install.md"
}

main "$@"
