#!/usr/bin/env bash
# `just release-check`: everything a release candidate must satisfy before anyone signs it
# (docs/m4/release.md; docs/m4/plan.md, REL).
#
#   check.sh [--dry-run | --tag vX.Y.Z] [--complete] [--no-tree] [--no-exec] DIR
#
#   DIR         the five archives (the macOS one twice: CI's own and the one the owner signs), the four .deb and .rpm packages and SHA256SUMS that package.sh
#               wrote (or a downloaded release)
#   --dry-run   a build of a branch, not of a release: the tag and the changelog are not checked
#   --tag TAG   the tag being released: it must be v<Cargo.toml version>, CHANGELOG.md must have a
#               section for the version, and a patch release must keep its minor's wire
#   --complete  accepted and implied: every file of the section 4.3 layout is always required
#   --no-tree   skip the checks that read the dependency graph (`just release-deps`)
#   --no-exec   run NOTHING that came from DIR: no `cargo metadata` in the extracted source archive
#               (its .cargo/config.toml can name a rustc wrapper, which cargo runs) and no
#               `--version` of a downloaded binary. The files are only unpacked, hashed, grepped and
#               inspected with file(1). `release-verify` uses this mode on the owner's machine.
#
# Exit status 0 only if every check passed.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

dry=0 tag='' complete=0 tree=1 exec=1 dir=''
while [ $# -gt 0 ]; do
  case $1 in
    --dry-run) dry=1; shift ;;
    --tag) tag=$2; shift 2 ;;
    --complete) complete=1; shift ;; # kept for the callers; the whole layout is required either way
    --no-tree) tree=0; shift ;;
    --no-exec) exec=0; shift ;;
    -*) die "unknown option: $1" ;;
    *) dir=$1; shift ;;
  esac
done
[ "$complete" = 1 ] || true
[ -n "$dir" ] || die "usage: check.sh [--dry-run | --tag vX.Y.Z] [--complete] [--no-tree] [--no-exec] DIR"
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
packages=$(package_names "$V" | tr '\n' ' ')
names=$(printf '%s\n' "fil-$V-aarch64-linux-musl.tar.gz" "fil-$V-source.tar.gz" "fil-$V-universal-apple-darwin.tar.gz" "$(ci_archive_name "$V")" "fil-$V-x86_64-linux-musl.tar.gz" $packages | LC_ALL=C sort | tr '\n' ' ')
if [ ! -f "$sums" ]; then
  fail "no SHA256SUMS in $dir"
else
  if [ "$(cat "$sums")" = "$(
    for n in $names; do
      [ -f "$dir/$n" ] && printf '%s  %s\n' "$(sha256 "$dir/$n")" "$n"
    done
  )" ]; then
    ok "SHA256SUMS lists exactly the five archives and the four packages, sorted, in sha256sum format"
  else
    fail "SHA256SUMS is not exactly the five archives and the four packages ($names), bytewise sorted, 'hash  name' lines (it is: $(tr '\n' '|' < "$sums"))"
  fi
  if sums_check "$dir" SHA256SUMS; then ok "sha256sum -c SHA256SUMS"; else fail "sha256sum -c SHA256SUMS"; fi
  [ -z "$(tail -c 1 "$sums")" ] || fail "SHA256SUMS does not end with a newline"
  grep -q "$(printf '\r')" "$sums" && fail "SHA256SUMS has CR line endings"
fi
extras=
for f in "$dir"/*; do
  b=${f##*/}
  case " SHA256SUMS SHA256SUMS.sig SHA256SUMS.asc $names " in *" $b "*) ;; *) extras="$extras $b" ;; esac
done
[ -z "$extras" ] || note "other files in $dir (not part of the release):$extras"

