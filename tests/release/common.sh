#!/usr/bin/env bash
# shellcheck disable=SC2034 # the variables are used by the scripts that source this file
# Shared by the release scripts in this directory (docs/m4/release.md). Source it; it sets nothing
# but functions and the variables below. Written for bash 3.2 (the Mac's /bin/bash) and GNU bash.
#
#   FIL_RELEASE_ROOT   the tree the scripts read (default: this checkout). The self-test points it
#                      at scratch copies to mutate one file at a time.

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
ROOT=${FIL_RELEASE_ROOT:-$REPO_ROOT}
RELEASE_DIR=$REPO_ROOT/tests/release

# The target names. `LINUX_TARGETS` are built by `cross`; the macOS pair is built natively and
# joined with lipo (docs/m4/release.md section 3).
LINUX_TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl"
DARWIN_TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
UNIVERSAL=universal-apple-darwin
# The archive label of each binary target (section 4.1).
label_of() {
  case $1 in
    x86_64-unknown-linux-musl) echo x86_64-linux-musl ;;
    aarch64-unknown-linux-musl) echo aarch64-linux-musl ;;
    "$UNIVERSAL") echo "$UNIVERSAL" ;;
    *) die "no archive label for $1" ;;
  esac
}
# The directory name of each Linux binary inside the remote bundle (section 4.3).
bundle_dir_of() {
  case $1 in
    x86_64-unknown-linux-musl) echo x86_64-linux ;;
    aarch64-unknown-linux-musl) echo aarch64-linux ;;
    *) die "no bundle directory for $1" ;;
  esac
}

die() {
  echo "release: $*" >&2
  exit 1
}
log() { echo "release: $*" >&2; }

# sha256 FILE: the hex digest, with whichever tool the machine has.
sha256() {
  if command -v sha256sum > /dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# sums_check DIR FILE: every line of FILE (inside DIR) is "<64 lowercase hex>  <name>", at least one
# line, and each named file there has that digest. Not `sha256sum -c`: some builds of it (macOS's
# /sbin/sha256sum) exit 0 for a file with no well-formed line at all.
sums_check() {
  local dir=$1 file=$2 h n count=0
  while IFS= read -r line || [ -n "$line" ]; do
    case $line in
      [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]"  "*) ;;
      *) return 1 ;;
    esac
    h=${line%%  *}
    n=${line#*  }
    case $n in "" | /* | .. | ../* | */../* | */..) return 1 ;; esac
    [ -f "$dir/$n" ] || return 1
    [ "$(sha256 "$dir/$n")" = "$h" ] || return 1
    count=$((count + 1))
  done < "$dir/$file"
  [ "$count" -gt 0 ]
}

