#!/usr/bin/env bash
# Digests handed from one release.yml job to the next as job outputs (docs/m4/release.md section 9.1).
# An output is set by the job that computed it, and no other job can change it, unlike an artifact,
# which a later job with the runner's token could replace. So the macOS binary, which no rebuild
# reproduces, and SHA256SUMS, which ties every release file, are each checked against the digest of
# the job that made them, right where the next owned step reads them.
#
#   digest.sh out NAME FILE          append NAME=<SHA-256 of FILE> to $GITHUB_OUTPUT (and print it)
#   digest.sh check WANT FILE...     every FILE has SHA-256 WANT (64 lowercase hex), else fail
#   digest.sh check-tar WANT TGZ...  the one TOP/bin/fil inside each .tar.gz has SHA-256 WANT
#   digest.sh listed DIR             DIR holds exactly SHA256SUMS and the files it lists (regular
#                                    files), so nothing unlisted is attested or uploaded
#   digest.sh tar-safe TAR           every member of TAR is a regular file or a directory, with no
#                                    leading / and no .. component, so unpacking it writes only
#                                    plain files under the target directory
#
# Exit status 0 only if every check passed.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

want_ok() { [[ $1 =~ ^[0-9a-f]{64}$ ]] || die "digest: '$1' is not a SHA-256 (64 lowercase hex): an output that was never set reads as empty"; }

cmd=${1:-}
[ $# -gt 0 ] && shift
case $cmd in
  out)
    [ $# = 2 ] || die "usage: digest.sh out NAME FILE"
    [[ $1 =~ ^[a-z0-9_]+$ ]] || die "digest: output name '$1' is not [a-z0-9_]+"
    [ -f "$2" ] || die "digest: no file $2"
    [ -n "${GITHUB_OUTPUT:-}" ] || die "digest: GITHUB_OUTPUT is not set (this runs in a workflow step)"
    d=$(sha256 "$2")
    printf '%s=%s\n' "$1" "$d" >> "$GITHUB_OUTPUT"
    echo "digest: $1 = $d ($2)" >&2
    ;;
  check)
    [ $# -ge 2 ] || die "usage: digest.sh check WANT FILE..."
    want=$1
    shift
    want_ok "$want"
    for f in "$@"; do
      [ -f "$f" ] || die "digest: no file $f"
      got=$(sha256 "$f")
      [ "$got" = "$want" ] || die "digest: $f is $got, not $want, the digest its build job recorded: it changed between the jobs"
      echo "digest: $f is $want, as recorded" >&2
    done
    ;;
  check-tar)
    [ $# -ge 2 ] || die "usage: digest.sh check-tar WANT TGZ..."
    want=$1
    shift
    want_ok "$want"
    t=$(mktemp -d)
    trap 'rm -rf "$t"' EXIT
    for a in "$@"; do
      [ -f "$a" ] || die "digest: no archive $a"
      members=$(tar -tzf "$a" | grep -E '^[^/]+/bin/fil$' || true)
      [ "$(printf '%s\n' "$members" | grep -c .)" = 1 ] || die "digest: $a does not hold exactly one TOP/bin/fil"
      tar -xzOf "$a" "$members" > "$t/fil"
      got=$(sha256 "$t/fil")
      [ "$got" = "$want" ] || die "digest: $a's $members is $got, not $want, the digest its build job recorded: it changed between the jobs"
      echo "digest: $a's $members is $want, as recorded" >&2
    done
    ;;
  listed)
    [ $# = 1 ] || die "usage: digest.sh listed DIR"
    dir=$1
    [ -f "$dir/SHA256SUMS" ] || die "digest: no $dir/SHA256SUMS"
    want=$({ echo SHA256SUMS; awk '{ print $2 }' "$dir/SHA256SUMS"; } | LC_ALL=C sort)
    have=$(cd "$dir" && LC_ALL=C ls -A | LC_ALL=C sort)
    [ "$have" = "$want" ] || die "digest: $dir holds other files than SHA256SUMS and the files it lists: has '$(printf '%s' "$have" | tr '\n' ' ')', lists '$(printf '%s' "$want" | tr '\n' ' ')'"
    for f in $have; do
      [ -f "$dir/$f" ] && [ ! -L "$dir/$f" ] || die "digest: $dir/$f is not a regular file"
    done
    echo "digest: $dir holds exactly SHA256SUMS and the $(($(printf '%s\n' "$want" | wc -l) - 1)) files it lists" >&2
    ;;
  tar-safe)
    [ $# = 1 ] || die "usage: digest.sh tar-safe TAR"
    [ -f "$1" ] || die "digest: no archive $1"
    python3 -I - "$1" << 'EOF' || die "digest: $1 is not safe to unpack (above)"
import sys, tarfile
bad = []
with tarfile.open(sys.argv[1]) as t:
    for m in t.getmembers():
        parts = m.name.split("/")
        if m.name.startswith("/"):
            bad.append(f"{m.name!r}: an absolute path")
        elif ".." in parts:
            bad.append(f"{m.name!r}: a .. component")
        elif not (m.isreg() or m.isdir()):
            bad.append(f"{m.name!r}: not a regular file or a directory (type {m.type!r})")
for b in bad:
    print(f"release: digest: {b}", file=sys.stderr)
sys.exit(1 if bad else 0)
EOF
    echo "digest: $1 holds only regular files and directories, inside it" >&2
    ;;
  *) die "usage: digest.sh out NAME FILE | check WANT FILE... | check-tar WANT TGZ... | listed DIR | tar-safe TAR" ;;
esac
