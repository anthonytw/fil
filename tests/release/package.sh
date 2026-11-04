#!/usr/bin/env bash
# Assemble the release files (docs/m4/release.md section 4) from the three built binaries:
# the three binary archives, the vendored source archive and SHA256SUMS.
#
#   package.sh --bins DIR --out DIR [--extra DIR]
#
#   --bins   DIR/x86_64-unknown-linux-musl/fil, DIR/aarch64-unknown-linux-musl/fil and
#            DIR/universal-apple-darwin/fil (what build-binary.sh and macos-build.sh write)
#   --out    where the archives and SHA256SUMS go (created; must be empty)
#   --extra  a tree laid out like an archive (share/man/man1/fil.1, share/doc/fil/THIRD-PARTY-
#            NOTICES.md, ...) whose files are added to every binary archive: the generated files
#            (notices, man pages, completions, examples) that other steps of the workflow produce
#
# Every archive is assembled by the GNU tar and gzip inside the digest-pinned x86_64 image of
# Cross.toml, never the host's, with the recipe of section 4.4. Needs Docker, git and cargo (for
# `cargo vendor`). SOURCE_DATE_EPOCH defaults to HEAD's committer time.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

bins='' out='' extra=''
while [ $# -gt 0 ]; do
  case $1 in
    --bins) bins=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    --extra) extra=$2; shift 2 ;;
    *) die "unknown argument: $1" ;;
  esac
done
[ -n "$bins" ] && [ -n "$out" ] || die "usage: package.sh --bins DIR --out DIR [--extra DIR]"
for t in $LINUX_TARGETS $UNIVERSAL; do
  [ -f "$bins/$t/fil" ] || die "missing binary: $bins/$t/fil"
done
mkdir -p "$out"
[ -z "$(ls -A "$out")" ] || die "$out is not empty"
out=$(cd "$out" && pwd)

V=$(cargo_version)
[ -n "$V" ] || die "no version in Cargo.toml"
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(source_date_epoch)}
export SOURCE_DATE_EPOCH
IMAGE=$(tools_image)
[ -n "$IMAGE" ] || die "Cross.toml has no x86_64 image"
log "version $V, SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH, tools image $IMAGE"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

# ---- the three binary archives
for t in $LINUX_TARGETS $UNIVERSAL; do
  name=fil-$V-$(label_of "$t")
  d=$stage/$name
  mkdir -p "$d/bin" "$d/libexec/fil/remote" "$d/share/doc/fil"
  install -m 755 "$bins/$t/fil" "$d/bin/fil"
  for lt in $LINUX_TARGETS; do
    bd=$(bundle_dir_of "$lt")
    mkdir -p "$d/libexec/fil/remote/$bd"
    install -m 755 "$bins/$lt/fil" "$d/libexec/fil/remote/$bd/fil"
  done
  # The bundle's list (section 4.3): paths relative to libexec/fil/remote, sorted bytewise.
  (
    cd "$d/libexec/fil/remote"
    for f in $(printf '%s\n' */fil | LC_ALL=C sort); do
      printf '%s  %s\n' "$(sha256 "$f")" "$f"
    done > SHA256SUMS
  )
  for f in README.md LICENSE CHANGELOG.md; do
    [ -f "$ROOT/$f" ] && install -m 644 "$ROOT/$f" "$d/share/doc/fil/$f"
  done
  if [ -n "$extra" ]; then
    (cd "$extra" && tar -cf - .) | (cd "$d" && tar -xf -)
  fi
done

# ---- the source archive: the tagged tree with every dependency vendored, building offline
name=fil-$V-source
git -C "$ROOT" archive --format=tar --prefix="$name/" HEAD | tar -x -C "$stage"
(
  cd "$stage/$name"
  mkdir .cargo
  # `cargo vendor` prints the source-replacement config on stdout and progress on stderr.
  cargo vendor --locked vendor > .cargo/config.toml
)
if [ ! -s "$stage/$name/.cargo/config.toml" ]; then
  # Nothing to replace is right only for a tree with no dependency from a registry or git at all.
  ! grep -q '^source = ' "$stage/$name/Cargo.lock" || die "the vendoring step wrote no source-replacement config"
  log "the tree has no external dependency: nothing vendored"
fi

# ---- tar and gzip, inside the image
# The container sees the stage read-only and no network; the files it writes belong to the caller.
docker run --rm --platform linux/amd64 --network none \
  --user "$(id -u):$(id -g)" \
  -e SOURCE_DATE_EPOCH -e LC_ALL=C \
  -v "$stage:/stage:ro" -v "$out:/out" \
  "$IMAGE" sh -ec '
    tar --version | head -n 1 >&2
    gzip --version | head -n 1 >&2
    cd /stage
    for d in fil-*; do
      tar --sort=name --format=gnu --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 \
        --numeric-owner --mode="u=rwX,go=rX" -cf - "$d" | gzip -n -9 > "/out/$d.tar.gz"
    done
  '

# ---- SHA256SUMS: exactly the four archives, sorted bytewise (section 4.2)
(
  cd "$out"
  for f in $(printf '%s\n' fil-"$V"-*.tar.gz | LC_ALL=C sort); do
    printf '%s  %s\n' "$(sha256 "$f")" "$f"
  done > SHA256SUMS
  cat SHA256SUMS >&2
)