# The one package version: [workspace.package] version in the root Cargo.toml (section 1).
cargo_version() {
  awk '
    /^\[workspace\.package\]/ { p = 1; next }
    /^\[/ { p = 0 }
    p && /^version[ ]*=/ { v = $0; sub(/^version[ ]*=[ ]*"/, "", v); sub(/".*$/, "", v); print v; exit }
  ' "$ROOT/Cargo.toml"
}
# rust-version of [workspace.package], and the channel of rust-toolchain.toml.
cargo_rust_version() {
  awk '
    /^\[workspace\.package\]/ { p = 1; next }
    /^\[/ { p = 0 }
    p && /^rust-version[ ]*=/ { v = $0; sub(/^rust-version[ ]*=[ ]*"/, "", v); sub(/".*$/, "", v); print v; exit }
  ' "$ROOT/Cargo.toml"
}
toolchain_channel() {
  sed -n 's/^channel[ ]*=[ ]*"\(.*\)"/\1/p' "$ROOT/rust-toolchain.toml" | head -n 1
}

# The wire this tree speaks (crates/fil-proto/src/limits.rs) and the last line of the golden's
# [history] (crates/fil-proto/tests/golden/wire.txt): the revision the golden ends at.
wire_major() { sed -n 's/^pub const PROTOCOL_MAJOR: u16 = \([0-9][0-9]*\);.*/\1/p' "$ROOT/crates/fil-proto/src/limits.rs"; }
wire_revision() { sed -n 's/^pub const PROTOCOL_REVISION: u16 = \([0-9][0-9]*\);.*/\1/p' "$ROOT/crates/fil-proto/src/limits.rs"; }
# release_bootstrap VER MAJOR REV: the install bootstrap a build of version VER on wire MAJOR.REV
# holds, from the golden of the remote commands (crates/fil-transport/src/pipe/remote-commands.txt,
# which shows it for 0.1.0 on wire 1.12; only the version and the wire differ between builds).
# Prints nothing if the golden is missing or its line does not start as the bootstrap does.
release_bootstrap() {
  local golden head
  golden=$(sed -n 's/^bootstrap = //p' "$ROOT/crates/fil-transport/src/pipe/remote-commands.txt" 2> /dev/null) || return 0
  head="sh -c 'v=0.1.0-w1.12; w=\"(wire 1.12)\"; "
  case $golden in "$head"*) ;; *) return 0 ;; esac
  printf "sh -c 'v=%s-w%s.%s; w=\"(wire %s.%s)\"; %s" "$1" "$2" "$3" "$2" "$3" "${golden#"$head"}"
}
golden_last_revision() {
  awk '/^\[history\]/ { h = 1; next } h && /^[0-9]+ = / { last = $1 } END { print last }' "${1:-$ROOT/crates/fil-proto/tests/golden/wire.txt}"
}

