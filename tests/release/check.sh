#!/usr/bin/env bash
# `just release-check`: everything a release candidate must satisfy before anyone signs it
# (docs/m4/release.md; docs/m4/plan.md, REL).
#
#   check.sh [--dry-run | --tag vX.Y.Z] [--complete] [--no-tree] DIR
#
#   DIR         the four archives and SHA256SUMS that package.sh wrote (or a downloaded release)
#   --dry-run   a build of a branch, not of a release: the tag and the changelog are not checked
#   --tag TAG   the tag being released: it must be v<Cargo.toml version>, CHANGELOG.md must have a
#               section for the version, and a patch release must keep its minor's wire
#   --complete  every file of the section 4.3 layout must be present. Without it, the files that
#               LIC and DOC add later are listed as pending (a dry run before they land)
#   --no-tree   skip the checks that read the dependency graph (`just release-deps`)
#
# Exit status 0 only if every check passed.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

dry=0 tag='' complete=0 tree=1 dir=''
while [ $# -gt 0 ]; do
  case $1 in
    --dry-run) dry=1; shift ;;
    --tag) tag=$2; shift 2 ;;
    --complete) complete=1; shift ;;
    --no-tree) tree=0; shift ;;
    -*) die "unknown option: $1" ;;
    *) dir=$1; shift ;;
  esac
done
[ -n "$dir" ] || die "usage: check.sh [--dry-run | --tag vX.Y.Z] [--complete] [--no-tree] DIR"
if [ "$dry" = 0 ] && [ -z "$tag" ]; then
  die "name the release with --tag vX.Y.Z (or pass --dry-run for a build that is not a release)"
fi
[ -d "$dir" ] || die "$dir is not a directory"
dir=$(cd "$dir" && pwd)

fails=0
fail() {
  echo "release-check: FAIL: $*" >&2
  fails=$((fails + 1))
}
ok() { echo "release-check: ok: $*" >&2; }
note() { echo "release-check: note: $*" >&2; }

V=$(cargo_version)
[ -n "$V" ] || die "no version in Cargo.toml"
case $V in
  [0-9]*) ;;
  *) fail "version '$V' does not start with a digit" ;;
esac
printf '%s' "$V" | grep -Eq '^[0-9][0-9A-Za-z.-]{0,63}$' || fail "version '$V' is not [0-9][0-9A-Za-z.-]{0,63} (docs/m4/release.md section 2)"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# ---- the tag, the changelog and the wire (section 1)
if [ "$dry" = 0 ]; then
  if [ "$tag" = "v$V" ]; then ok "tag $tag equals Cargo.toml version $V"; else fail "tag $tag is not v$V (Cargo.toml)"; fi
  # A heading "## 0.1.0", "## [0.1.0]" or "## v0.1.0", optionally followed by a date.
  vre=$(printf '%s' "$V" | sed 's/[.]/[.]/g')
  if grep -Eq "^##[[:space:]]+\[?v?${vre}\]?([[:space:]]|\$)" "$ROOT/CHANGELOG.md" 2> /dev/null; then
    ok "CHANGELOG.md has a section for $V"
  else
    fail "CHANGELOG.md has no section for $V"
  fi
else
  note "dry run: tag and changelog not checked"
fi
major=$(wire_major) rev=$(wire_revision) last=$(golden_last_revision)
if [ -n "$rev" ] && [ "$rev" = "$last" ]; then
  ok "the wire golden's [history] ends at revision $rev (wire $major.$rev)"
else
  fail "the wire golden's [history] ends at '$last', limits.rs says PROTOCOL_REVISION '$rev'"
