#!/usr/bin/env bash
# `just release-test`: tests of the release scripts themselves (docs/m4/plan.md, REL, "Tests" and
# "Mutation"). Part of `just ci`. Needs git, ssh-keygen, perl and tar; no network, and no Docker
# unless FIL_RELEASE_TEST_DOCKER=1 (then the reproducible-archive tests run too: they pull the
# digest-pinned image of Cross.toml).
#
# Every "mutation" below changes ONE thing in a scratch copy and requires the matching check to
# fail, so a check that quietly stopped checking is a test failure.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

# Scratch git commands must not use the caller's config (global signing, hooks, identity).
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.invalid GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.invalid

tmp=$(mktemp -d)
[ -n "$tmp" ] && [ -d "$tmp" ] || { echo "release-test: no scratch directory" >&2; exit 1; }
trap 'rm -rf "$tmp"' EXIT
passed=0 failed=0

pass() { passed=$((passed + 1)); echo "  ok   $1"; }
flunk() { failed=$((failed + 1)); echo "  FAIL $1"; }
# expect_pass NAME CMD... / expect_fail NAME CMD...: the command's output is shown only on a surprise.
expect_pass() {
  local name=$1
  shift
  if "$@" > "$tmp/out.log" 2>&1; then pass "$name"; else
    flunk "$name (it should have passed)"
    sed 's/^/       | /' "$tmp/out.log"
  fi
}
expect_fail() {
  local name=$1
  shift
  if "$@" > "$tmp/out.log" 2>&1; then
    flunk "$name (it should have failed)"
    sed 's/^/       | /' "$tmp/out.log"
  else pass "$name"; fi
}

# ============================================================== 1. the pins in the tree itself
echo "== the tree"
expect_pass "rust-version is the pinned toolchain" test "$(cargo_rust_version)" = "$(toolchain_channel)"
expect_pass "crates/fil inherits rust-version" grep -q '^rust-version.workspace = true' "$REPO_ROOT/crates/fil/Cargo.toml"
expect_pass "Cross.toml pins the x86_64 image by digest" grep -Eq '^image = "ghcr.io/cross-rs/x86_64-unknown-linux-musl@sha256:[0-9a-f]{64}"' "$REPO_ROOT/Cross.toml"
expect_pass "Cross.toml pins the aarch64 image by digest" grep -Eq '^image = "ghcr.io/cross-rs/aarch64-unknown-linux-musl@sha256:[0-9a-f]{64}"' "$REPO_ROOT/Cross.toml"
# The path remap is in force (trim-paths is not stable in the pinned cargo): the profile key and
# the flags must not both be absent, or the binary carries builder paths.
if grep -Eq '^trim-paths[ ]*=' "$REPO_ROOT/Cargo.toml"; then
  pass "[profile.release] sets trim-paths"
else
  expect_pass "no trim-paths in the profile, so the remap flags are in force" grep -q -- '--remap-path-prefix' "$RELEASE_DIR/common.sh"
fi
for k in 'codegen-units = 1' 'lto = "fat"' 'strip = "symbols"' 'debug = 0'; do
  expect_pass "[profile.release] has $k" grep -q "^$k" "$REPO_ROOT/Cargo.toml"
done