# ---- the archives
# Layout (section 4.3): every file of it, DOC's (man pages, completions, examples, changelog) too.
required="bin/fil
libexec/fil/remote/x86_64-linux/fil
libexec/fil/remote/aarch64-linux/fil
libexec/fil/remote/SHA256SUMS
share/doc/fil/README.md
share/doc/fil/LICENSE
share/doc/fil/THIRD-PARTY-NOTICES.md
share/doc/fil/CHANGELOG.md
share/doc/fil/config.example.toml
share/doc/fil/daemon.example.toml
share/doc/fil/policy.example.toml
share/man/man1/fil.1
share/man/man5/fil-config.5
share/man/man5/fil-policy.5
share/bash-completion/completions/fil
share/zsh/site-functions/_fil
share/fish/vendor_completions.d/fil.fish"

# Host names and addresses that appear in the tree's test directories. A release binary must carry
# none of them (docs/m4/plan.md, REL). Loopback, the unspecified address, broadcast and multicast
# are real defaults and are skipped; so is tests/release (its own scratch values).
fixture_dirs=
for d in "$ROOT"/crates/*/tests "$ROOT"/crates/*/fuzz "$ROOT"/crates/*/examples "$ROOT"/tests/realnet "$ROOT"/tests/macos; do
  if [ -d "$d" ]; then fixture_dirs="$fixture_dirs $d"; fi
done
fixtures=
if [ -n "$fixture_dirs" ]; then
  # shellcheck disable=SC2086
  fixtures=$(
    {
      grep -rhIoE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' $fixture_dirs 2> /dev/null | grep -vE '^(127\.|0\.0\.0\.0$|255\.255\.255\.255$|22[4-9]\.|23[0-9]\.)' || true
      grep -rhIoE '[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.(example|test|invalid|local|lab|home|lan|internal)|[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.example\.(com|net|org)' $fixture_dirs 2> /dev/null
      true
    } | LC_ALL=C sort -u
  )
fi
fixture_leaks() { # BINARY: the fixture strings found in it, as whole tokens (a token is [A-Za-z0-9.-]+)
  [ -n "$fixtures" ] || return 0
  LC_ALL=C grep -aoE '[A-Za-z0-9.-]{4,}' "$1" | sed 's/[.-]*$//' | LC_ALL=C sort -u > "$work/tokens"
  printf '%s\n' "$fixtures" | LC_ALL=C sort -u | LC_ALL=C comm -12 - "$work/tokens" | tr '\n' ' '
}

is_gnu_tar() { tar --version 2> /dev/null | grep -q 'GNU tar'; }

x86_bin='' aarch_bin=''
# The install bootstrap this release's binaries must hold (common.sh, from the golden).
boot=$(release_bootstrap "$V" "$major" "$rev")
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
    for f in Cargo.toml Cargo.lock .cargo/config.toml vendor LICENSE THIRD-PARTY-NOTICES.md; do
      [ -e "$root/$f" ] || fail "$name.tar.gz lacks $f"
    done
    if [ "$exec" = 0 ]; then
      note "--no-exec: $name.tar.gz is not resolved with cargo (the rebuild compares it byte for byte)"
    elif [ -d "$root/vendor" ] && [ -f "$root/Cargo.lock" ]; then
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
  # vt100 (the test emulator) must not be in the binary, whatever the dependency graph said.
  if grep -aqE 'registry/src/[^/]+/vt100-' "$b"; then fail "$name: bin/fil contains source paths of the vt100 crate"; fi
  # The legal lines of `--version` (section 7 of release.md; GPLv3's Appropriate Legal Notices) are
  # compiled in; read from the file, because a downloaded binary is not run under --no-exec.
  if grep -aqF -- 'Copyright (C) 2026 Anthony Wertz' "$b" && grep -aqF -- 'License GPL-3.0-or-later <https://gnu.org/licenses/gpl.html>' "$b" && grep -aqF -- 'There is NO WARRANTY, to the extent permitted by law.' "$b"; then
    ok "$name: bin/fil carries the copyright, license and warranty lines of --version"
  else
    fail "$name: bin/fil lacks the copyright, license or warranty line of --version (crates/fil/src/cli.rs)"
  fi
  # A tripwire on the remote commands (DESIGN.md section 8.2; M4 OQ-1, approved by the owner
  # 2026-11-04): the install bootstrap is in the binary, byte for byte as the golden pins it for
  # this version and wire, and every `sh -c` in the binary (which also catches `bash -c`, `zsh -c`
  # and the like) is the start of that text. A universal binary holds one per architecture. This
  # finds a second shell-wrapped literal only: a command built from separate argv words has no
  # such text, which is why crates/fil/tests/remote_commands.rs checks the behavior (every
  # subcommand against a recording fake ssh).
  if [ -z "$boot" ]; then
    fail "$name: no install bootstrap to compare with (crates/fil-transport/src/pipe/remote-commands.txt)"
  else
    n_boot=$({ grep -aoF -- "$boot" "$b" || true; } | wc -l | tr -d ' ')
    n_sh=$({ grep -aoF -- 'sh -c' "$b" || true; } | wc -l | tr -d ' ')
    if [ "$n_boot" -ge 1 ] && [ "$n_sh" = "$n_boot" ]; then
      ok "$name: bin/fil holds the approved install bootstrap ($n_boot) and no other shell command line"
    elif [ "$n_boot" -lt 1 ]; then
      fail "$name: bin/fil does not hold the approved install bootstrap for $V on wire $major.$rev (remote-commands.txt)"
    else
      fail "$name: bin/fil has $n_sh 'sh -c' texts but $n_boot copies of the approved install bootstrap: a remote command line other than the pinned one (first: $({ grep -aoE '[ -~]{0,30}sh -c[ -~]{0,30}' "$b" || true; } | head -n 3 | tr '\n' ' '))"
    fi
  fi
  # Fixture hosts and addresses (the list is derived from the test directories of the tree).
  leak=$(fixture_leaks "$b")
  if [ -n "$leak" ]; then fail "$name: bin/fil contains test-fixture host names or addresses: $leak"; else ok "$name: bin/fil has none of the $(printf '%s\n' "$fixtures" | grep -c .) test-fixture host names and addresses"; fi
  # The remap must have been applied: a dependency's panic location is always in the binary and
  # must read $REMAP_CARGO_HOME/registry/..., never the builder's cargo home.
  if grep -aqF -- "$REMAP_CARGO_HOME/registry/src/" "$b"; then ok "$name: bin/fil carries remapped source paths ($REMAP_CARGO_HOME/registry/...)"; else fail "$name: bin/fil has no $REMAP_CARGO_HOME/registry/src/ path: the path remap was not applied"; fi
  [ "$hit" = 0 ] && ok "$name: bin/fil has no builder path and no FIL_TEST_ string"
  if (cd "$ROOT" && just no-test-hooks "$b") > "$work/nth.log" 2>&1; then ok "$name: no-test-hooks"; else cat "$work/nth.log" >&2; fail "$name: no-test-hooks"; fi
  desc=$(file "$b")
  case $label in
    x86_64-linux-musl | aarch64-linux-musl)
      if grep -q 'not stripped' <<< "$desc" || ! grep -q 'stripped' <<< "$desc"; then fail "$name: bin/fil is not stripped (debug info or symbols): $desc"; fi ;;
  esac
  case $label in
    x86_64-linux-musl) grep -Eq 'ELF 64-bit.*x86-64.*(statically linked|static-pie linked)' <<< "$desc" && ok "$name: bin/fil is a static x86_64 ELF" || fail "$name: bin/fil is not a static x86_64 ELF: $desc" ;;
    aarch64-linux-musl) grep -Eq 'ELF 64-bit.*ARM aarch64.*(statically linked|static-pie linked)' <<< "$desc" && ok "$name: bin/fil is a static aarch64 ELF" || fail "$name: bin/fil is not a static aarch64 ELF: $desc" ;;
    universal-apple-darwin) grep -Eq 'Mach-O universal binary with 2 architectures.*x86_64.*arm64|Mach-O universal binary with 2 architectures.*arm64.*x86_64' <<< "$desc" && ok "$name: bin/fil is a universal Mach-O (x86_64, arm64)" || fail "$name: bin/fil is not a two-architecture Mach-O: $desc" ;;
  esac
done

# The macOS archive is signed after CI (section 10.3); CI's own, unchanged, is the .ci archive. Before
# signing the two are the same file. After, they differ in bin/fil alone, and the two bin/fil are the
# same code: they normalize (macho_norm.py) to the same bytes.
ci=$dir/$(ci_archive_name "$V")
mac=$dir/fil-$V-universal-apple-darwin.tar.gz
if [ ! -f "$ci" ]; then
  fail "missing archive $(ci_archive_name "$V") (CI's own macOS archive, section 4.1)"
elif [ -f "$mac" ]; then
  if cmp -s "$ci" "$mac"; then
    ok "fil-$V-universal-apple-darwin.tar.gz is CI's archive, not yet signed"
  elif macho archives "$ci" "$mac" > "$work/macho.log" 2>&1; then
    ok "fil-$V-universal-apple-darwin.tar.gz is CI's archive with the macOS binary signed: $(cat "$work/macho.log")"
  else
    cat "$work/macho.log" >&2
    fail "fil-$V-universal-apple-darwin.tar.gz differs from CI's archive by more than the signature of bin/fil"
  fi
fi

# The notices (section 7): every binary archive carries the THIRD-PARTY-NOTICES.md of the tree it
# was built from, which is the one in the source archive (`just notices` checked it is current).
src_notices=$work/x-source/fil-$V-source/THIRD-PARTY-NOTICES.md
if [ -f "$src_notices" ]; then
  for label in x86_64-linux-musl aarch64-linux-musl universal-apple-darwin; do
    n=$work/x-$label/fil-$V-$label/share/doc/fil/THIRD-PARTY-NOTICES.md
    [ -f "$n" ] || continue
    cmp -s "$n" "$src_notices" && ok "fil-$V-$label: THIRD-PARTY-NOTICES.md is the source archive's" \
      || fail "fil-$V-$label: share/doc/fil/THIRD-PARTY-NOTICES.md differs from the source archive's THIRD-PARTY-NOTICES.md"
  done
fi

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

# ---- the .deb and .rpm packages (section 4.1; docs/m4/packaging.md): read by pkg_inspect.py, which
# runs nothing from them. Each holds exactly the files of its Linux archive, byte for byte, at the
# places package_path gives, owned by root, 0755 or 0644, under the name fil (PACKAGE_NAME), the
# version and the architecture its file name says, with no maintainer script and no dependency, and
# an rpm owns no directory but its own. An rpm may carry the owner's signature (section 10.4) or not
# (the draft, before signing): which, is reported here and required by release-verify.
package_version_ok "$V" || fail "version '$V' is not X.Y.Z or X.Y.Z-PRE (PRE of [0-9A-Za-z.]): nfpm cannot package it"
pkg_version=$(package_version "$V")
for kind in deb rpm; do for a in x86_64 aarch64; do
  p=$(package_file "$kind" "$a" "$V")
  arch=$(package_arch "$kind" "$a")
  label=$a-linux-musl
  aroot=$work/x-$label/fil-$V-$label
  if [ ! -f "$dir/$p" ]; then
    fail "missing package $p"
    continue
  fi
  px=$work/p-$p
  if ! python3 -I "$RELEASE_DIR/pkg_inspect.py" "$dir/$p" "$px" > "$work/p.list" 2> "$work/p.err"; then
    fail "$p cannot be read: $(cat "$work/p.err")"
    continue
  fi
  meta="$(sed -n 's/^name //p' "$work/p.list") $(sed -n 's/^version //p' "$work/p.list") $(sed -n 's/^arch //p' "$work/p.list")"
  if [ "$meta" = "$PACKAGE_NAME $pkg_version $arch" ]; then ok "$p is $PACKAGE_NAME $pkg_version for $arch"; else fail "$p says '$meta', not '$PACKAGE_NAME $pkg_version $arch'"; fi
  extra=$(grep -E '^(script|depends) ' "$work/p.list" | tr '\n' ';' || true)
  if [ -z "$extra" ]; then ok "$p has no maintainer script and no dependency"; else fail "$p has maintainer scripts or dependencies: $extra"; fi
  if [ "$kind" = rpm ]; then
    dirs=$(sed -n 's/^dir [^ ]* [^ ]* //p' "$work/p.list" | LC_ALL=C sort | tr '\n' ' ')
    wantdirs=$(cd "$aroot" 2> /dev/null && find libexec/fil share/doc/fil -type d | sed -e 's|^share/doc/fil|/usr/share/doc/'"$PACKAGE_NAME"'|' -e 's|^libexec|/usr/libexec|' | LC_ALL=C sort | tr '\n' ' ')
    if [ "$dirs" = "$wantdirs" ]; then ok "$p owns only its own directories"; else fail "$p owns the directories '$dirs', not '$wantdirs'"; fi
    note "$p: signed $(sed -n 's/^signed //p' "$work/p.list")"
  fi
  aroot=$work/x-$label/fil-$V-$label
  if [ ! -d "$aroot" ]; then
    fail "$p: no archive fil-$V-$label to compare it with"
    continue
  fi
  want=$(
    cd "$aroot"
    find . -type f | sed 's|^\./||' | LC_ALL=C sort | while IFS= read -r f; do
      dst=$(package_path "$kind" "$f") || dst="(no place for $f)"
      if [ -x "$f" ]; then m=755; else m=644; fi
      echo "file $m root:root $dst"
    done | LC_ALL=C sort
  )
  have=$(grep '^file ' "$work/p.list" | LC_ALL=C sort)
  if [ "$have" = "$want" ]; then
    ok "$p holds exactly the files of fil-$V-$label.tar.gz, root-owned, at their places"
  else
    fail "$p's files are not those of fil-$V-$label.tar.gz (< package, > archive): $(diff <(printf '%s\n' "$have") <(printf '%s\n' "$want") | grep '^[<>]' | head -n 6 | tr '\n' ' ')"
  fi
  bad=$({ grep "^dir " "$work/p.list" || true; } | awk '$2 != "755" || $3 != "root:root"' | head -n 3)
  [ -z "$bad" ] || fail "$p has a directory that is not 0755 root:root: $bad"
  differ=
  while IFS= read -r f; do
    dst=$(package_path "$kind" "$f") || continue
    cmp -s "$aroot/$f" "$px$dst" || differ="$differ $f"
  done < <(cd "$aroot" && find . -type f | sed 's|^\./||')
  if [ -z "$differ" ]; then ok "$p: every file is byte for byte the archive's"; else fail "$p: files differ from the archive's:$differ"; fi
done; done

# ---- run the binary where it can run (section 2): the first line is FROZEN
smoke() {
  local b=$1 first expect
  first=$("$b" --version 2> /dev/null | head -n 1) || true
  expect="fil $V (wire $major.$rev)"
  if [ "$first" = "$expect" ]; then ok "$b --version: $first"; else fail "$b --version printed '$first', not '$expect'"; fi
  if "$b" --version 2> /dev/null | tail -n +2 | grep -qF '(wire '; then fail "$b --version: a line after the first contains '(wire '"; fi
}
if [ "$exec" = 0 ]; then
  note "--no-exec: no downloaded binary was run"
else
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) [ -n "$x86_bin" ] && smoke "$x86_bin" ;;
    Linux-aarch64) [ -n "$aarch_bin" ] && smoke "$aarch_bin" ;;
  esac
fi
if [ "$exec" = 1 ] && [ "$(uname -s)" = Darwin ] && [ -f "$work/x-universal-apple-darwin/fil-$V-universal-apple-darwin/bin/fil" ]; then
  smoke "$work/x-universal-apple-darwin/fil-$V-universal-apple-darwin/bin/fil"
fi

if [ "$fails" -gt 0 ]; then
  echo "release-check: $fails check(s) FAILED" >&2
  exit 1
fi
echo "release-check: all checks passed" >&2