fi
if [ "$dry" = 0 ]; then
  # A patch release (X.Y.Z, Z > 0) keeps the wire of the previous tag of its minor.
  case $V in
    [0-9]*.[0-9]*.[0-9]*)
      xy=${V%.*}
      z=${V##*.}
      if [ "$z" -gt 0 ] 2> /dev/null; then
        prev=
        for t in $(git -C "$ROOT" tag -l "v$xy.*" | sort -t. -k3 -n); do
          [ "$t" = "v$V" ] && continue
          prev=$t
        done
        if [ -z "$prev" ]; then
          fail "patch release $V but no earlier tag v$xy.* exists to compare the wire with"
        else
          prev_last=$(git -C "$ROOT" show "$prev:crates/fil-proto/tests/golden/wire.txt" | awk '/^\[history\]/ { h = 1; next } h && /^[0-9]+ = / { l = $1 } END { print l }')
          prev_major=$(git -C "$ROOT" show "$prev:crates/fil-proto/src/limits.rs" | sed -n 's/^pub const PROTOCOL_MAJOR: u16 = \([0-9][0-9]*\);.*/\1/p')
          if [ "$prev_major.$prev_last" = "$major.$last" ]; then
            ok "patch release $V keeps the wire $major.$last of $prev"
          else
            fail "patch release $V changes the wire ($prev: $prev_major.$prev_last, now $major.$last); a wire change needs a new minor"
          fi
        fi
      fi
      ;;
  esac
fi

# ---- the toolchain pin (section 1): rust-version is the pinned toolchain
rv=$(cargo_rust_version)
tc=$(toolchain_channel)
if [ -n "$rv" ] && [ "$rv" = "$tc" ]; then ok "rust-version $rv is the pinned toolchain"; else fail "rust-version '$rv' differs from rust-toolchain.toml channel '$tc'"; fi

# ---- the tree: the dependency graph of the release binary
if [ "$tree" = 1 ]; then
  if (cd "$ROOT" && just release-deps) > "$work/deps.log" 2>&1; then ok "release-deps (no vt100, no test-hooks in the release graph)"; else
    cat "$work/deps.log" >&2
    fail "release-deps"
  fi
else
  note "--no-tree: release-deps skipped"
fi

# ---- SHA256SUMS (section 4.2)
sums=$dir/SHA256SUMS
names="fil-$V-aarch64-linux-musl.tar.gz fil-$V-source.tar.gz fil-$V-universal-apple-darwin.tar.gz fil-$V-x86_64-linux-musl.tar.gz"
if [ ! -f "$sums" ]; then
  fail "no SHA256SUMS in $dir"
else
  if [ "$(cat "$sums")" = "$(
    for n in $names; do
      [ -f "$dir/$n" ] && printf '%s  %s\n' "$(sha256 "$dir/$n")" "$n"
    done
  )" ]; then
    ok "SHA256SUMS lists exactly the four archives, sorted, in sha256sum format"
  else
    fail "SHA256SUMS is not exactly the four archives ($names), bytewise sorted, 'hash  name' lines (it is: $(tr '\n' '|' < "$sums"))"
  fi
  if sums_check "$dir" SHA256SUMS; then ok "sha256sum -c SHA256SUMS"; else fail "sha256sum -c SHA256SUMS"; fi
  [ -z "$(tail -c 1 "$sums")" ] || fail "SHA256SUMS does not end with a newline"
  grep -q "$(printf '\r')" "$sums" && fail "SHA256SUMS has CR line endings"
fi
extras=
for f in "$dir"/*; do
  b=${f##*/}
  case " SHA256SUMS SHA256SUMS.sig $names " in *" $b "*) ;; *) extras="$extras $b" ;; esac
done
[ -z "$extras" ] || note "other files in $dir (not part of the release):$extras"

# ---- the archives
# Layout (section 4.3). `pending` are files LIC and DOC add; they are required with --complete.
required="bin/fil
libexec/fil/remote/x86_64-linux/fil
libexec/fil/remote/aarch64-linux/fil
libexec/fil/remote/SHA256SUMS
share/doc/fil/README.md
share/doc/fil/LICENSE"
pending="share/doc/fil/CHANGELOG.md
share/doc/fil/THIRD-PARTY-NOTICES.md
share/doc/fil/config.example.toml
share/doc/fil/daemon.example.toml
share/doc/fil/policy.example.toml
share/man/man1/fil.1
share/man/man5/fil-config.5
share/man/man5/fil-policy.5
share/bash-completion/completions/fil
share/zsh/site-functions/_fil
share/fish/vendor_completions.d/fil.fish"