# ============================================================== 2. the workflow lint, and mutations
echo "== workflow lint"
wf=$REPO_ROOT/.github/workflows/release.yml
expect_pass "the release workflow passes the lint" "$RELEASE_DIR/lint-workflow.sh" "$wf"
# mutate_wf NAME PERL_EXPR: the lint must reject the workflow after the substitution.
mutate_wf() {
  local name=$1 expr=$2
  perl -0pe "$expr" "$wf" > "$tmp/wf.yml"
  if cmp -s "$wf" "$tmp/wf.yml"; then
    flunk "mutation '$name' did not change the workflow (the test is stale)"
  else
    expect_fail "mutation: $name" "$RELEASE_DIR/lint-workflow.sh" "$tmp/wf.yml"
  fi
}
mutate_wf "the dry-run build job gets id-token: write" 's/(  build-linux:.*?    permissions:\n      contents: read\n)/$1      id-token: write\n/s'
mutate_wf "the package job gets attestations: write" 's/(  package:.*?    permissions:\n      contents: read\n)/$1      attestations: write\n/s'
mutate_wf "a non-publish job gets contents: write" 's/(  smoke-macos:.*?    permissions:\n      contents: )read/${1}write/s'
mutate_wf "a job drops its permissions block" 's/(  smoke-macos:.*?)    permissions:\n      contents: read\n/$1/s'
mutate_wf "the workflow-level permissions widen" 's/\npermissions:\n  contents: read\n/\npermissions:\n  contents: write\n/'
mutate_wf "publish stops requiring a tag" "s/ && startsWith\\(github.ref, 'refs\\/tags\\/v'\\)//"
mutate_wf "an action is pinned by tag" 's/(actions\/upload-artifact)@[0-9a-f]{40} # v7.0.2/$1\@v7/'
mutate_wf "an action is pinned by a short SHA" 's/(taiki-e\/install-action)\@682ce6c3c696a4cd5168a05f98e51aa02b877941/$1\@682ce6c/'
mutate_wf "a secret is used" 's/(GH_TOKEN: )\$\{\{ github.token \}\}/$1\$\{\{ secrets.RELEASE_TOKEN \}\}/'
mutate_wf "a cache step is added" 's/(      - name: Toolchain \(from rust-toolchain.toml\)\n)/      - uses: actions\/cache\@0123456789abcdef0123456789abcdef01234567\n$1/'
mutate_wf "checkout keeps its token" 's/(  build-macos:.*?    steps:\n      - uses: actions\/checkout\@[0-9a-f]{40} # v7.0.1\n        with:\n)          persist-credentials: false\n/$1          fetch-depth: 1\n/s'
mutate_wf "a pull_request_target trigger" 's/\n  workflow_dispatch:\n/\n  workflow_dispatch:\n  pull_request_target:\n/'
mutate_wf "the push trigger takes branches" 's/(    tags: \["v\*"\]\n)/$1    branches: [main]\n/'
mutate_wf "a non-publish job has permissions: write-all" 's/(  build-linux:.*?    permissions:)\n      contents: read\n/$1 write-all\n/s'
mutate_wf "a non-publish job has flow-form permissions" 's/(  build-linux:.*?    permissions:)\n      contents: read\n/$1 { contents: write, id-token: write }\n/s'
mutate_wf "publish's if: uses || instead of &&" 's/ && startsWith\(github.ref/ || startsWith\(github.ref/'
mutate_wf "publish's if: loses its event check" "s/github.event_name == 'push' && //"
mutate_wf "an expression in a one-line run:" 's/run: tests\/release\/prepare.sh/run: tests\/release\/prepare.sh \$\{\{ github.ref_name \}\}/'
mutate_wf "an expression inside a run: block" 's/(          set -euo pipefail\n          for set in a b)/          echo \$\{\{ github.ref_name \}\}\n$1/'
mutate_wf "workflow_dispatch takes inputs" 's/\n  workflow_dispatch:\n/\n  workflow_dispatch:\n    inputs:\n      ref:\n        required: false\n/'
mutate_wf "crates-io runs outside its environment" 's/(  crates-io:.*?)    environment: crates-io\n/$1/s'
mutate_wf "crates-io runs in another environment" 's/    environment: crates-io\n/    environment: release\n/'
mutate_wf "crates-io stops requiring a tag" 's/(  crates-io:.*?    if: github.event_name == .push.) && startsWith\(github.ref, .refs\/tags\/v.\)/$1/s'
mutate_wf "crates-io gets attestations: write" 's/(      id-token: write # trusted publishing only\n)/$1      attestations: write\n/'
mutate_wf "crates-io gets contents: write" 's/(  crates-io:.*?    permissions:\n      contents: )read/${1}write/s'
mutate_wf "another job names the crates-io environment" 's/(  packages-arm64:\n.*?    runs-on: ubuntu-24.04-arm\n)/$1    environment: crates-io\n/s'
mutate_wf "crates-io's actions: read becomes write" 's/(  crates-io:.*?      actions: )read/${1}write/s'
mutate_wf "prepare's actions: read becomes write" 's/(  prepare:.*?      actions: )read/${1}write/s'
mutate_wf "another job gets id-token: write" 's/(  packages-arm64:.*?    permissions:\n      contents: read\n)/$1      id-token: write\n/s'
# --locked: scripts in a copy of the directory
mkdir -p "$tmp/scripts"
cp "$RELEASE_DIR"/*.sh "$tmp/scripts/"
perl -pi -e 's/cross build --locked/cross build/' "$tmp/scripts/build-binary.sh"
expect_fail "mutation: cross build without --locked" env FIL_LINT_SCRIPTS="$tmp/scripts" "$RELEASE_DIR/lint-workflow.sh" "$wf"

# ============================================================== 3. tag signers (verify-tag.sh)
echo "== tag verification: the signers come from main, never from the tag"
keys=$tmp/keys
mkdir -p "$keys"
ssh-keygen -q -t ed25519 -N '' -C release-key -f "$keys/release"
ssh-keygen -q -t ed25519 -N '' -C attacker-key -f "$keys/attacker"
signer_line() { printf 'release namespaces="git,fil-release" %s\n' "$(cut -d' ' -f1,2 "$1.pub")"; }

g() { git -C "$tmp/origin" "$@"; }
mkdir -p "$tmp/origin"
git init -q -b main "$tmp/origin"
mkdir -p "$tmp/origin/.github"
# c0: an early main commit that (by mistake, later reverted) lists the attacker's key too
{ signer_line "$keys/release"; signer_line "$keys/attacker"; } > "$tmp/origin/.github/release-signers"
echo one > "$tmp/origin/f"
g add -A
g commit -q -m "main: signers list the release key and, by mistake, another"
# v5: signed by that other key, at c0. c0 is on main and ITS copy of the signers file lists the key.
g -c gpg.format=ssh -c "user.signingkey=$keys/attacker" tag -s -m "v5" v5
# c1: the mistake is reverted. main's signers now list the release key only.
signer_line "$keys/release" > "$tmp/origin/.github/release-signers"
g commit -q -am "main: signers list the release key"
# v1: signed by the release key, on main
g -c gpg.format=ssh -c "user.signingkey=$keys/release" tag -s -m "v1" v1
# v2: the attack. A side branch whose copy of the signers file lists only the attacker's key, and a
# tag signed by the attacker.
g checkout -q -b evil
signer_line "$keys/attacker" > "$tmp/origin/.github/release-signers"
echo evil > "$tmp/origin/f"
g commit -q -am "evil: signers list the attacker key"
g -c gpg.format=ssh -c "user.signingkey=$keys/attacker" tag -s -m "v2" v2
# v3: signed by the release key, but on a side branch (not on main)
echo more > "$tmp/origin/f"
g commit -q -am "side"
g -c gpg.format=ssh -c "user.signingkey=$keys/release" tag -s -m "v3" v3
# v4: lightweight
g tag v4 main
g checkout -q main
# the verifier's clone: main as origin/main, and the tags
git init -q --bare "$tmp/clone"
git -C "$tmp/clone" fetch -q "$tmp/origin" "+refs/heads/main:refs/remotes/origin/main" "+refs/tags/*:refs/tags/*"
vt() { "$RELEASE_DIR/verify-tag.sh" "$tmp/clone" "$@"; }
expect_pass "a tag signed by the release key, on main, verifies" vt v1
expect_fail "a tag signed by a key that only the TAG's own signers file lists is refused (off main)" vt v2
expect_fail "a tag signed by a key that main's signers no longer list is refused, though its own commit lists it" vt v5
expect_fail "a tag signed by the release key but off main is refused" vt v3
expect_fail "a lightweight tag is refused" vt v4
# The fixture must be sensitive: reading the signers from the tag instead of main accepts the attack.
sed 's|"\$mainref:.github/release-signers"|"refs/tags/$tag:.github/release-signers"|' "$RELEASE_DIR/verify-tag.sh" > "$tmp/verify-tag-mutant.sh"
chmod +x "$tmp/verify-tag-mutant.sh"
if cmp -s "$RELEASE_DIR/verify-tag.sh" "$tmp/verify-tag-mutant.sh"; then
  flunk "mutation 'signers from the tag' did not change verify-tag.sh (the test is stale)"
else
  expect_pass "mutation: signers taken from the tag ACCEPT the attack (so the test above is meaningful)" "$tmp/verify-tag-mutant.sh" "$tmp/clone" v5
fi
# the placeholder: a signers file with no key
git init -q -b main "$tmp/ph"
mkdir -p "$tmp/ph/.github"
printf '# TODO: the owner adds the release key (OQ-2)\n\n' > "$tmp/ph/.github/release-signers"
echo x > "$tmp/ph/f"
git -C "$tmp/ph" add -A
git -C "$tmp/ph" commit -q -m ph
git -C "$tmp/ph" -c gpg.format=ssh -c "user.signingkey=$keys/release" tag -s -m "v1" v1
git init -q --bare "$tmp/phclone"
git -C "$tmp/phclone" fetch -q "$tmp/ph" "+refs/heads/main:refs/remotes/origin/main" "+refs/tags/*:refs/tags/*"
expect_fail "a signers file with no key (the placeholder) verifies nothing" "$RELEASE_DIR/verify-tag.sh" "$tmp/phclone" v1
git -C "$tmp/origin" show main:.github/release-signers > "$tmp/own-signers"
expect_pass "the verifier's own signers file (FIL_RELEASE_SIGNERS) stands in for main's" env "FIL_RELEASE_SIGNERS=$tmp/own-signers" "$RELEASE_DIR/verify-tag.sh" "$tmp/phclone" v1
# SHA256SUMS.sig: the same keys, the fil-release namespace (what the owner signs with, offline)
echo "abc  file" > "$tmp/SHA256SUMS"
ssh-keygen -q -Y sign -f "$keys/release" -n fil-release "$tmp/SHA256SUMS" 2> /dev/null
allowed=$tmp/allowed
signer_line "$keys/release" > "$allowed"
expect_pass "SHA256SUMS.sig verifies with ssh-keygen -Y verify -n fil-release" bash -c "ssh-keygen -Y verify -f '$allowed' -I release -n fil-release -s '$tmp/SHA256SUMS.sig' < '$tmp/SHA256SUMS'"
echo "abd  file" > "$tmp/SHA256SUMS.altered"
expect_fail "an altered SHA256SUMS does not verify" bash -c "ssh-keygen -Y verify -f '$allowed' -I release -n fil-release -s '$tmp/SHA256SUMS.sig' < '$tmp/SHA256SUMS.altered'"
expect_fail "a signature in another namespace does not verify as fil-release" bash -c "ssh-keygen -Y verify -f '$allowed' -I release -n git -s '$tmp/SHA256SUMS.sig' < '$tmp/SHA256SUMS'"

# release-verify STOPS at the first failure of tag, signature or attestation: nothing from the
# download is looked at (release-check would print "release-check:" lines if it ran).
mkdir -p "$tmp/dl"
cp "$tmp/SHA256SUMS" "$tmp/dl/SHA256SUMS"
rv() { env FIL_RELEASE_REPO_URL="$tmp/origin" "$RELEASE_DIR/verify.sh" "$@" --from-dir "$tmp/dl" --no-rebuild --work "$tmp/vwork"; }
expect_fail "release-verify stops on a tag the main signers do not list" rv 5
grep -q 'STOP: tag v5 does not verify' "$tmp/out.log" && ! grep -q 'release-check' "$tmp/out.log" \
  && pass "...and ran nothing after it" || { flunk "...and ran nothing after it"; sed "s/^/       | /" "$tmp/out.log"; }
expect_fail "release-verify stops when SHA256SUMS.sig is missing" rv 1
grep -q 'STOP: no SHA256SUMS.sig' "$tmp/out.log" && ! grep -q 'release-check' "$tmp/out.log" \
  && pass "...and ran nothing after it" || { flunk "...and ran nothing after it"; sed "s/^/       | /" "$tmp/out.log"; }
cp "$tmp/SHA256SUMS.altered" "$tmp/dl/SHA256SUMS"
cp "$tmp/SHA256SUMS.sig" "$tmp/dl/SHA256SUMS.sig"
expect_fail "release-verify stops on a SHA256SUMS.sig that does not match SHA256SUMS" rv 1
grep -q 'STOP: SHA256SUMS.sig does not verify' "$tmp/out.log" && ! grep -q 'release-check' "$tmp/out.log" \
  && pass "...and ran nothing after it" || { flunk "...and ran nothing after it"; sed "s/^/       | /" "$tmp/out.log"; }
cp "$tmp/SHA256SUMS" "$tmp/dl/SHA256SUMS"
expect_fail "release-verify stops when the files do not match the signed SHA256SUMS" rv 1
grep -q 'STOP: the files do not match' "$tmp/out.log" && ! grep -q 'release-check' "$tmp/out.log" \
  && pass "...and ran nothing after it" || { flunk "...and ran nothing after it"; sed "s/^/       | /" "$tmp/out.log"; }

# ============================================================== 4. release-check on synthetic releases
echo "== release-check"
# A scratch tree (a git repository holding the files check.sh reads), stand-in binaries (shell
# scripts that print the version line, so `--version` runs; a PATH shim answers file(1) for them),
# and archives made with whatever tar the machine has.
mk_tree() { # DIR VER
  local d=$1
  mkdir -p "$d/crates/fil-proto/src" "$d/crates/fil-proto/tests/golden" "$d/crates/fil-transport/src/pipe"
  cp "$REPO_ROOT/Cargo.toml" "$REPO_ROOT/Cross.toml" "$REPO_ROOT/rust-toolchain.toml" "$REPO_ROOT/justfile" "$REPO_ROOT/LICENSE" "$REPO_ROOT/README.md" "$REPO_ROOT/THIRD-PARTY-NOTICES.md" "$d/"
  cp "$REPO_ROOT/crates/fil-proto/src/limits.rs" "$d/crates/fil-proto/src/"
  cp "$REPO_ROOT/crates/fil-proto/tests/golden/wire.txt" "$d/crates/fil-proto/tests/golden/"
  cp "$REPO_ROOT/crates/fil-transport/src/pipe/remote-commands.txt" "$d/crates/fil-transport/src/pipe/"
  git init -q -b main "$d"
  set_version "$d" "$2"
  git -C "$d" add -A
  git -C "$d" commit -q -m tree
}
set_version() { # DIR VER: Cargo.toml and a changelog section
  perl -pi -e 'if (!$d && s/^version = "[^"]*"/version = "'"$2"'"/) { $d = 1 }' "$1/Cargo.toml"
  printf '# Changelog\n\n## [%s] - 2026-11-04\n\n- first\n' "$2" > "$1/CHANGELOG.md"
}
fake_bin() { # FILE KIND
  cat > "$1" << EOT
#!/bin/sh
# FAKE-$2 /cargo-home/registry/src/index.crates.io-0/dep-1/src/lib.rs
echo "fil $VER (wire $(wire_major).$(wire_revision))"
echo "Copyright (C) 2026 Anthony Wertz"
echo "License GPL-3.0-or-later <https://gnu.org/licenses/gpl.html>"
echo "There is NO WARRANTY, to the extent permitted by law."
EOT
  # The install bootstrap, as a real build holds it (release-check requires it, and no other `sh -c`).
  printf '# %s\n' "$(release_bootstrap "$VER" "$(wire_major)" "$(wire_revision)")" >> "$1"
  chmod 755 "$1"
}
mkdir -p "$tmp/shim"
cat > "$tmp/shim/file" << 'EOT'
#!/bin/sh
# Test shim for file(1): the real one cannot tell a shell script from a static ELF.
f=$1
if grep -q FAKE-ELF-X86 "$f" 2> /dev/null; then echo "$f: ELF 64-bit LSB executable, x86-64, statically linked, stripped"
elif grep -q FAKE-ELF-AARCH64 "$f" 2> /dev/null; then echo "$f: ELF 64-bit LSB executable, ARM aarch64, statically linked, stripped"
elif grep -q FAKE-NOTSTRIPPED "$f" 2> /dev/null; then echo "$f: ELF 64-bit LSB executable, x86-64, statically linked, with debug_info, not stripped"
elif grep -q FAKE-MACHO "$f" 2> /dev/null; then echo "$f: Mach-O universal binary with 2 architectures: [x86_64] [arm64]"
else /usr/bin/file "$@"; fi
EOT
chmod +x "$tmp/shim/file"
tar_flags=
# GNU tar keeps the files' modes, and release-check (with GNU tar) refuses group or other write: fix
# them, so that a umask of 002 (Ubuntu's default for a login) does not fail the clean cases. bsdtar
# (macOS) is not checked for modes.
tar --version 2> /dev/null | grep -q 'GNU tar' && tar_flags='--owner=0 --group=0 --numeric-owner --mode=u=rwX,go=rX'

stage_release() { # uses $tree $stage $VER
  local lbl kind d saved=$ROOT
  ROOT=$tree
  rm -rf "$stage"
  mkdir -p "$stage"
  for lbl in x86_64-linux-musl aarch64-linux-musl universal-apple-darwin; do
    d=$stage/fil-$VER-$lbl
    mkdir -p "$d/bin" "$d/libexec/fil/remote/x86_64-linux" "$d/libexec/fil/remote/aarch64-linux" "$d/share/doc/fil"
    case $lbl in x86_64-linux-musl) kind=ELF-X86 ;; aarch64-linux-musl) kind=ELF-AARCH64 ;; *) kind=MACHO ;; esac
    fake_bin "$d/bin/fil" "$kind"
    fake_bin "$d/libexec/fil/remote/x86_64-linux/fil" ELF-X86
    fake_bin "$d/libexec/fil/remote/aarch64-linux/fil" ELF-AARCH64
    (cd "$d/libexec/fil/remote" && for f in aarch64-linux/fil x86_64-linux/fil; do printf '%s  %s\n' "$(sha256 "$f")" "$f"; done > SHA256SUMS)
    cp "$tree/README.md" "$tree/LICENSE" "$tree/THIRD-PARTY-NOTICES.md" "$d/share/doc/fil/"
    # DOC's files (docs/man/build.sh): man pages, completions, example configs, the changelog.
    mkdir -p "$d/share/man/man1" "$d/share/man/man5" "$d/share/bash-completion/completions" "$d/share/zsh/site-functions" "$d/share/fish/vendor_completions.d"
    printf '# Changelog\n\n## [0.9.0]\n' > "$d/share/doc/fil/CHANGELOG.md"
    for f in config daemon policy; do echo "# $f" > "$d/share/doc/fil/$f.example.toml"; done
    for f in man/man1/fil.1 man/man5/fil-config.5 man/man5/fil-policy.5 bash-completion/completions/fil zsh/site-functions/_fil fish/vendor_completions.d/fil.fish; do
      echo "stub" > "$d/share/$f"
    done
  done
  ROOT=$saved
}
# A file named .second-top-LABEL in the stage becomes a second top-level entry of that archive.
# A file named .source-drop-notices in the stage leaves THIRD-PARTY-NOTICES.md out of the source archive.
pack_release() { # uses $tree $stage $rel $VER
  local d src="$stage/fil-$VER-source" lbl
  rm -rf "$rel" "$src"
  mkdir -p "$rel" "$src/vendor" "$src/.cargo"
  cp "$tree/LICENSE" "$tree/THIRD-PARTY-NOTICES.md" "$tree/rust-toolchain.toml" "$src/"
  # A workspace of one dependency-free crate: `cargo metadata --offline` resolves it from nothing.
  printf '[workspace]\nresolver = "3"\nmembers = ["crates/x"]\n' > "$src/Cargo.toml"
  mkdir -p "$src/crates/x/src"
  printf '[package]\nname = "x"\nversion = "0.1.0"\nedition = "2024"\n' > "$src/crates/x/Cargo.toml"
  echo 'fn main() {}' > "$src/crates/x/src/main.rs"
  (cd "$src" && cargo generate-lockfile --offline > /dev/null 2>&1)
  : > "$src/.cargo/config.toml"
  [ -d "$stage/.source-extra" ] && cp -R "$stage/.source-extra/." "$src/"
  [ -e "$stage/.source-drop-notices" ] && rm "$src/THIRD-PARTY-NOTICES.md"
  for d in "$stage"/fil-*; do
    lbl=${d##*/}
    lbl=${lbl#fil-"$VER"-}
    extra=
    [ -e "$stage/.second-top-$lbl" ] && extra=.second-top-$lbl
    # shellcheck disable=SC2086
    (cd "$stage" && tar $tar_flags -czf "$rel/fil-$VER-$lbl.tar.gz" "fil-$VER-$lbl" $extra)
  done
  pack_packages
  write_sums
}
# The four packages, from the staged Linux trees (selftest_pkg.py; the real ones come from nfpm).
# Mutations, as files in the stage: .pkg-owner (a uid for every entry), .pkg-extra (one more file in
# every package), .pkg-bin (a different bin/fil in every package), .pkg-version (another version),
# .pkg-depends (a dependency), .pkg-script (a maintainer script), .pkg-rpm-dir (one more directory
# the rpm owns).
pack_packages() {
  local a lbl kind arch d m f dst mode owner=0 pv=$VER dep='' scr=''
  [ -f "$stage/.pkg-owner" ] && owner=$(cat "$stage/.pkg-owner")
  [ -f "$stage/.pkg-version" ] && pv=$(cat "$stage/.pkg-version")
  [ -f "$stage/.pkg-depends" ] && dep=$(cat "$stage/.pkg-depends")
  [ -f "$stage/.pkg-script" ] && scr=1
  pv=$(printf '%s' "$pv" | sed 's/-/~/')
  for a in x86_64 aarch64; do
    lbl=$a-linux-musl
    d=$stage/fil-$VER-$lbl
    for kind in deb rpm; do
      m=$tmp/manifest
      : > "$m"
      while IFS= read -r f; do
        dst=$(package_path "$kind" "$f")
        if [ -x "$d/$f" ]; then mode=755; else mode=644; fi
        src=$d/$f
        [ "$f" = bin/fil ] && [ -f "$stage/.pkg-bin" ] && src=$stage/.pkg-bin
        printf '%s\t%s\t%s\n' "$src" "$dst" "$mode" >> "$m"
      done < <(cd "$d" && find . -type f | sed 's|^\./||' | LC_ALL=C sort)
      [ -f "$stage/.pkg-extra" ] && printf '%s\t%s\t%s\n' "$stage/.pkg-extra" /usr/bin/extra 755 >> "$m"
      if [ "$kind" = rpm ]; then
        # An rpm owns its own directories, as linux-packages.sh lists them.
        (cd "$d" && find libexec/fil share/doc/fil -type d) | sed -e "s|^share/doc/fil|/usr/share/doc/$PACKAGE_NAME|" -e 's|^libexec|/usr/libexec|' \
          | while IFS= read -r dst; do printf -- '-\t%s\t755\n' "$dst"; done >> "$m"
        [ -f "$stage/.pkg-rpm-dir" ] && printf -- '-\t/usr/bin\t755\n' >> "$m"
      fi
      f=$(package_file "$kind" "$a" "$VER")
      arch=$(package_arch "$kind" "$a")
      SELFTEST_PKG_DEPENDS=$dep SELFTEST_PKG_SCRIPT=$scr python3 -I "$RELEASE_DIR/selftest_pkg.py" "$kind" "$rel/$f" "$PACKAGE_NAME" "$pv" 1 "$arch" "$owner" "$m"
    done
  done
}
write_sums() { (cd "$rel" && for f in $({ printf '%s\n' fil-"$VER"-*.tar.gz; package_names "$VER"; } | LC_ALL=C sort); do if [ -f "$f" ]; then printf '%s  %s\n' "$(sha256 "$f")" "$f"; fi; done > SHA256SUMS); }

