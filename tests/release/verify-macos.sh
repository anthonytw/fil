#!/usr/bin/env bash
# The macOS half of `tests/release/just.sh release-verify` (docs/m4/release.md section 10.3). verify.sh calls it; it is
# a script of its own so that the self-test can run it on signed fixtures.
#
#   verify-macos.sh agree  DIR VERSION
#   verify-macos.sh report DIR VERSION --team TEAMID [--pre-sign] [--mac-rebuild FILE]
#
# agree:  the macOS archive equals CI's own (the .ci archive, the one CI attested) in every member and
#         every piece of tar metadata but bin/fil, and the two bin/fil normalize to the same bytes
#         (macho_norm.py). Runs on any OS. This is what ties the archive the owner signed to the
#         attestation; exit status 1 if it does not hold.
# report: who signed bin/fil and what the signature grants, checked from the signature itself
#         (macho_norm.py, any OS) and, where codesign exists, by `codesign --verify --strict`:
#         every slice has the identifier fil; before the owner's signing (--pre-sign) the signature is
#         CI's ad hoc one; after it, it is a Developer ID signature with exactly the hardened runtime
#         flag, the Team ID TEAMID (from .github/macos-team-id on main), no entitlements and a
#         certificate chain. With --mac-rebuild FILE, a macOS fil built from the tag: both normalize
#         to the same bytes or this says so and how they differ. What this machine could not check is
#         listed. Exit status 1 if any check failed.
set -uo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

mode=${1:-}
dir=${2:-}
version=${3:-}
[ -n "$mode" ] && [ -d "$dir" ] && [ -n "$version" ] || die "usage: verify-macos.sh agree|report DIR VERSION [...]"
shift 3
team='' presign=0 rebuild=''
while [ $# -gt 0 ]; do
  case $1 in
    --team) team=$2; shift 2 ;;
    --pre-sign) presign=1; shift ;;
    --mac-rebuild) rebuild=$2; shift 2 ;;
    *) die "unknown option: $1" ;;
  esac
done
name=fil-$version-universal-apple-darwin
main=$dir/$name.tar.gz
ci=$dir/$(ci_archive_name "$version")
fails=0
fail() { echo "release-verify: FAIL: $*" >&2; fails=$((fails + 1)); }
ok() { echo "release-verify: ok: $*" >&2; }
note() { echo "release-verify: note: $*" >&2; }

case $mode in
  agree)
    [ -f "$ci" ] || die "the release has no $(basename "$ci") (CI's own macOS archive, section 4.1)"
    [ -f "$main" ] || die "the release has no $name.tar.gz"
    if cmp -s "$ci" "$main"; then
      ok "$name.tar.gz is CI's archive, byte for byte (not yet signed)"
    elif out=$(macho archives "$ci" "$main" 2>&1); then
      ok "$name.tar.gz is CI's archive with its macOS binary signed: $out"
    else
      echo "release-verify: $out" >&2
      exit 1
    fi
    exit 0
    ;;
  report) ;;
  *) die "unknown mode: $mode" ;;
esac

t=$(mktemp -d "${TMPDIR:-/tmp}/fil-vmac.XXXXXX")
trap 'rm -rf "$t"' EXIT
tar -xzOf "$main" "$name/bin/fil" > "$t/fil" 2> /dev/null || { fail "cannot read $name/bin/fil"; exit 1; }
chmod 755 "$t/fil"
if ! info=$(macho info "$t/fil" 2>&1); then
  fail "the macOS binary's signature cannot be parsed ($info)"
  exit 1