# cross_image TARGET: the image Cross.toml pins for TARGET (the digest-pinned reference).
cross_image() {
  awk -v want="[target.$1]" '
    $0 == want { p = 1; next }
    /^\[/ { p = 0 }
    p && /^image[ ]*=/ { v = $0; sub(/^image[ ]*=[ ]*"/, "", v); sub(/".*$/, "", v); print v; exit }
  ' "$ROOT/Cross.toml"
}
# The image the archives are assembled in: the x86_64 image (section 4.4).
tools_image() { cross_image x86_64-unknown-linux-musl; }
# The image the .deb and .rpm packages are built in (linux-packages.sh): nfpm 2.48.0 (MIT, a build
# tool in no binary), pinned by the digest of its multi-architecture index, so that the package job
# (amd64) and the VM's rebuild (arm64) run the same nfpm. To bump it: resolve the new tag's index
# digest (`docker image inspect --format '{{json .RepoDigests}}'` after a pull), change both here,
# and re-run a release dry run and `tests/release/just.sh release-verify`.
NFPM_VERSION=2.48.0
NFPM_IMAGE=docker.io/goreleaser/nfpm@sha256:d57a1452e7a019896b355f7b1ec67262278f6c503631f652d4fce7aef9ac8df7
# package_path KIND PATH: where a file at PATH in a Linux archive (bin/fil, libexec/..., share/...)
# is installed by the package of KIND (deb or rpm); nothing, with status 1, for a path that has no
# place. linux-packages.sh builds by it and release-check checks by it.
package_path() {
  case $2 in
    bin/fil) echo /usr/bin/fil ;;
    libexec/fil/*) echo "/usr/$2" ;;
    share/doc/fil/*) echo "/usr/share/doc/fil/${2#share/doc/fil/}" ;;
    share/zsh/site-functions/*) if [ "$1" = deb ]; then echo "/usr/share/zsh/vendor-completions/${2#share/zsh/site-functions/}"; else echo "/usr/$2"; fi ;;
    share/*) echo "/usr/$2" ;;
    *) return 1 ;;
  esac
}
# The distributions the packages are install-tested in (install-packages.sh, and the apt and dnf
# repositories in repos.sh), pinned by the digest of their multi-architecture index (bump like
# Cross.toml's). Not the -slim image: it excludes /usr/share/doc from every package it installs.
DEBIAN_IMAGE=docker.io/library/debian:13@sha256:2dc419c6d7fe3dad0a2f9fd469c34cc90b0f9ba642d8fde837fe3d7d34a0e7f1
FEDORA_IMAGE=docker.io/library/fedora:45@sha256:a7d72ba2745b100a3351087472e5ee7b2450af7cf331e7673007e3817612839c
# The Linux packages are named `fil` (decided, owner 2026-11-04: D1); the crate is `fil-shell`.
PACKAGE_NAME=fil
# package_file KIND ARCH VERSION: the file name of a package (KIND deb or rpm, ARCH x86_64 or
# aarch64, the Linux archive's architecture).
package_file() {
  case $1-$2 in
    deb-x86_64) echo "${PACKAGE_NAME}_$3_amd64.deb" ;;
    deb-aarch64) echo "${PACKAGE_NAME}_$3_arm64.deb" ;;
    rpm-x86_64) echo "$PACKAGE_NAME-$3-1.x86_64.rpm" ;;
    rpm-aarch64) echo "$PACKAGE_NAME-$3-1.aarch64.rpm" ;;
    *) die "no package file for $1 $2" ;;
  esac
}
# The package's own name for an architecture (ARCH as above).
package_arch() {
  case $1-$2 in
    deb-x86_64) echo amd64 ;;
    deb-aarch64) echo arm64 ;;
    rpm-*) echo "$2" ;;
  esac
}
# The packages' version for a release version: a pre-release's first `-` is `~` (it sorts before
# the release, as nfpm writes it), then release 1.
package_version() { printf '%s-1' "$(printf '%s' "$1" | sed 's/-/~/')"; }
# package_version_ok VERSION: X.Y.Z or X.Y.Z-PRE, PRE of [0-9A-Za-z.], the subset of the release
# versions (section 2) that nfpm takes as semver and that becomes X.Y.Z~PRE in the packages.
package_version_ok() { printf '%s' "$1" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?'; }
# The four package files of version $1 (section 4.1), sorted bytewise.
package_names() {
  local k a
  for k in deb rpm; do for a in x86_64 aarch64; do package_file "$k" "$a" "$1"; done; done | LC_ALL=C sort
}

# The installer (section 4.1; docs/user/install.md): packaging/install.sh of the tagged tree, byte for
# byte, published under this name. It is a release file like the others: SHA256SUMS lists it, so the
# release key's signature covers it.
INSTALLER_SRC=packaging/install.sh
INSTALLER_NAME=fil-install.sh
# The documents that give the installer's one-liner (and the key fingerprints users compare).
INSTALLER_DOCS="README.md docs/user/install.md"
# installer_oneliner VERSION: the one form the docs may give the `curl | sh` one-liner in. It names
# the version twice: the URL is that release's own copy (immutable once published, under a v* tag
# only the owner can create), never releases/latest, which any writer could point at a release of
# their own; and --version makes the script fetch that release's SHA256SUMS, never latest's.
# `just set-version` rewrites both; release-check holds the docs to it (doc_oneliner_problems).
installer_oneliner() {
  printf 'curl -fsSL https://github.com/anthonytw/fil/releases/download/v%s/%s | sh -s -- --version %s' "$1" "$INSTALLER_NAME" "$1"
}
# doc_oneliner_problems FILE VERSION: what is wrong with how FILE gives the installer one-liner, one
# problem a line (nothing when it is right): it must give installer_oneliner VERSION at least once,
# pipe the installer to a shell in no other form, and never name releases/latest. A Markdown table's
# `\|` counts as `|`.
doc_oneliner_problems() {
  local f=$1 want line rest n=0
  [ -f "$f" ] || { echo "it is missing"; return 0; }
  want=$(installer_oneliner "$2")
  ! grep -q 'releases/latest' "$f" || echo "it names releases/latest"
  while IFS= read -r line || [ -n "$line" ]; do
    line=${line//\\|/|}
    rest=${line//"$want"/}
    [ "$rest" = "$line" ] || n=$((n + 1))
    case $rest in
      *"$INSTALLER_NAME"*"|"*sh*) echo "it pipes $INSTALLER_NAME to a shell in another form: $(printf '%s' "$line" | head -c 200)" ;;
    esac
  done < "$f"
  [ "$n" -gt 0 ] || echo "it does not give '$want'"
}
# release_names VERSION: the files SHA256SUMS lists (sections 4.1 and 4.2): the five archives (the
# macOS one twice), the four packages and the installer, sorted bytewise.
release_names() {
  {
    printf '%s\n' "fil-$1-aarch64-linux-musl.tar.gz" "fil-$1-source.tar.gz" "fil-$1-universal-apple-darwin.tar.gz" "$(ci_archive_name "$1")" "fil-$1-x86_64-linux-musl.tar.gz" "$INSTALLER_NAME"
    package_names "$1"
  } | LC_ALL=C sort
}
# installer_signers FILE: the allowed-signers lines an installer script pins (its FIL_RELEASE_SIGNERS),
# one per line, sorted.
installer_signers() {
  awk '
    /^FIL_RELEASE_SIGNERS=\047/ { on = 1; sub(/^FIL_RELEASE_SIGNERS=\047/, "") }
    on { line = $0; if (sub(/\047[ \t]*$/, "", line)) { print line; on = 0; exit } print line }
  ' "$1" | LC_ALL=C sort
}
# installer_pin_problems FILE: what is wrong with how an installer script pins its key, one problem a
# line (nothing when it is right): exactly one FIL_RELEASE_SIGNERS= and one FIL_RELEASE_PRINCIPAL=
# assignment (a second one could replace the pinned key after release-check read the first), a
# principal of plain characters that every pinned line names first (ssh-keygen -I matches it), the
# fil-release namespace, and the one verify call reading exactly those (-f the file the pinned lines
# are written to, -I the principal, -n the namespace).
installer_pin_problems() {
  local f=$1 n principal line
  n=$(grep -c 'FIL_RELEASE_SIGNERS=' "$f" || true)
  [ "$n" = 1 ] || echo "FIL_RELEASE_SIGNERS= appears $n times, not once"
  n=$(grep -c 'FIL_RELEASE_PRINCIPAL=' "$f" || true)
  [ "$n" = 1 ] || echo "FIL_RELEASE_PRINCIPAL= appears $n times, not once"
  n=$(grep -c 'FIL_RELEASE_NAMESPACE=' "$f" || true)
  [ "$n" = 1 ] && grep -qx 'FIL_RELEASE_NAMESPACE=fil-release' "$f" || echo "FIL_RELEASE_NAMESPACE is not set once, to fil-release"
  principal=$(sed -n 's/^FIL_RELEASE_PRINCIPAL=\([A-Za-z0-9._-][A-Za-z0-9._-]*\)$/\1/p' "$f")
  if [ -z "$principal" ]; then
    echo "no FIL_RELEASE_PRINCIPAL=NAME line (NAME of [A-Za-z0-9._-])"
  else
    while IFS= read -r line; do
      [ "${line%% *}" = "$principal" ] || echo "a pinned line does not name the principal '$principal' first: ${line%% *}"
    done <<EOF
$(installer_signers "$f")
EOF
  fi
  # shellcheck disable=SC2016 # the installer's own text, matched literally
  n=$(grep -cF 'printf '\''%s\n'\'' "$FIL_RELEASE_SIGNERS" > "$tmp/signers"' "$f" || true)
  [ "$n" = 1 ] || echo "the pinned lines are not written to \$tmp/signers exactly once"
  # shellcheck disable=SC2016
  n=$(grep -cF 'ssh-keygen -Y verify -f "$tmp/signers" -I "$FIL_RELEASE_PRINCIPAL" -n "$FIL_RELEASE_NAMESPACE"' "$f" || true)
  [ "$n" = 1 ] || echo "the verify call is not exactly once ssh-keygen -Y verify -f \"\$tmp/signers\" -I \"\$FIL_RELEASE_PRINCIPAL\" -n \"\$FIL_RELEASE_NAMESPACE\""
  n=$(grep -v '^[[:space:]]*#' "$f" | grep -c 'ssh-keygen -Y verify' || true)
  [ "$n" = 2 ] || echo "ssh-keygen -Y verify appears $n times (the probe for -Y and the one verify call)"
}
# signers_release_lines FILE: the `fil-release` lines of an allowed-signers file (.github/release-signers),
# sorted: what the installer must pin.
signers_release_lines() { grep -E '^[^#[:space:]]+[[:space:]]+namespaces="fil-release"[[:space:]]' "$1" | LC_ALL=C sort || true; }

# The committer time of REF (default HEAD): the SOURCE_DATE_EPOCH of a release (section 4.4).
source_date_epoch() { git -C "$ROOT" log -1 --format=%ct "${1:-HEAD}"; }

# The path remapping that keeps builder paths out of the binary. cargo's profile key for this,
# `trim-paths`, is not stable in the pinned cargo (see [profile.release] in Cargo.toml), so the
# release builds pass the same thing as rustc flags. Each root is mapped to a FIXED name, so that
# `just release-check` can also require that the remap was applied: a dependency's panic location is
# always in the binary, and it must read /cargo-home/registry/..., not the builder's CARGO_HOME.
REMAP_WORKSPACE=/build
REMAP_CARGO_HOME=/cargo-home
REMAP_RUSTUP_HOME=/rustup-home
# remap_flags WORKSPACE CARGO_HOME RUSTUP_HOME
remap_flags() {
  local p
  for p in "$1" "$2" "$3"; do
    case $p in
      *[[:space:]]*) die "path '$p' contains a space; RUSTFLAGS cannot carry it" ;;
    esac
  done
  printf '%s' "--remap-path-prefix=$1=$REMAP_WORKSPACE --remap-path-prefix=$2=$REMAP_CARGO_HOME --remap-path-prefix=$3=$REMAP_RUSTUP_HOME"
}
# Inside a `cross` container the roots are fixed: the project is /project, cargo's home /cargo, the
# mounted toolchain /rust.
remap_flags_cross() { remap_flags /project /cargo /rust; }

# A clean environment for a release build: nothing from the caller's shell may change the bytes.
# (cargo reads these on top of the profile.)
clean_build_env() {
  unset CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_RUSTFLAGS RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER \
    CARGO_PROFILE_RELEASE_LTO CARGO_PROFILE_RELEASE_CODEGEN_UNITS CARGO_PROFILE_RELEASE_STRIP \
    CARGO_PROFILE_RELEASE_DEBUG CARGO_INCREMENTAL
}

# rewrite_sums DIR VERSION: DIR/SHA256SUMS for the release files of VERSION in DIR (section 4.2):
# the five archives (the macOS one twice, section 4.1), the four packages and the installer, sorted
# bytewise (release_names). package.sh writes it, and the owner's signing steps (sign-macos.sh,
# sign-packages.sh) rewrite it.
rewrite_sums() {
  local dir=$1 v=$2 f
  (
    cd "$dir" || exit 1
    for f in $(release_names "$v"); do
      [ -f "$f" ] || die "rewrite_sums: no $f in $dir"
      printf '%s  %s\n' "$(sha256 "$f")" "$f"
    done > SHA256SUMS.new
    mv SHA256SUMS.new SHA256SUMS
  )
}

# pack_archives STAGE OUT: every directory fil-* of STAGE becomes OUT/<dir>.tar.gz, with the recipe of
# section 4.4 and the GNU tar and gzip of the digest-pinned x86_64 image, never the host's. The
# stage is mounted read-only and the container has no network; the files it writes belong to the
# caller. SOURCE_DATE_EPOCH must be set. package.sh packs the release with it, and the owner's
# macOS signing (sign-macos.sh) repacks the macOS archive with it, so that the two are packed
# exactly alike. FIL_RELEASE_PACK_CMD is test-only (the self-test has no Docker): a command run as
# `$FIL_RELEASE_PACK_CMD STAGE OUT` in its place.
pack_archives() {
  local stage=$1 out=$2 image
  [ -d "$stage" ] && [ -d "$out" ] || die "pack_archives: $stage and $out must be directories"
  [ -n "${SOURCE_DATE_EPOCH:-}" ] || die "pack_archives: SOURCE_DATE_EPOCH is not set"
  export SOURCE_DATE_EPOCH
  if [ -n "${FIL_RELEASE_PACK_CMD:-}" ]; then
    "$FIL_RELEASE_PACK_CMD" "$stage" "$out"
    return
  fi
  image=$(tools_image)
  [ -n "$image" ] || die "Cross.toml has no x86_64 image"
  command -v docker > /dev/null && docker info > /dev/null 2>&1 || die "Docker is needed (the archives are packed by the GNU tar and gzip of a pinned image) and is not running"
  docker run --rm --platform linux/amd64 --network none \
    --user "$(id -u):$(id -g)" \
    -e SOURCE_DATE_EPOCH -e LC_ALL=C \
    -v "$stage:/stage:ro" -v "$out:/out" \
    "$image" sh -ec '
      tar --version | head -n 1 >&2
      gzip --version | head -n 1 >&2
      cd /stage
      for d in fil-*; do
        tar --sort=name --format=gnu --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 \
          --numeric-owner --mode="u=rwX,go=rX" -cf - "$d" | gzip -n -9 > "/out/$d.tar.gz"
      done
    '
}

# ---- macOS code signing (docs/m4/release.md section 10.3). FIL_CODESIGN is test-only: the self-test
# runs on Linux, with a fake.
cs() { "${FIL_CODESIGN:-codesign}" "$@"; }

# repack_macos_archive ARCHIVE BIN OUT: ARCHIVE (the macOS release archive) with bin/fil replaced
# by BIN, packed by pack_archives into the file OUT. SOURCE_DATE_EPOCH is the mtime of the archive's
# own entries (every entry carries it: section 4.4), so the repack is the CI packing but for the
# binary: repacking with the archive's own binary gives back the archive byte for byte.
repack_macos_archive() {
  local archive=$1 bin=$2 out=$3 st name sde
  st=$(mktemp -d "${TMPDIR:-/tmp}/fil-repack.XXXXXX")
  mkdir "$st/in" "$st/out"
  tar -xzpf "$archive" -C "$st/in" || { rm -rf "$st"; die "cannot unpack $archive"; }
  name=$(ls "$st/in")
  case $name in fil-*-universal-apple-darwin) ;; *) rm -rf "$st"; die "$archive does not hold one fil-*-universal-apple-darwin directory" ;; esac
  [ -f "$st/in/$name/bin/fil" ] || { rm -rf "$st"; die "$archive has no bin/fil"; }
  sde=$(perl -e 'print((stat $ARGV[0])[9])' "$st/in/$name/bin/fil")
  install -m 755 "$bin" "$st/in/$name/bin/fil"
  SOURCE_DATE_EPOCH=$sde pack_archives "$st/in" "$st/out" || { rm -rf "$st"; die "cannot pack the macOS archive"; }
  mv "$st/out/$name.tar.gz" "$out"
  rm -rf "$st"
}
# macho ARGS: the portable Mach-O normalizer (macho_norm.py, python3 stdlib only, any OS). It cuts a
# signature off a binary and normalizes what the signer changes with it, so the CI binary, the same
# binary with a Developer ID signature and a rebuild compare equal; see its header and section 10.3.
macho() { python3 -I "$RELEASE_DIR/macho_norm.py" "$@"; }

# ci_archive_name VERSION: the macOS archive exactly as CI built and attested it (decided, coordinator
# 2026-11-06, section 4.1): the release carries it unchanged next to the archive the owner signs.
ci_archive_name() { printf 'fil-%s-universal-apple-darwin.ci.tar.gz' "$1"; }

# dev_id_requirement TEAM: the code requirement that ties a signature to the certificate and not only
# to the Team ID written in the CodeDirectory: Apple's anchor with the Developer ID CA, a Developer ID
# Application leaf (the two OIDs are Apple's, as in the requirement codesign itself prints for such
# a binary) and the team as the leaf certificate's OU. `codesign --verify -R="$(dev_id_requirement
# TEAM)"` fails for anyone else's certificate (checked on a Developer-ID-signed app: the right team
# passes, another fails, an ad hoc binary fails).
dev_id_requirement() {
  printf 'anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] and certificate leaf[field.1.2.840.113635.100.6.1.13] and certificate leaf[subject.OU] = "%s"' "$1"
}

# Every test-only override any script here reads: each replaces a real tool with a fake, or a real
# input with a test's. One list, so that none goes unnoticed (release-preflight.sh refuses them all
# outside the self-test). A script that adds an override adds it here.
TEST_OVERRIDES="FIL_RELEASE_VERIFY FIL_NOTARYTOOL FIL_CODESIGN FIL_DITTO FIL_RELEASE_PACK_CMD FIL_RELEASE_SIGNERS FIL_GH FIL_MACOS_TEAM_ID FIL_RELEASE_ROOT
  FIL_GATE_GH FIL_GPG FIL_RPMSIGN FIL_RPMKEYS FIL_PACKAGE_KEY_FILE FIL_RELEASE_REPO_URL FIL_PUBLISH_CARGO FIL_PUBLISH_CURL FIL_RESERVE_CARGO FIL_RESERVE_CURL
  FIL_DOCKER FIL_LIMACTL FIL_SSH FIL_SECURITY FIL_CARGO FIL_CRATES_CURL FIL_DF FIL_SSH_ADD FIL_GPG_PRESET FIL_RELEASE_TTY FIL_RELEASE_LOCK_HOLD
  FIL_SIGN_MACOS FIL_SIGN_PACKAGES FIL_RELEASE_KEYS FIL_RELEASE_POLL FIL_BREW FIL_RELEASE_SHARED FIL_ENTRY_CHECK_STOP FIL_RELEASE_GITHUB_STANDIN FIL_RELEASE_SELFTEST"

# The owner's Developer ID (sign-macos.sh, release-preflight.sh); FIL_CODESIGN_IDENTITY names another.
DEFAULT_CODESIGN_IDENTITY='Developer ID Application: Anthony Wertz (6X3PT3FXGA)'

# test_override_notes: a NOTE for each test-only override that is set. They replace a real tool with
# a fake or skip a check; a shell that still has one exported must never go unnoticed.
test_override_notes() {
  local v
  for v in $TEST_OVERRIDES; do
    [ -z "${!v:-}" ] || echo "release: NOTE: $v is set ($(printf '%s' "${!v}" | cut -c1-60)): a test-only override; this run does NOT use the real tool or check it replaces" >&2
  done
}

# package_key_fingerprint FILE: the fingerprint of the one OpenPGP public key in FILE (the package
# signing key, .github/package-signing-key.asc; section 10.4). Fails unless FILE holds exactly one.
package_key_fingerprint() {
  ${FIL_GPG:-gpg} --show-keys --with-colons "$1" 2> /dev/null \
    | awk -F: '$1 == "pub" { n++ } $1 == "fpr" && !seen { print $10; seen = 1 } END { exit n != 1 }'
}
