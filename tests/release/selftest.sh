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

# ============================================================== 4. release-check on synthetic releases
echo "== release-check"
# A scratch tree (a git repository holding the files check.sh reads), stand-in binaries (shell
# scripts that print the version line, so `--version` runs; a PATH shim answers file(1) for them),
# and archives made with whatever tar the machine has.
mk_tree() { # DIR VER
  local d=$1
  mkdir -p "$d/crates/fil-proto/src" "$d/crates/fil-proto/tests/golden"
  cp "$REPO_ROOT/Cargo.toml" "$REPO_ROOT/Cross.toml" "$REPO_ROOT/rust-toolchain.toml" "$REPO_ROOT/justfile" "$REPO_ROOT/LICENSE" "$REPO_ROOT/README.md" "$d/"
  cp "$REPO_ROOT/crates/fil-proto/src/limits.rs" "$d/crates/fil-proto/src/"
  cp "$REPO_ROOT/crates/fil-proto/tests/golden/wire.txt" "$d/crates/fil-proto/tests/golden/"
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
EOT
  chmod 755 "$1"
}
mkdir -p "$tmp/shim"
cat > "$tmp/shim/file" << 'EOT'
#!/bin/sh
# Test shim for file(1): the real one cannot tell a shell script from a static ELF.
f=$1
if grep -q FAKE-ELF-X86 "$f" 2> /dev/null; then echo "$f: ELF 64-bit LSB executable, x86-64, statically linked"
elif grep -q FAKE-ELF-AARCH64 "$f" 2> /dev/null; then echo "$f: ELF 64-bit LSB executable, ARM aarch64, statically linked"
elif grep -q FAKE-MACHO "$f" 2> /dev/null; then echo "$f: Mach-O universal binary with 2 architectures: [x86_64] [arm64]"
else /usr/bin/file "$@"; fi
EOT
chmod +x "$tmp/shim/file"
tar_flags=
tar --version 2> /dev/null | grep -q 'GNU tar' && tar_flags='--owner=0 --group=0 --numeric-owner'

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
    cp "$tree/README.md" "$tree/LICENSE" "$d/share/doc/fil/"
  done
  ROOT=$saved
}
# A file named .second-top-LABEL in the stage becomes a second top-level entry of that archive.
pack_release() { # uses $tree $stage $rel $VER
  local d src="$stage/fil-$VER-source" lbl
  rm -rf "$rel" "$src"
  mkdir -p "$rel" "$src/vendor" "$src/.cargo"
  cp "$tree/LICENSE" "$src/"
  # A workspace of one dependency-free crate: `cargo metadata --offline` resolves it from nothing.
  printf '[workspace]\nresolver = "3"\nmembers = ["crates/x"]\n' > "$src/Cargo.toml"
  mkdir -p "$src/crates/x/src"
  printf '[package]\nname = "x"\nversion = "0.1.0"\nedition = "2024"\n' > "$src/crates/x/Cargo.toml"
  echo 'fn main() {}' > "$src/crates/x/src/main.rs"
  (cd "$src" && cargo generate-lockfile --offline > /dev/null 2>&1)
  : > "$src/.cargo/config.toml"
  for d in "$stage"/fil-*; do
    lbl=${d##*/}
    lbl=${lbl#fil-"$VER"-}
    extra=
    [ -e "$stage/.second-top-$lbl" ] && extra=.second-top-$lbl
    # shellcheck disable=SC2086
    (cd "$stage" && tar $tar_flags -czf "$rel/fil-$VER-$lbl.tar.gz" "fil-$VER-$lbl" $extra)
  done
  write_sums
}
write_sums() { (cd "$rel" && for f in $(printf '%s\n' fil-"$VER"-*.tar.gz | LC_ALL=C sort); do printf '%s  %s\n' "$(sha256 "$f")" "$f"; done > SHA256SUMS); }

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
  (cd "$stage" && eval "$@")
  pack_release
  expect_fail "mutation: $name" check --dry-run --no-tree "$rel"
}

new_case 0.9.0
expect_pass "a clean dry run passes" check --dry-run --no-tree "$rel"
expect_pass "a clean tag run passes (changelog, tag, wire)" check --tag v0.9.0 --no-tree "$rel"
expect_fail "--complete fails while LIC and DOC files are pending" check --tag v0.9.0 --complete --no-tree "$rel"
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
mutated "bin/fil is missing" "rm fil-0.9.0-aarch64-linux-musl/bin/fil"
mutated "the license is missing" "rm fil-0.9.0-x86_64-linux-musl/share/doc/fil/LICENSE"
mutated "the binary is not static" "perl -pi -e 's/FAKE-ELF-X86/FAKE-DYNAMIC/' fil-0.9.0-x86_64-linux-musl/bin/fil"

# SHA256SUMS
new_case 0.9.0
cp "$rel/SHA256SUMS" "$tmp/sums.good"
sed '1d' "$tmp/sums.good" > "$rel/SHA256SUMS"
expect_fail "mutation: SHA256SUMS lists only three files" check --dry-run --no-tree "$rel"
{ tail -n 1 "$tmp/sums.good"; sed '$d' "$tmp/sums.good"; } > "$rel/SHA256SUMS"
expect_fail "mutation: SHA256SUMS is not sorted" check --dry-run --no-tree "$rel"
{ cat "$tmp/sums.good"; echo "$(sha256 "$tmp/sums.good")  SHA256SUMS"; } > "$rel/SHA256SUMS"
expect_fail "mutation: SHA256SUMS covers itself" check --dry-run --no-tree "$rel"
sed '1s/^./0/;1s/^0/f/' "$tmp/sums.good" > "$rel/SHA256SUMS"
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

# ============================================================== 5. reproducible archives (Docker)
if [ "${FIL_RELEASE_TEST_DOCKER:-0}" = 1 ]; then
  echo "== reproducible archives (Docker)"
  rt=$tmp/repro
  mkdir -p "$rt/tree/crates/x/src" "$rt/bins"
  cp "$REPO_ROOT/Cross.toml" "$REPO_ROOT/rust-toolchain.toml" "$REPO_ROOT/LICENSE" "$REPO_ROOT/README.md" "$rt/tree/"
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
  sed 's/--mtime="@\$SOURCE_DATE_EPOCH" //' "$RELEASE_DIR/package.sh" > "$RELEASE_DIR/.package-mutant.sh"
  chmod +x "$RELEASE_DIR/.package-mutant.sh"
  if cmp -s "$RELEASE_DIR/package.sh" "$RELEASE_DIR/.package-mutant.sh"; then flunk "mutation 'mtimes' did not change package.sh (the test is stale)"; fi
  PK="$RELEASE_DIR/.package-mutant.sh" pk "$tmp/m1" > /dev/null 2>&1 || true
  sleep 2
  PK="$RELEASE_DIR/.package-mutant.sh" pk "$tmp/m2" > /dev/null 2>&1 || true
  rm -f "$RELEASE_DIR/.package-mutant.sh"
  expect_fail "mutation: tar mtimes left alone, and the two assemblies' SHA256SUMS then differ" cmp "$tmp/m1/SHA256SUMS" "$tmp/m2/SHA256SUMS"
else
  echo "== reproducible archives: skipped (set FIL_RELEASE_TEST_DOCKER=1; needs Docker)"
fi

echo
echo "release-test: $passed passed, $failed failed"
[ "$failed" = 0 ]