case_n=0
new_case() { # VER: a tree, a stage and the packed release; sets $tree $stage $rel $VER
  VER=$1
  case_n=$((case_n + 1))
  tree=$tmp/case$case_n/tree stage=$tmp/case$case_n/stage rel=$tmp/case$case_n/rel
  mkdir -p "$tmp/case$case_n"
  mk_tree "$tree" "$VER"
  stage_release
  pack_release
}
check() { env PATH="$tmp/shim:$PATH" FIL_RELEASE_ROOT="$tree" "$RELEASE_DIR/check.sh" "$@"; }
# mutated NAME COMMAND...: a fresh case whose stage is changed (inside the stage directory) before packing
mutated() {
  local name=$1
  shift
  new_case 0.9.0
  (cd "$stage" && eval "$*")
  pack_release
  expect_fail "mutation: $name" check --dry-run --no-tree "$rel"
}

# mutated_msg NAME PATTERN COMMAND...: as `mutated`, and the failure output must say PATTERN
mutated_msg() {
  local name=$1 pat=$2
  shift 2
  new_case 0.9.0
  (cd "$stage" && eval "$*")
  pack_release
  if check --dry-run --no-tree "$rel" > "$tmp/out.log" 2>&1; then flunk "mutation: $name (it should have failed)"
  elif grep -qF -- "$pat" "$tmp/out.log"; then pass "mutation: $name"
  else flunk "mutation: $name (failed, but not with '$pat')"; sed 's/^/       | /' "$tmp/out.log" | tail -5; fi
}

new_case 0.9.0
expect_pass "a clean dry run passes" check --dry-run --no-tree "$rel"
expect_pass "a clean tag run passes (changelog, tag, wire)" check --tag v0.9.0 --no-tree "$rel"
expect_pass "a clean complete run passes" check --tag v0.9.0 --complete --no-tree "$rel"
expect_fail "a real release needs a tag (neither --tag nor --dry-run)" check --no-tree "$rel"
expect_fail "mutation: the tag is not the Cargo.toml version" check --tag v0.9.1 --no-tree "$rel"
printf '# Changelog\n\n## [0.0.1]\n' > "$tree/CHANGELOG.md"
expect_fail "mutation: the version has no changelog section" check --tag v0.9.0 --no-tree "$rel"
expect_pass "(a dry run does not read the changelog)" check --dry-run --no-tree "$rel"

mutated "a builder path (/Users/) is left in the Linux bin/fil" \
  "echo '# /Users/builder/.cargo/registry/src/x.rs' >> fil-0.9.0-x86_64-linux-musl/bin/fil"
mutated "a builder path (/home/) is left in the macOS bin/fil" \
  "echo '# /home/runner/work/fil' >> fil-0.9.0-universal-apple-darwin/bin/fil"
mutated "a FIL_TEST_ string reaches bin/fil" \
  "echo '# FIL_TEST_WIRE_REVISION' >> fil-0.9.0-aarch64-linux-musl/bin/fil"
mutated "the path remap was not applied (no /cargo-home/ in bin/fil)" \
  "perl -pi -e 's|/cargo-home/|/root/.cargo/|' fil-0.9.0-universal-apple-darwin/bin/fil"