is_gnu_tar() { tar --version 2> /dev/null | grep -q 'GNU tar'; }

x86_bin='' aarch_bin=''
for label in x86_64-linux-musl aarch64-linux-musl universal-apple-darwin source; do
  name=fil-$V-$label
  arc=$dir/$name.tar.gz
  if [ ! -f "$arc" ]; then
    fail "missing archive $name.tar.gz"
    continue
  fi
  tops=$(tar -tzf "$arc" | awk -F/ '{ print $1 }' | sort -u)
  if [ "$tops" = "$name" ]; then ok "$name.tar.gz has exactly one top-level entry, $name"; else fail "$name.tar.gz top-level entries are not exactly '$name' (they are: $(echo "$tops" | tr '\n' ' '))"; fi
  if tar -tzf "$arc" | grep -E -q '^/|(^|/)\.\.(/|$)'; then fail "$name.tar.gz has an absolute or '..' path"; fi
  if is_gnu_tar; then
    # Reproducible recipe (section 4.4): numeric owner 0/0, no group/other write, no setuid.
    bad=$(tar --numeric-owner -tvzf "$arc" | awk '$2 != "0/0" || substr($1, 6, 1) == "w" || substr($1, 9, 1) == "w" || $1 ~ /[st]/ { print $0 }' | head -n 3)
    [ -z "$bad" ] || fail "$name.tar.gz has an entry with an owner other than 0/0, group/other write or a set-id bit: $bad"
  fi
  ex=$work/x-$label
  mkdir -p "$ex"
  tar -xzf "$arc" -C "$ex"
  root=$ex/$name
  if [ "$label" = source ]; then
    for f in Cargo.toml Cargo.lock .cargo/config.toml vendor LICENSE; do
      [ -e "$root/$f" ] || fail "$name.tar.gz lacks $f"
    done
    if [ -d "$root/vendor" ] && [ -f "$root/Cargo.lock" ]; then
      # Builds offline: every package of the lock resolves from vendor/ alone.
      if (cd "$root" && cargo metadata --locked --offline --format-version 1 > /dev/null 2> "$work/meta.log"); then
        ok "$name.tar.gz resolves every dependency from vendor/, offline"
      else
        cat "$work/meta.log" >&2
        fail "$name.tar.gz does not resolve offline"
      fi
    fi
    continue
  fi
  for f in $required; do
    [ -f "$root/$f" ] || fail "$name.tar.gz lacks $f"
  done
  missing=
  for f in $pending; do
    if [ ! -f "$root/$f" ]; then
      if [ "$complete" = 1 ]; then fail "$name.tar.gz lacks $f"; else missing="$missing $f"; fi
    fi
  done
  [ -z "$missing" ] || note "$name.tar.gz: pending (LIC or DOC), not yet in the archive:$missing"
  # The bundle's list is the sha256sum of its two binaries, sorted, relative paths.
  if [ -f "$root/libexec/fil/remote/SHA256SUMS" ]; then
    if [ "$(cat "$root/libexec/fil/remote/SHA256SUMS")" = "$(cd "$root/libexec/fil/remote" && for f in aarch64-linux/fil x86_64-linux/fil; do printf '%s  %s\n' "$(sha256 "$f")" "$f"; done)" ] && sums_check "$root/libexec/fil/remote" SHA256SUMS; then
      ok "$name: libexec/fil/remote/SHA256SUMS checks"
    else
      fail "$name: libexec/fil/remote/SHA256SUMS is not the sorted sha256sum of aarch64-linux/fil and x86_64-linux/fil"
    fi
  fi
  case $label in
    x86_64-linux-musl) x86_bin=$root/bin/fil ;;
    aarch64-linux-musl) aarch_bin=$root/bin/fil ;;
  esac
  # ---- the binary of this archive
  b=$root/bin/fil
  [ -f "$b" ] || continue
  hit=0
  for pat in /Users/ /home/ FIL_TEST_; do
    if grep -aqF -- "$pat" "$b"; then
      fail "$name: bin/fil contains '$pat' ($(grep -aoE "[ -~]{0,40}$(printf '%s' "$pat" | sed 's/[./]/\\&/g')[ -~]{0,40}" "$b" | head -n 2 | tr '\n' ' '))"
      hit=1
    fi
  done
  # The remap must have been applied: a dependency's panic location is always in the binary and
  # must read $REMAP_CARGO_HOME/registry/..., never the builder's cargo home.
  if grep -aqF -- "$REMAP_CARGO_HOME/registry/src/" "$b"; then ok "$name: bin/fil carries remapped source paths ($REMAP_CARGO_HOME/registry/...)"; else fail "$name: bin/fil has no $REMAP_CARGO_HOME/registry/src/ path: the path remap was not applied"; fi
  [ "$hit" = 0 ] && ok "$name: bin/fil has no builder path and no FIL_TEST_ string"
  if (cd "$ROOT" && just no-test-hooks "$b") > "$work/nth.log" 2>&1; then ok "$name: no-test-hooks"; else cat "$work/nth.log" >&2; fail "$name: no-test-hooks"; fi
  desc=$(file "$b")
  case $label in
    x86_64-linux-musl) grep -Eq 'ELF 64-bit.*x86-64.*(statically linked|static-pie linked)' <<< "$desc" && ok "$name: bin/fil is a static x86_64 ELF" || fail "$name: bin/fil is not a static x86_64 ELF: $desc" ;;
    aarch64-linux-musl) grep -Eq 'ELF 64-bit.*ARM aarch64.*(statically linked|static-pie linked)' <<< "$desc" && ok "$name: bin/fil is a static aarch64 ELF" || fail "$name: bin/fil is not a static aarch64 ELF: $desc" ;;
    universal-apple-darwin) grep -Eq 'Mach-O universal binary with 2 architectures.*x86_64.*arm64|Mach-O universal binary with 2 architectures.*arm64.*x86_64' <<< "$desc" && ok "$name: bin/fil is a universal Mach-O (x86_64, arm64)" || fail "$name: bin/fil is not a two-architecture Mach-O: $desc" ;;
  esac