fi
echo "$info" | sed 's/^/release-verify:   /' >&2
kinds=$(printf '%s\n' "$info" | sed -n 's/.* kind=\([^ ]*\).*/\1/p' | sort -u | tr '\n' ' ')
if printf '%s\n' "$info" | grep -qv ' identifier=fil '; then fail "a slice of the macOS binary does not have the identifier fil"; fi
case $kinds in
  "adhoc ")
    if [ "$presign" = 1 ]; then
      note "the macOS binary is CI's ad hoc build: not yet signed by the owner (tests/release/just.sh release-sign-macos)"
    else
      fail "the macOS binary is only ad hoc signed: it is not Developer-ID signed and notarized (tests/release/just.sh release-sign-macos)"
    fi
    ;;
  "signed ")
    [ -n "$team" ] || { fail "no Team ID to check the signature against (.github/macos-team-id)"; exit 1; }
    if [ "$(printf '%s\n' "$info" | grep -c ' flags=0x10000 ')" = "$(printf '%s\n' "$info" | grep -c .)" ]; then ok "macOS binary: flags are exactly the hardened runtime (0x10000) in every slice"; else fail "macOS binary: flags are not exactly the hardened runtime (0x10000)"; fi
    if [ "$(printf '%s\n' "$info" | grep -c " team=$team ")" = "$(printf '%s\n' "$info" | grep -c .)" ]; then ok "macOS binary: Team ID $team in every slice"; else fail "macOS binary: the Team ID is not $team in every slice"; fi
    if printf '%s\n' "$info" | grep -q ' entitlements=yes'; then fail "macOS binary: the signature carries entitlements (none are granted)"; else ok "macOS binary: no entitlements"; fi
    ;;
  *) fail "the macOS binary's slices are signed in different ways or not at all ($kinds)" ;;
esac

cs_ok=0
command -v "${FIL_CODESIGN:-codesign}" > /dev/null 2>&1 && cs_ok=1
if [ "$cs_ok" = 1 ]; then
  if cs --verify --strict "$t/fil" > "$t/verify.log" 2>&1; then ok "codesign --verify --strict"; else cat "$t/verify.log" >&2; fail "codesign --verify --strict failed on the macOS binary"; fi
  if [ "$kinds" = "signed " ]; then
    if cs --verify --strict -R="$(dev_id_requirement "$team")" "$t/fil" > "$t/req.log" 2>&1; then
      ok "macOS binary: the designated requirement holds (Developer ID CA, Developer ID Application leaf, certificate OU $team)"
    else
      cat "$t/req.log" >&2
      fail "macOS binary: it does not satisfy 'Developer ID Application certificate whose OU is $team'"
    fi
  fi
  cs -dvv "$t/fil" > "$t/dvv.log" 2>&1 || true
  grep -E '^(Authority|TeamIdentifier|Timestamp)=' "$t/dvv.log" | sed 's/^/release-verify:   /' >&2 || true
  if [ "$kinds" = "signed " ]; then
    grep -q '^Timestamp=' "$t/dvv.log" && ok "macOS binary: secure timestamp" || fail "macOS binary: no secure timestamp"
    grep -q '^Authority=Developer ID Application:' "$t/dvv.log" && ok "macOS binary: signed by a Developer ID Application certificate" || fail "macOS binary: no Developer ID Application authority"
  fi
else
  note "not checked here (no codesign; needs a Mac): codesign --verify --strict, the designated requirement that ties the signature to a Developer ID certificate whose OU is the Team ID, the certificate chain and the signing time"
fi
[ "$kinds" != "signed " ] || note "not checked by any script: that Apple notarized it (a bare binary has no ticket to staple; Gatekeeper asks Apple online the first time it runs)"

h=$(macho sha256 "$t/fil") || { fail "the macOS binary cannot be normalized"; exit 1; }
note "macOS binary, signature normalized away: sha256 $h"
if [ -n "$rebuild" ]; then
  [ -f "$rebuild" ] || die "no --mac-rebuild file: $rebuild"
  if out=$(macho same "$t/fil" "$rebuild" 2>&1); then
    ok "macOS binary: BYTE-IDENTICAL to $rebuild once the signature is normalized away ($out)"
  else
    echo "$out" | sed 's/^/release-verify:   /' >&2
    fail "macOS binary: NOT byte-identical to $rebuild once the signature is normalized away; everything but the signature was compared, so the code differs"
  fi
else
  note "no --mac-rebuild FILE: the macOS binary was not compared with a build of your own"
fi
[ "$fails" = 0 ]