mutated "an archive has two top-level entries" \
  "echo stray > .second-top-x86_64-linux-musl"
mutated "a bundled copy differs from the target's own binary" \
  "echo '# changed' >> fil-0.9.0-universal-apple-darwin/libexec/fil/remote/aarch64-linux/fil; (cd fil-0.9.0-universal-apple-darwin/libexec/fil/remote && for f in aarch64-linux/fil x86_64-linux/fil; do printf '%s  %s\n' \"\$(shasum -a 256 \$f | cut -d' ' -f1)\" \$f; done > SHA256SUMS)"
mutated "the bundle's SHA256SUMS is wrong" \
  "echo 'deadbeef  x86_64-linux/fil' > fil-0.9.0-x86_64-linux-musl/libexec/fil/remote/SHA256SUMS"
mutated_msg "bin/fil lacks the legal lines of --version" "lacks the copyright, license or warranty line" \
  "perl -pi -e 's/Anthony Wertz/Someone/' fil-0.9.0-aarch64-linux-musl/bin/fil"
mutated_msg "a vt100 source path is in bin/fil" "vt100" \
  "echo '# /cargo-home/registry/src/index.crates.io-0/vt100-0.18.0/src/lib.rs' >> fil-0.9.0-aarch64-linux-musl/bin/fil"
mutated_msg "bin/fil has no install bootstrap" "does not hold the approved install bootstrap" \
  "perl -ni -e 'print unless /^# sh -c /' fil-0.9.0-x86_64-linux-musl/bin/fil"
mutated_msg "bin/fil's install bootstrap is altered" "does not hold the approved install bootstrap" \
  "perl -pi -e 's/rm -rf/rm -fr/ if /^# sh -c /' fil-0.9.0-universal-apple-darwin/bin/fil"
mutated_msg "bin/fil's install bootstrap is for another wire" "does not hold the approved install bootstrap" \
  "perl -pi -e 's/\\(wire (\\d+)\\.(\\d+)\\)/(wire \$1.9\$2)/ if /^# sh -c /' fil-0.9.0-aarch64-linux-musl/bin/fil"
mutated_msg "bin/fil holds a second remote command line" "other than the pinned one" \
  "echo \"# bash -c 'fil server --stdio'\" >> fil-0.9.0-universal-apple-darwin/bin/fil"
mutated_msg "an ELF is not stripped" "is not stripped" "perl -pi -e 's/FAKE-ELF-X86/FAKE-NOTSTRIPPED/' fil-0.9.0-x86_64-linux-musl/bin/fil"
# test-fixture host names and addresses: the list comes from the tree's test directories
new_case 0.9.0
mkdir -p "$tree/crates/x/tests"
printf 'host = "box.example.net"\naddr = "198.51.100.7"\nloop = "127.0.0.1"\n' > "$tree/crates/x/tests/fixture.toml"
expect_pass "fixtures that are not in the binaries pass" check --dry-run --no-tree "$rel"
echo '# connects to box.example.net' >> "$stage/fil-0.9.0-x86_64-linux-musl/bin/fil"
pack_release
expect_fail "mutation: a fixture host name is in bin/fil" check --dry-run --no-tree "$rel"
grep -qF 'box.example.net' "$tmp/out.log" && pass "...and the failure names it" || flunk "...and the failure names it"
stage_release
echo '# 198.51.100.7:22' >> "$stage/fil-0.9.0-aarch64-linux-musl/bin/fil"
pack_release
expect_fail "mutation: a fixture address is in bin/fil" check --dry-run --no-tree "$rel"
stage_release
echo '# 127.0.0.1 and 198.51.100.77 and sub.box.example.net.invalid' >> "$stage/fil-0.9.0-universal-apple-darwin/bin/fil"
pack_release
expect_pass "loopback, a longer address and a different name are not the fixtures" check --dry-run --no-tree "$rel"
mutated "bin/fil is missing" "rm fil-0.9.0-aarch64-linux-musl/bin/fil"
mutated "the license is missing" "rm fil-0.9.0-x86_64-linux-musl/share/doc/fil/LICENSE"
mutated "a man page is missing" "rm fil-0.9.0-x86_64-linux-musl/share/man/man1/fil.1"
mutated "a completion is missing" "rm fil-0.9.0-universal-apple-darwin/share/zsh/site-functions/_fil"
mutated "an example config is missing" "rm fil-0.9.0-aarch64-linux-musl/share/doc/fil/daemon.example.toml"
mutated "the notices are missing" "rm fil-0.9.0-aarch64-linux-musl/share/doc/fil/THIRD-PARTY-NOTICES.md"
mutated_msg "an archive's notices are not the source archive's" "differs from the source archive's THIRD-PARTY-NOTICES.md" \
  "echo '- stale' >> fil-0.9.0-universal-apple-darwin/share/doc/fil/THIRD-PARTY-NOTICES.md"
mutated_msg "the source archive lacks the notices" "fil-0.9.0-source.tar.gz lacks THIRD-PARTY-NOTICES.md" "touch .source-drop-notices"
mutated "the binary is not static" "perl -pi -e 's/FAKE-ELF-X86/FAKE-DYNAMIC/' fil-0.9.0-x86_64-linux-musl/bin/fil"
# the .deb and .rpm packages
mutated_msg "a package's files are not root's" "not those of" "echo 1000 > .pkg-owner"
mutated_msg "a package holds a file its archive does not" "not those of" "echo x > .pkg-extra"
mutated_msg "a package's bin/fil is not the archive's" "files differ from the archive's" "cp fil-0.9.0-x86_64-linux-musl/bin/fil .pkg-bin && echo '# other' >> .pkg-bin"
mutated_msg "a package says another version" "not 'fil 0.9.0-1" "echo 0.9.1 > .pkg-version"
mutated_msg "a package has a dependency" "maintainer scripts or dependencies" "echo libc6 > .pkg-depends"
mutated_msg "a package has a maintainer script" "maintainer scripts or dependencies" "touch .pkg-script"
mutated_msg "an rpm owns a directory that is not its own" "owns the directories" "touch .pkg-rpm-dir"
new_case 0.9.0-rc.1
expect_pass "a pre-release's packages carry the version 0.9.0~rc.1-1" check --dry-run --no-tree "$rel"
new_case 0.9.0
rm "$rel/fil_0.9.0_arm64.deb"
write_sums
expect_fail "mutation: a package is missing" check --dry-run --no-tree "$rel"
new_case 0.9.0
cp "$rel/fil_0.9.0_amd64.deb" "$rel/fil_0.9.0_arm64.deb"
write_sums
mutated_check_msg() { if check --dry-run --no-tree "$rel" > "$tmp/out.log" 2>&1; then flunk "$1 (it should have failed)"; elif grep -qF -- "$2" "$tmp/out.log"; then pass "$1"; else flunk "$1 (failed, but not with '$2')"; sed 's/^/       | /' "$tmp/out.log" | tail -5; fi; }
mutated_check_msg "mutation: the arm64 .deb is the amd64 one" "not 'fil 0.9.0-1 arm64'"
new_case 0.9.0
printf 'not a package' > "$rel/fil-0.9.0-1.x86_64.rpm"
write_sums
mutated_check_msg "mutation: an rpm that is not one" "cannot be read"
# A signed rpm: only its signature header changes, which pkg_inspect sees, and its body does not.
new_case 0.9.0
r=$rel/fil-0.9.0-1.x86_64.rpm
body() { python3 -I "$RELEASE_DIR/pkg_inspect.py" "$1" "$tmp/pi-$RANDOM" | sed -n "s/^$2 //p"; }
b0=$(body "$r" body)
cp "$r" "$tmp/signed.rpm"
python3 -I "$RELEASE_DIR/selftest_pkg.py" sign "$tmp/signed.rpm"
expect_pass "pkg_inspect: an unsigned rpm says signed no" test "$(body "$r" signed)" = no
expect_pass "pkg_inspect: a signed rpm says signed yes" test "$(body "$tmp/signed.rpm" signed)" = yes
expect_pass "pkg_inspect: signing changes the file" sh -c '! cmp -s "$1" "$2"' sh "$r" "$tmp/signed.rpm"
expect_pass "pkg_inspect: ...and not the body (header and payload)" test "$(body "$tmp/signed.rpm" body)" = "$b0"
python3 -I "$RELEASE_DIR/selftest_pkg.py" tamper "$tmp/signed.rpm"
expect_fail "pkg_inspect: a changed payload changes the body" test "$(body "$tmp/signed.rpm" body)" = "$b0"
cp "$r" "$tmp/s2.rpm" && python3 -I "$RELEASE_DIR/selftest_pkg.py" sign "$tmp/s2.rpm" && cp "$tmp/s2.rpm" "$r" && write_sums
expect_pass "release-check passes a release whose rpm is signed" check --dry-run --no-tree "$rel"
# Versions the packages can carry (nfpm's semver): X.Y.Z and X.Y.Z-PRE.
for v in 0.1.0 0.1.0-rc.1 1.20.3-beta.2; do expect_pass "package version $v is accepted" package_version_ok "$v"; done
for v in 1.0 0.1 0.1.0-rc-2 0.1.0+b 0.1.0-; do expect_fail "package version $v is refused" package_version_ok "$v"; done
expect_pass "package_version: 0.1.0-rc.1 is 0.1.0~rc.1-1" test "$(package_version 0.1.0-rc.1)" = "0.1.0~rc.1-1"

# The owner's package signing (sign-packages.sh), on fake gpg, rpmsign and rpmkeys.
st=$tmp/sign
mkdir -p "$st"
printf -- '-----BEGIN PGP PUBLIC KEY BLOCK-----\nfake\n-----END PGP PUBLIC KEY BLOCK-----\n' > "$st/key.asc"
cat > "$st/gpg" << 'EOF2'
#!/bin/sh
case " $* " in
  *" --show-keys "*) printf 'pub:u:4096:1:AAAA:1:::u:::sc:\nfpr:::::::::0123456789ABCDEF0123456789ABCDEF01234567:\n' ;;
  *" --detach-sign "*) while [ $# -gt 0 ]; do [ "$1" = --output ] && o=$2; shift; done; echo SIG > "$o" ;;
  *" --verify "*) [ -z "$FAKE_GPG_BAD" ] ;;
  *) exit 0 ;;