done

# Byte-identical bundle (section 4.3): in every binary archive, each remote copy is the binary of
# its target as it appears in that target's own archive.
if [ -n "$x86_bin" ] && [ -n "$aarch_bin" ]; then
  for label in x86_64-linux-musl aarch64-linux-musl universal-apple-darwin; do
    r=$work/x-$label/fil-$V-$label/libexec/fil/remote
    [ -d "$r" ] || continue
    cmp -s "$r/x86_64-linux/fil" "$x86_bin" && cmp -s "$r/aarch64-linux/fil" "$aarch_bin" \
      && ok "fil-$V-$label: the remote bundle is byte-identical to the Linux archives' bin/fil" \
      || fail "fil-$V-$label: the remote bundle differs from the Linux archives' bin/fil"
  done
fi

# ---- run the binary where it can run (section 2): the first line is FROZEN
smoke() {
  local b=$1 first expect
  first=$("$b" --version 2> /dev/null | head -n 1) || true
  expect="fil $V (wire $major.$rev)"
  if [ "$first" = "$expect" ]; then ok "$b --version: $first"; else fail "$b --version printed '$first', not '$expect'"; fi
  if "$b" --version 2> /dev/null | tail -n +2 | grep -qF '(wire '; then fail "$b --version: a line after the first contains '(wire '"; fi
}
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) [ -n "$x86_bin" ] && smoke "$x86_bin" ;;
  Linux-aarch64) [ -n "$aarch_bin" ] && smoke "$aarch_bin" ;;
esac
if [ "$(uname -s)" = Darwin ] && [ -f "$work/x-universal-apple-darwin/fil-$V-universal-apple-darwin/bin/fil" ]; then
  smoke "$work/x-universal-apple-darwin/fil-$V-universal-apple-darwin/bin/fil"
fi

if [ "$fails" -gt 0 ]; then
  echo "release-check: $fails check(s) FAILED" >&2
  exit 1
fi
echo "release-check: all checks passed" >&2