esac
EOF2
cat > "$st/rpmsign" << EOF2
#!/bin/sh
for a; do f=\$a; done
python3 -I "$RELEASE_DIR/selftest_pkg.py" sign "\$f"
[ -z "\$FAKE_TAMPER" ] || python3 -I "$RELEASE_DIR/selftest_pkg.py" tamper "\$f"
EOF2
cat > "$st/rpmkeys" << 'EOF2'
#!/bin/sh
case " $* " in
  *" --checksig "*) for a; do f=$a; done; if [ -n "$FAKE_RPMKEYS_BAD" ]; then echo "$f: digests SIGNATURES NOT OK"; else echo "$f: digests signatures OK"; fi ;;
  *) exit 0 ;;
esac
EOF2
chmod +x "$st/gpg" "$st/rpmsign" "$st/rpmkeys"
sp() { env FIL_GPG="$st/gpg" FIL_RPMSIGN="$st/rpmsign" FIL_RPMKEYS="$st/rpmkeys" FIL_PACKAGE_KEY_FILE="$st/key.asc" FIL_RELEASE_ROOT="$tree" "$@" "$RELEASE_DIR/sign-packages.sh" 0.9.0 "$rel"; }
new_case 0.9.0
cp "$rel/SHA256SUMS" "$tmp/sums.before"
b_before=$(body "$rel/fil-0.9.0-1.aarch64.rpm" body)
expect_pass "sign-packages: signs the two rpms" sp
expect_pass "sign-packages: ...which then say signed yes, with the same body" sh -c 'test "$1" = yes && test "$2" = "$3"' sh "$(body "$rel/fil-0.9.0-1.aarch64.rpm" signed)" "$(body "$rel/fil-0.9.0-1.aarch64.rpm" body)" "$b_before"
expect_pass "sign-packages: ...the .debs are untouched" sh -c 'grep "_amd64.deb\$" "$1" | cmp -s - "$2"' sh "$rel/SHA256SUMS" <(grep '_amd64.deb$' "$tmp/sums.before")
expect_pass "sign-packages: ...SHA256SUMS is rewritten and checks" sh -c '! cmp -s "$1" "$2"' sh "$rel/SHA256SUMS" "$tmp/sums.before"
expect_pass "sign-packages: ...sums_check passes" sums_check "$rel" SHA256SUMS
expect_pass "sign-packages: ...and SHA256SUMS.asc is written" test -s "$rel/SHA256SUMS.asc"
expect_pass "release-check passes the signed release" check --dry-run --no-tree "$rel"
new_case 0.9.0
cp "$rel/fil-0.9.0-1.x86_64.rpm" "$tmp/x86.before"
expect_fail "sign-packages: an rpmsign that changes more than the signature header is refused" sp FAKE_TAMPER=1
expect_pass "sign-packages: ...and the rpm in the release directory is left as it was" cmp "$rel/fil-0.9.0-1.x86_64.rpm" "$tmp/x86.before"
new_case 0.9.0
expect_fail "sign-packages: a signature rpmkeys does not verify is refused" sp FAKE_RPMKEYS_BAD=1
new_case 0.9.0
expect_fail "sign-packages: a SHA256SUMS.asc that does not verify is refused" sp FAKE_GPG_BAD=1
new_case 0.9.0
printf '# placeholder\n' > "$st/nokey.asc"
expect_fail "sign-packages: no key in the key file, no signing" env FIL_GPG="$st/gpg" FIL_RPMSIGN="$st/rpmsign" FIL_RPMKEYS="$st/rpmkeys" FIL_PACKAGE_KEY_FILE="$st/nokey.asc" FIL_RELEASE_ROOT="$tree" "$RELEASE_DIR/sign-packages.sh" 0.9.0 "$rel"
echo tampered >> "$rel/fil_0.9.0_amd64.deb"
expect_fail "sign-packages: files that do not match SHA256SUMS are refused" sp

# --no-exec: nothing from the download runs. The source archive's own cargo config names a rustc
# wrapper; `cargo metadata` runs it (so the fixture is sensitive), --no-exec must not.
new_case 0.9.0
mkdir -p "$stage/.source-extra"
printf '#!/bin/sh\ntouch "%s"\nexec "$@"\n' "$tmp/wrapper-ran" > "$stage/.source-extra/w.sh"
chmod 755 "$stage/.source-extra/w.sh"
printf '[build]\nrustc-wrapper = "%s"\n' "$stage/.source-extra/w.sh" > "$stage/.source-extra/.cargo-config"
mkdir -p "$stage/.source-extra/.cargo"
mv "$stage/.source-extra/.cargo-config" "$stage/.source-extra/.cargo/config.toml"
pack_release
rm -f "$tmp/wrapper-ran"
check --no-exec --dry-run --no-tree "$rel" > /dev/null 2>&1 || true
[ ! -e "$tmp/wrapper-ran" ] && pass "--no-exec: the source archive's rustc wrapper did not run" || flunk "--no-exec: the source archive's rustc wrapper ran"
check --dry-run --no-tree "$rel" > /dev/null 2>&1 || true
[ -e "$tmp/wrapper-ran" ] && pass "(without --no-exec the same wrapper does run: the hazard is real)" || flunk "(without --no-exec the wrapper did not run: the test is stale)"
# and a downloaded binary is not run: a binary that leaves a mark when run
new_case 0.9.0
for d in "$stage"/fil-0.9.0-x86_64-linux-musl "$stage"/fil-0.9.0-aarch64-linux-musl "$stage"/fil-0.9.0-universal-apple-darwin; do
  for b in "$d/bin/fil" "$d"/libexec/fil/remote/*/fil; do
    perl -pi -e 's|^echo "fil|touch "'"$tmp"'/binary-ran"\necho "fil|' "$b"
  done
  (cd "$d/libexec/fil/remote" && for f in aarch64-linux/fil x86_64-linux/fil; do printf '%s  %s\n' "$(sha256 "$f")" "$f"; done > SHA256SUMS)
done
pack_release
rm -f "$tmp/binary-ran"
check --no-exec --dry-run --no-tree "$rel" > /dev/null 2>&1 || true
[ ! -e "$tmp/binary-ran" ] && pass "--no-exec: no downloaded binary was run" || flunk "--no-exec: a downloaded binary was run"
check --dry-run --no-tree "$rel" > /dev/null 2>&1 || true
[ -e "$tmp/binary-ran" ] && pass "(without --no-exec a binary of this machine's kind is run: the hazard is real)" || flunk "(without --no-exec nothing ran: the test is stale)"

# SHA256SUMS
new_case 0.9.0
cp "$rel/SHA256SUMS" "$tmp/sums.good"
sed '1d' "$tmp/sums.good" > "$rel/SHA256SUMS"
expect_fail "mutation: SHA256SUMS lists only three files" check --dry-run --no-tree "$rel"
{ tail -n 1 "$tmp/sums.good"; sed '$d' "$tmp/sums.good"; } > "$rel/SHA256SUMS"
expect_fail "mutation: SHA256SUMS is not sorted" check --dry-run --no-tree "$rel"
{ cat "$tmp/sums.good"; echo "$(sha256 "$tmp/sums.good")  SHA256SUMS"; } > "$rel/SHA256SUMS"
expect_fail "mutation: SHA256SUMS covers itself" check --dry-run --no-tree "$rel"
# Change the first digest's first hex digit, whatever it is (0 becomes 1, anything else 0).
sed '1s/^0/X/;1s/^[1-9a-f]/0/;1s/^X/1/' "$tmp/sums.good" > "$rel/SHA256SUMS"
cmp -s "$tmp/sums.good" "$rel/SHA256SUMS" && flunk "(the checksum mutation changed nothing)"
expect_fail "mutation: a checksum is wrong" check --dry-run --no-tree "$rel"

# the wire: the golden's [history] must end at PROTOCOL_REVISION
new_case 0.9.0
perl -pi -e 's/^(pub const PROTOCOL_REVISION: u16 = )(\d+);/$1.($2+1).";"/e' "$tree/crates/fil-proto/src/limits.rs"
expect_fail "mutation: the golden's [history] does not end at PROTOCOL_REVISION" check --dry-run --no-tree "$rel"

# a patch release keeps the wire of its minor (the previous tag of the same minor)
new_case 0.9.0
git -C "$tree" tag -a -m x v0.9.0
set_version "$tree" 0.9.1
git -C "$tree" commit -q -am "0.9.1"
VER=0.9.1
stage_release
pack_release
expect_pass "a patch release with its minor's wire passes" check --tag v0.9.1 --no-tree "$rel"
perl -pi -e 's/^(pub const PROTOCOL_REVISION: u16 = )(\d+);/$1.($2+1).";"/e' "$tree/crates/fil-proto/src/limits.rs"
last=$(golden_last_revision "$tree/crates/fil-proto/tests/golden/wire.txt")
printf '%s = 0000000000000000\n' "$((last + 1))" >> "$tree/crates/fil-proto/tests/golden/wire.txt"
git -C "$tree" commit -q -am "wire bump"
stage_release
pack_release
expect_fail "mutation: a patch release changes the wire" check --tag v0.9.1 --no-tree "$rel"

# sums_check must not be fooled by a checksum file with no well-formed line (macOS's sha256sum -c exits 0)
mkdir -p "$tmp/sc"
echo data > "$tmp/sc/f"
printf '%s  f\n' "$(sha256 "$tmp/sc/f")" > "$tmp/sc/good"
expect_pass "sums_check accepts a correct file" sums_check "$tmp/sc" good
echo "abc  f" > "$tmp/sc/short"
expect_fail "mutation: a checksum file with a malformed line passes sums_check" sums_check "$tmp/sc" short
: > "$tmp/sc/empty"
expect_fail "mutation: an empty checksum file passes sums_check" sums_check "$tmp/sc" empty
printf '%s  missing\n' "$(sha256 "$tmp/sc/f")" > "$tmp/sc/missing"
expect_fail "mutation: a listed file that does not exist passes sums_check" sums_check "$tmp/sc" missing
printf '%s  f\n' "0000000000000000000000000000000000000000000000000000000000000000" > "$tmp/sc/wrong"
expect_fail "mutation: a wrong digest passes sums_check" sums_check "$tmp/sc" wrong

# ============================================================== 4b. the in-workflow tag checks (prepare.sh)
echo "== prepare.sh (the tag checks that are NOT the gate)"
new_case 0.9.0
git -C "$tree" tag -a -m "fil 0.9.0" v0.9.0
git -C "$tree" tag v0.9.0-light
git -C "$tree" tag -a -m "fil 0.9.1" v0.9.1
prep() { # TAG [EVENT]: a fresh clone with origin set, as actions/checkout leaves it
  rm -rf "$tmp/w"
  git clone -q "$tree" "$tmp/w"
  : > "$tmp/gho"
  env FIL_RELEASE_ROOT="$tmp/w" GITHUB_EVENT_NAME="${2:-push}" GITHUB_REF="refs/tags/$1" GITHUB_REF_NAME="$1" GITHUB_OUTPUT="$tmp/gho" "$RELEASE_DIR/prepare.sh"
}
expect_pass "an annotated tag equal to the version on main is accepted" prep v0.9.0
expect_pass "...and it reports release=true" grep -q '^release=true' "$tmp/gho"
expect_fail "a tag that is not v<Cargo.toml version> is refused" prep v0.9.1
expect_fail "a lightweight tag is refused (checkout turns annotated tags into lightweight ones; the script re-fetches the tag object)" prep v0.9.0-light
main_tree=$tree
new_case 0.9.0
git -C "$tree" checkout -q -b side
echo side > "$tree/side"
git -C "$tree" add side
git -C "$tree" commit -q -m side
git -C "$tree" tag -a -m "off main" v0.9.0
git -C "$tree" checkout -q main
expect_fail "a v<version> tag whose commit is not on main is refused" prep v0.9.0
tree=$main_tree
expect_pass "a workflow_dispatch run is a dry run (release=false)" bash -c "cd '$tmp' && : > gho && FIL_RELEASE_ROOT='$tree' GITHUB_EVENT_NAME=workflow_dispatch GITHUB_REF=refs/heads/x GITHUB_OUTPUT='$tmp/gho' '$RELEASE_DIR/prepare.sh' && grep -q '^release=false' gho"
expect_fail "a push that is not a v* tag is refused" bash -c "FIL_RELEASE_ROOT='$tree' GITHUB_EVENT_NAME=push GITHUB_REF=refs/heads/main '$RELEASE_DIR/prepare.sh'"
# The tag moved after the run started (deleted and pushed again on another commit): the run's
# checked-out commit is the old one, and a waiting job must not build or publish it.
new_case 0.9.0
git -C "$tree" tag -a -m "fil 0.9.0" v0.9.0
rm -rf "$tmp/w"
git clone -q "$tree" "$tmp/w"
echo later > "$tree/later"
git -C "$tree" add later
git -C "$tree" commit -q -m "a fix"
git -C "$tree" tag -f -a -m "fil 0.9.0, again" v0.9.0 > /dev/null
: > "$tmp/gho"
expect_fail "a tag that now names another commit than the run's is refused" env FIL_RELEASE_ROOT="$tmp/w" GITHUB_EVENT_NAME=push GITHUB_REF=refs/tags/v0.9.0 GITHUB_REF_NAME=v0.9.0 GITHUB_OUTPUT="$tmp/gho" "$RELEASE_DIR/prepare.sh"
grep -q 'is now commit' "$tmp/out.log" && pass "...and says so" || { flunk "...and says so"; sed 's/^/       | /' "$tmp/out.log" | tail -3; }

# ============================================================== 4b'. the crates-io job's gate (crates-io-gate.sh)
echo "== crates-io-gate.sh"
gate=$RELEASE_DIR/crates-io-gate.sh
gt=$tmp/gate
mkdir -p "$gt/files"
# A repository whose main lists the release key, and a release whose SHA256SUMS that key signed.
git init -q -b main "$gt/repo"
mkdir -p "$gt/repo/.github"
signer_line "$keys/release" | sed 's/namespaces="git,fil-release"/namespaces="fil-release"/' > "$gt/repo/.github/release-signers"
git -C "$gt/repo" add -A && git -C "$gt/repo" commit -q -m main
git -C "$gt/repo" update-ref refs/remotes/origin/main HEAD
echo "sums" > "$gt/files/SHA256SUMS"
ssh-keygen -q -Y sign -f "$keys/release" -n fil-release "$gt/files/SHA256SUMS"
cat > "$gt/gh" << 'EOF2'
#!/bin/sh
# FAKE_ENV: the environment's user reviewers (owner, bot, none, missing); FAKE_DRAFT: true, false or missing
case "$1 $2" in
  "api repos/"*)
    case $FAKE_ENV in owner) echo "splarg-bot anthonytw" ;; bot) echo splarg-bot ;; none) echo ;; *) echo "HTTP 403" >&2; exit 1 ;; esac ;;
  "release view")
    [ "$FAKE_DRAFT" = missing ] && exit 1
    echo "$FAKE_DRAFT" ;;
  "release download")
    while [ $# -gt 0 ]; do [ "$1" = -D ] && d=$2; shift; done
    cp "$FAKE_FILES"/SHA256SUMS "$FAKE_FILES"/SHA256SUMS.sig "$d"/ ;;
  *) exit 2 ;;
esac
EOF2
chmod +x "$gt/gh"
g_run() { env FIL_RELEASE_ROOT="$gt/repo" FIL_GATE_GH="$gt/gh" GITHUB_REPOSITORY=o/r FAKE_FILES="$gt/files" "$@" "$gate" v0.9.0; }
expect_pass "gate: a protected environment, a published release, a valid signature" g_run FAKE_ENV=owner FAKE_DRAFT=false
expect_fail "gate: an environment with no required reviewer is refused" g_run FAKE_ENV=none FAKE_DRAFT=false
expect_fail "gate: an environment whose only reviewer is not the owner is refused" g_run FAKE_ENV=bot FAKE_DRAFT=false
g_env() { env FIL_RELEASE_ROOT="$gt/repo" FIL_GATE_GH="$gt/gh" GITHUB_REPOSITORY=o/r "$@"; }
expect_pass "gate --environment-only: the owner reviews" g_env FAKE_ENV=owner "$gate" --environment-only
expect_fail "gate --environment-only: an environment the token cannot read is an error on a tag push" g_env FAKE_ENV=missing "$gate" --environment-only
expect_pass "gate --environment-only --warn: ...and a warning on a dry run" g_env FAKE_ENV=missing "$gate" --environment-only --warn
grep -q '::warning::cannot read the crates-io environment' "$tmp/out.log" && pass "...which says so" || { flunk "...which says so"; sed 's/^/       | /' "$tmp/out.log"; }
expect_fail "gate: an environment that cannot be read is refused" g_run FAKE_ENV=missing FAKE_DRAFT=false
expect_fail "gate: a draft is refused" g_run FAKE_ENV=owner FAKE_DRAFT=true
expect_fail "gate: a release the job cannot see is refused" g_run FAKE_ENV=owner FAKE_DRAFT=missing
cp "$gt/files/SHA256SUMS.sig" "$gt/good.sig"
echo "other" > "$gt/files/SHA256SUMS"
expect_fail "gate: a SHA256SUMS.sig that does not match SHA256SUMS is refused" g_run FAKE_ENV=owner FAKE_DRAFT=false
echo "sums" > "$gt/files/SHA256SUMS"
rm "$gt/files/SHA256SUMS.sig"
ssh-keygen -q -Y sign -f "$keys/attacker" -n fil-release "$gt/files/SHA256SUMS"
expect_fail "gate: a signature by a key that main does not list is refused" g_run FAKE_ENV=owner FAKE_DRAFT=false
rm -f "$gt/files/SHA256SUMS.sig"
cp "$gt/good.sig" "$gt/files/SHA256SUMS.sig"
expect_pass "gate: (the good signature again passes)" g_run FAKE_ENV=owner FAKE_DRAFT=false

# ============================================================== 4c. crates.io name reservation
echo "== reserve-crate-names.sh"
rc_script=$RELEASE_DIR/reserve-crate-names.sh
expect_pass "reserve-crate-names: --help works" bash "$rc_script" --help
rc_names=$(sed -n 's/^CRATES="\(.*\)"$/\1/p' "$rc_script")
expect_pass "reserve-crate-names: the nine published crates, in order" test "$rc_names" = "fil-shell fil-proto fil-transport fil-session fil-daemon fil-client fil-term fil-predict fil-forward"
# fil-shell is the published name of the user-facing crate, which crates/fil is today.
for n in $rc_names; do
  d=$n
  [ "$n" != fil-shell ] || d=fil
  expect_pass "reserve-crate-names: $n has a crate in the workspace" test -f "$REPO_ROOT/crates/$d/Cargo.toml"
done
# The fakes stand in for cargo and curl, so no network is used and nothing is published.
mkdir -p "$tmp/rc"
cat > "$tmp/rc/curl" << 'EOF'
#!/bin/sh
# the last argument is the URL: "fil-term" is taken, the rest are free; fil-term's owners are alice;
# FAKE_CURL_500 names a crate whose lookup fails with a server error
for a; do u=$a; done
case $u in
  */crates/"${FAKE_CURL_500:-none}") printf 500 ;;
  */owners) printf '{"users":[{"kind":"user","id":1,"login":"alice"}]}' ;;
  */crates/fil-term) printf 200 ;;
  *) printf 404 ;;
esac
EOF
cat > "$tmp/rc/cargo" << 'EOF'
#!/bin/sh
echo "$*" >> "$FAKE_CARGO_LOG"
if [ "$1" = owner ]; then
  [ -z "${FAKE_OWNER_MSG:-}" ] || echo "$FAKE_OWNER_MSG"
  exit "${FAKE_OWNER_RC:-0}"
fi
n=$(sed -n 's/^name = "\(.*\)"$/\1/p' Cargo.toml)
mkdir -p "$FAKE_CARGO_SEEN" && cp Cargo.toml "$FAKE_CARGO_SEEN/$n.Cargo.toml" && cp src/lib.rs "$FAKE_CARGO_SEEN/$n.lib.rs"
EOF
chmod +x "$tmp/rc/curl" "$tmp/rc/cargo"
# No token in the environment or in the (empty) CARGO_HOME, so the login check is the script's own.
mkdir -p "$tmp/empty"
rc_run() { env -u CARGO_REGISTRY_TOKEN CARGO_HOME="$tmp/empty" FIL_RESERVE_CARGO="$tmp/rc/cargo" FIL_RESERVE_CURL="$tmp/rc/curl" FIL_RESERVE_PACE_SECS=0 FAKE_CARGO_LOG="$tmp/rc/log" FAKE_CARGO_SEEN="$tmp/rc/seen" TMPDIR="$tmp" bash "$rc_script" "$@"; }
# rc_status CMD...: the command's exit status, whatever it prints.
rc_status() { "$@" > /dev/null 2>&1 && echo 0 || echo $?; }
: > "$tmp/rc/log"
expect_pass "reserve-crate-names: a run with no flag is a dry run, and a taken name is skipped" rc_run
expect_pass "reserve-crate-names: cargo was called eight times, always with --dry-run" test "$(grep -c -- '^publish --dry-run' "$tmp/rc/log")" = 8 -a "$(grep -vc -- '--dry-run' "$tmp/rc/log")" = 0
expect_fail "reserve-crate-names: no placeholder for the taken name" test -e "$tmp/rc/seen/fil-term.Cargo.toml"
: > "$tmp/rc/log"
expect_pass "reserve-crate-names: --dry-run --invite runs no cargo owner" rc_run --dry-run --invite bob
expect_pass "reserve-crate-names: --invite takes a login (exit 2)" test "$(rc_status rc_run --invite)" = 2
expect_pass "reserve-crate-names: --invite refuses a malformed login (exit 2)" test "$(rc_status rc_run --invite 'a b;c')" = 2
expect_pass "reserve-crate-names: --account takes a login (exit 2)" test "$(rc_status rc_run --account)" = 2
expect_pass "reserve-crate-names: an unknown argument is refused with exit 2" test "$(rc_status bash "$rc_script" --bogus)" = 2
expect_fail "reserve-crate-names: cargo owner was never run by a dry run" grep -q '^owner' "$tmp/rc/log"
expect_pass "reserve-crate-names: placeholder manifest (version, license, repository, no authors, no email)" \
  sh -c 'f=$1/fil-proto.Cargo.toml; grep -q "^version = \"0.0.1\"" "$f" && grep -q "^license = \"GPL-3.0-or-later\"" "$f" && grep -q "^repository = \"https://github.com/anthonytw/fil\"" "$f" && ! grep -q "authors" "$f" && ! grep -q "@" "$f"' sh "$tmp/rc/seen"
expect_pass "reserve-crate-names: placeholder lib.rs is only a doc comment" \
  sh -c '! grep -v "^//!" "$1/fil-proto.lib.rs" | grep -q .' sh "$tmp/rc/seen"
expect_pass "reserve-crate-names: the scratch directory is removed" sh -c '! ls "$1" | grep -q fil-reserve' sh "$tmp"
# --account: fil-term is taken, and alice owns it.
expect_pass "reserve-crate-names: --account alice owns the taken name, so the run passes" rc_run --account alice
rc_run --account bob > "$tmp/rc/out" 2>&1 && st=0 || st=$?
expect_pass "reserve-crate-names: a name taken by others exits 1" test "$st" = 1
expect_pass "reserve-crate-names: ...and is listed with its owners" grep -q 'taken by others: fil-term(alice)' "$tmp/rc/out"
# The publish path, on the fake cargo: nothing is published, nothing real is run.
: > "$tmp/rc/log"
rc_run --publish --yes --skip-login-check > "$tmp/rc/out" 2>&1 && st=0 || st=$?
expect_pass "reserve-crate-names: --publish publishes the eight free names" test "$st" = 0 -a "$(grep -c '^publish ' "$tmp/rc/log")" = 8
expect_fail "reserve-crate-names: ...none of them as a dry run" grep -q -- '--dry-run' "$tmp/rc/log"
# 8 publishes, a burst of 5: each of the last three waits (PACE_SECS is 0 here).
expect_pass "reserve-crate-names: ...and the burst wait comes before the sixth, seventh and eighth only" test "$(grep -c 'burst (5) is used up' "$tmp/rc/out")" = 3
: > "$tmp/rc/log"
rc_run --publish --yes > "$tmp/rc/out" 2>&1 && st=0 || st=$?
expect_pass "reserve-crate-names: without a token --publish stops (exit 1), and cargo is not called" test "$st" = 1 -a ! -s "$tmp/rc/log"
expect_pass "reserve-crate-names: ...with a cargo login message" grep -q 'cargo login' "$tmp/rc/out"
: > "$tmp/rc/log"
echo no | rc_run --publish --skip-login-check > "$tmp/rc/out" 2>&1 && st=0 || st=$?
expect_pass "reserve-crate-names: a typed 'no' aborts, and cargo is not called" test "$st" = 1 -a ! -s "$tmp/rc/log"
rm -rf "$tmp/rc/seen"
: > "$tmp/rc/log"
FAKE_CURL_500=fil-daemon rc_run --publish --yes --skip-login-check > "$tmp/rc/out" 2>&1 && st=0 || st=$?
expect_pass "reserve-crate-names: a server error on one name exits 1 and nothing is published for it" test "$st" = 1 -a ! -e "$tmp/rc/seen/fil-daemon.Cargo.toml" -a -e "$tmp/rc/seen/fil-proto.Cargo.toml"
# --invite with --account: no invitation for the crate that bob does not own (fil-term), carol for the rest.
: > "$tmp/rc/log"
rc_run --publish --yes --skip-login-check --account bob --invite carol > "$tmp/rc/out" 2>&1 && st=0 || st=$?
expect_pass "reserve-crate-names: invitations are sent for the new crates" test "$(grep -c '^owner --add carol ' "$tmp/rc/log")" = 8
expect_fail "reserve-crate-names: ...none for the crate that others own" grep -q '^owner --add carol fil-term' "$tmp/rc/log"
expect_pass "reserve-crate-names: ...and the run still exits 1 for it" test "$st" = 1
# The invitation step, on the fakes (sourcing the script runs nothing).
rc_invite() { # rc_invite OWNER CRATE [CARGO_OUTPUT [CARGO_STATUS]]: prints the outcome
  (
    export FIL_RESERVE_CARGO="$tmp/rc/cargo" FIL_RESERVE_CURL="$tmp/rc/curl" FAKE_CARGO_LOG="$tmp/rc/log" FAKE_OWNER_MSG="${3:-}" FAKE_OWNER_RC="${4:-0}"
    # shellcheck source=tests/release/reserve-crate-names.sh
    source "$rc_script"
    work=$tmp/rc/work
    mkdir -p "$work"
    invite_owner "$1" "$2"
  )
}
: > "$tmp/rc/log"
expect_pass "reserve-crate-names: an invitation is sent with cargo owner --add" test "$(rc_invite bob fil-proto)" = invited
expect_pass "reserve-crate-names: ...as 'owner --add bob fil-proto'" grep -qx 'owner --add bob fil-proto' "$tmp/rc/log"
: > "$tmp/rc/log"
expect_pass "reserve-crate-names: an owner already on the public list is not invited again" test "$(rc_invite alice fil-term)" = already
expect_fail "reserve-crate-names: ...and cargo owner was not run" grep -q '^owner' "$tmp/rc/log"
expect_pass "reserve-crate-names: a pending invitation (a success answer) counts as done" test "$(rc_invite bob fil-proto 'user bob already has a pending invitation' 0)" = already
expect_pass "reserve-crate-names: a crate this account does not own is left alone" test "$(rc_invite bob fil-proto 'error: only owners have permission to modify owners' 101)" = notours
expect_pass "reserve-crate-names: other permission errors are failures, not 'not ours'" test "$(rc_invite bob fil-proto 'error: 403 Forbidden: token lacks the change-owners scope' 101)" = failed
expect_pass "reserve-crate-names: any other failure is reported" test "$(rc_invite bob fil-proto 'error: could not resolve host' 101)" = failed
# The 429 retry, on a fake command (sourcing the script runs nothing).
rc_retry() { # rc_retry FAILS_BEFORE_SUCCESS OUTPUT: prints the number of calls and the status
  (
    export FIL_RESERVE_RETRY_SECS=0 FIL_RESERVE_MAX_RETRIES=3
    # shellcheck source=tests/release/reserve-crate-names.sh
    source "$rc_script"
    calls=0
    fake() { calls=$((calls + 1)); if [ "$calls" -le "$1" ]; then echo "$2"; return 101; fi; }
    st=0
    run_retry_429 "$tmp/rc/retry.log" fake "$1" "$2" 2> /dev/null || st=$?
    echo "$calls $st"
  )
}
expect_pass "reserve-crate-names: a 429 is waited out and retried" test "$(rc_retry 2 'the remote server responded with an error (status 429 Too Many Requests): too many new crates')" = "3 0"
expect_pass "reserve-crate-names: any other failure is not retried" test "$(rc_retry 2 'error: the remote server responded with an error (status 400 Bad Request)')" = "1 101"
expect_pass "reserve-crate-names: a bare 429 in the text is not a rate limit" test "$(rc_retry 2 'error: crate size 4290 bytes exceeds')" = "1 101"
expect_pass "reserve-crate-names: a 429 that does not clear gives up" test "$(rc_retry 99 'status 429 Too Many Requests')" = "4 101"

# ============================================================== 4d. the published crates (publish-crates.sh)
echo "== publish-crates.sh"
pc=$RELEASE_DIR/publish-crates.sh
pc_order=$("$pc" --list)
expect_pass "publish-crates: the crates with publish = [\"crates-io\"] are the nine reserved names" \
  test "$(printf '%s\n' $pc_order | LC_ALL=C sort | tr '\n' ' ')" = "$(printf '%s\n' $rc_names | LC_ALL=C sort | tr '\n' ' ')"
expect_pass "publish-crates: fil-proto first, fil-shell last" sh -c 'case "$1" in "fil-proto "*" fil-shell") exit 0 ;; *) exit 1 ;; esac' sh "$pc_order"
for n in $rc_names; do
  d=$n
  [ "$n" != fil-shell ] || d=fil
  expect_pass "crates: $n's LICENSE is a link to the root LICENSE (cargo packages its text)" test "$(readlink "$REPO_ROOT/crates/$d/LICENSE")" = ../../LICENSE
done
for c in fil-netsim fil-testkit fil-testbins; do
  expect_pass "publish-crates: $c is publish = false" grep -qx 'publish = false' "$REPO_ROOT/crates/$c/Cargo.toml"
done
wsv=$(cargo_version)
expect_pass "every [workspace.dependencies] version is the workspace's, exactly (=$wsv)" \
  sh -c '! awk "/^\[workspace.dependencies\]/ { p = 1; next } /^\[/ { p = 0 } p && /version/" "$1" | grep -v "version = \"=$2\"" | grep -q .' sh "$REPO_ROOT/Cargo.toml" "$wsv"
# --publish on fakes: fil-proto's version is already there, fil-term's lookup fails.
mkdir -p "$tmp/pc"
cat > "$tmp/pc/curl" << 'EOF2'
#!/bin/sh
for a; do u=$a; done
case $u in
  */crates/fil-proto/*) printf 200 ;;
  */crates/"${FAKE_PC_500:-none}"/*) printf 503 ;;
  *) printf 404 ;;
esac
EOF2
printf '#!/bin/sh\necho "$*" >> "$FAKE_PC_LOG"\n' > "$tmp/pc/cargo"
chmod +x "$tmp/pc/curl" "$tmp/pc/cargo"
pc_run() { env FIL_PUBLISH_CARGO="$tmp/pc/cargo" FIL_PUBLISH_CURL="$tmp/pc/curl" FAKE_PC_LOG="$tmp/pc/log" "$@" "$pc" --publish; }
: > "$tmp/pc/log"
expect_fail "publish-crates: --publish without a token stops" pc_run -u CARGO_REGISTRY_TOKEN
expect_pass "publish-crates: ...and calls no cargo" test ! -s "$tmp/pc/log"
expect_pass "publish-crates: --publish skips a version already on crates.io and publishes the other eight" pc_run CARGO_REGISTRY_TOKEN=fake
expect_pass "publish-crates: ...one at a time, in order" test "$(awk '{ print $NF }' "$tmp/pc/log" | tr '\n' ' ')" = "$(printf '%s\n' $pc_order | grep -v '^fil-proto$' | tr '\n' ' ')"
expect_pass "publish-crates: ...each with --no-verify (nothing compiles beside the token) and --locked" test "$(grep -c -- '^publish --locked --no-verify -p ' "$tmp/pc/log")" = 8
: > "$tmp/pc/log"
expect_fail "publish-crates: an unexpected answer from crates.io stops the run" pc_run CARGO_REGISTRY_TOKEN=fake FAKE_PC_500=fil-forward
expect_pass "publish-crates: ...after the crates before fil-forward" grep -qE ' fil-transport$' "$tmp/pc/log"
expect_fail "publish-crates: ...and before fil-forward and what depends on it" grep -qE ' (fil-forward|fil-client|fil-shell)$' "$tmp/pc/log"

# ============================================================== 4e. the packaging templates (packaging/render.sh)
echo "== packaging/render.sh (Homebrew formula, AUR PKGBUILD)"
render=$REPO_ROOT/packaging/render.sh
mkdir -p "$tmp/render"
h() { printf '%064d' "$1"; }
printf '%s  %s\n' "$(h 1)" fil-0.9.0-rc.1-aarch64-linux-musl.tar.gz "$(h 2)" fil-0.9.0-rc.1-source.tar.gz \
  "$(h 3)" fil-0.9.0-rc.1-universal-apple-darwin.tar.gz "$(h 4)" fil-0.9.0-rc.1-x86_64-linux-musl.tar.gz > "$tmp/render/SUMS"
expect_pass "render: the formula" "$render" brew 0.9.0-rc.1 "$tmp/render/SUMS" "$tmp/render/out"
expect_pass "render: ...takes each archive's checksum from SHA256SUMS" sh -c 'grep -q "sha256 \"$2\"" "$1" && grep -q "sha256 \"$3\"" "$1" && grep -q "sha256 \"$4\"" "$1" && grep -q "/v0.9.0-rc.1/fil-0.9.0-rc.1-universal-apple-darwin.tar.gz" "$1"' sh "$tmp/render/out/fil.rb" "$(h 1)" "$(h 3)" "$(h 4)"
if command -v ruby > /dev/null; then expect_pass "render: the formula is valid Ruby" ruby -c "$tmp/render/out/fil.rb"; fi
expect_pass "render: the PKGBUILD and .SRCINFO" "$render" aur 0.9.0-rc.1 "$tmp/render/SUMS" "$tmp/render/out"
expect_pass "render: ...pkgver has no '-' (0.9.0rc.1), _ver is the release's" sh -c 'grep -qx "pkgver=0.9.0rc.1" "$1/PKGBUILD" && grep -qx "_ver=0.9.0-rc.1" "$1/PKGBUILD" && grep -q "pkgver = 0.9.0rc.1" "$1/.SRCINFO"' sh "$tmp/render/out"
expect_pass "render: ...the x86_64 and aarch64 checksums in both files" sh -c 'grep -q "sha256sums_x86_64=(.$2.)" "$1/PKGBUILD" && grep -q "sha256sums_aarch64=(.$3.)" "$1/PKGBUILD" && grep -q "sha256sums_x86_64 = $2" "$1/.SRCINFO"' sh "$tmp/render/out" "$(h 4)" "$(h 1)"
expect_pass "render: the PKGBUILD is valid bash" bash -n "$tmp/render/out/PKGBUILD"
grep -v x86_64 "$tmp/render/SUMS" > "$tmp/render/SUMS.short"
expect_fail "render: a release without one of the archives is refused" "$render" brew 0.9.0-rc.1 "$tmp/render/SUMS.short" "$tmp/render/o2"
expect_fail "render: a version that is not a release version is refused" "$render" brew '0.9.0+x' "$tmp/render/SUMS" "$tmp/render/o3"

# ============================================================== 5. reproducible archives (Docker)
if [ "${FIL_RELEASE_TEST_DOCKER:-0}" = 1 ]; then
  echo "== reproducible archives (Docker)"
  rt=$tmp/repro
  mkdir -p "$rt/tree/crates/x/src" "$rt/bins"
  cp "$REPO_ROOT/Cross.toml" "$REPO_ROOT/rust-toolchain.toml" "$REPO_ROOT/LICENSE" "$REPO_ROOT/README.md" "$REPO_ROOT/THIRD-PARTY-NOTICES.md" "$rt/tree/"
  printf '[workspace]\nresolver = "3"\nmembers = ["crates/*"]\n[workspace.package]\nversion = "0.9.0"\nedition = "2024"\n' > "$rt/tree/Cargo.toml"
  printf '[package]\nname = "x"\nversion.workspace = true\nedition.workspace = true\n' > "$rt/tree/crates/x/Cargo.toml"
  echo 'fn main() {}' > "$rt/tree/crates/x/src/main.rs"
  git init -q -b main "$rt/tree"
  (cd "$rt/tree" && cargo generate-lockfile --offline 2> /dev/null && git add -A && git commit -q -m t)
  for t in $LINUX_TARGETS $UNIVERSAL; do mkdir -p "$rt/bins/$t" && echo "binary $t" > "$rt/bins/$t/fil"; done
  pk() { env FIL_RELEASE_ROOT="$rt/tree" SOURCE_DATE_EPOCH=1700000000 "${PK:-$RELEASE_DIR/package.sh}" --bins "$rt/bins" --out "$1"; }
  pk "$tmp/r1" > /dev/null 2> "$tmp/pk.log" || { cat "$tmp/pk.log"; exit 1; }
  sleep 2
  pk "$tmp/r2" > /dev/null 2>&1
  expect_pass "two assemblies of the same inputs give byte-identical SHA256SUMS" cmp "$tmp/r1/SHA256SUMS" "$tmp/r2/SHA256SUMS"
  # mutation: leave the tar mtimes alone
  trap 'rm -rf "$tmp" "$RELEASE_DIR/.package-mutant.sh"' EXIT INT TERM
  sed 's/--mtime="@\$SOURCE_DATE_EPOCH" //' "$RELEASE_DIR/package.sh" > "$RELEASE_DIR/.package-mutant.sh"
  chmod +x "$RELEASE_DIR/.package-mutant.sh"
  if cmp -s "$RELEASE_DIR/package.sh" "$RELEASE_DIR/.package-mutant.sh"; then flunk "mutation 'mtimes' did not change package.sh (the test is stale)"; fi
  PK="$RELEASE_DIR/.package-mutant.sh" pk "$tmp/m1" > /dev/null 2>&1 || true
  sleep 2
  PK="$RELEASE_DIR/.package-mutant.sh" pk "$tmp/m2" > /dev/null 2>&1 || true
  rm -f "$RELEASE_DIR/.package-mutant.sh"
  expect_fail "mutation: tar mtimes left alone, and the two assemblies' SHA256SUMS then differ" cmp "$tmp/m1/SHA256SUMS" "$tmp/m2/SHA256SUMS"
  expect_pass "the assembly writes the four packages too" test "$(grep -cE '\.(deb|rpm)$' "$tmp/r1/SHA256SUMS")" = 4
  # mutation: nfpm without the fixed mtime (and without SOURCE_DATE_EPOCH, which it also reads) stamps the packages with the time of the run
  sed -e '/mtime: /d' -e 's/-e SOURCE_DATE_EPOCH //' "$RELEASE_DIR/linux-packages.sh" > "$RELEASE_DIR/.pkg-mutant.sh"
  chmod +x "$RELEASE_DIR/.pkg-mutant.sh"
  trap 'rm -rf "$tmp" "$RELEASE_DIR/.package-mutant.sh" "$RELEASE_DIR/.pkg-mutant.sh"' EXIT INT TERM
  if cmp -s "$RELEASE_DIR/linux-packages.sh" "$RELEASE_DIR/.pkg-mutant.sh"; then flunk "mutation 'package mtime' did not change linux-packages.sh (the test is stale)"; fi
  mkdir -p "$tmp/p1" "$tmp/p2"
  env FIL_RELEASE_ROOT="$rt/tree" SOURCE_DATE_EPOCH=1700000000 "$RELEASE_DIR/.pkg-mutant.sh" --archives "$tmp/r1" --out "$tmp/p1" > /dev/null 2>&1 || true
  sleep 2
  env FIL_RELEASE_ROOT="$rt/tree" SOURCE_DATE_EPOCH=1700000000 "$RELEASE_DIR/.pkg-mutant.sh" --archives "$tmp/r1" --out "$tmp/p2" > /dev/null 2>&1 || true
  rm -f "$RELEASE_DIR/.pkg-mutant.sh"
  expect_fail "mutation: packages without the fixed mtime, and two builds then differ" cmp "$tmp/p1/fil_0.9.0_amd64.deb" "$tmp/p2/fil_0.9.0_amd64.deb"
else
  echo "== reproducible archives: skipped (set FIL_RELEASE_TEST_DOCKER=1; needs Docker)"
fi

echo
echo "release-test: $passed passed, $failed failed"
[ "$failed" = 0 ]
