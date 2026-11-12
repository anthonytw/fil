#!/usr/bin/env bash
# `just release-test`: tests of the release scripts themselves (docs/m4/plan.md, REL, "Tests" and
# "Mutation"). Part of `just ci`. Needs git, ssh-keygen, perl and tar; no network, and no Docker
# unless FIL_RELEASE_TEST_DOCKER=1 (then the reproducible-archive tests run too: they pull the
# digest-pinned image of Cross.toml).
#
# Every "mutation" below changes ONE thing in a scratch copy and requires the matching check to
# fail, so a check that quietly stopped checking is a test failure.
#
# The whole script is one { ... } group that ends in `exit`: bash reads all of it before it runs any
# of it, so rewriting this file in place while it runs (an editor that writes in place) cannot derail
# the run. The scripts it starts are read when they start.
{
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
# the check profile: the two manual check workflows
for cw in linux-check macos-check; do
  expect_pass "the $cw workflow passes the check-profile lint" "$RELEASE_DIR/lint-workflow.sh" --check "$REPO_ROOT/.github/workflows/$cw.yml"
done
cw=$REPO_ROOT/.github/workflows/linux-check.yml
mutate_cw() {
  local name=$1 expr=$2
  perl -0pe "$expr" "$cw" > "$tmp/cw.yml"
  if cmp -s "$cw" "$tmp/cw.yml"; then
    flunk "check-profile mutation '$name' did not change the workflow (the test is stale)"
  else
    expect_fail "check-profile mutation: $name" "$RELEASE_DIR/lint-workflow.sh" --check "$tmp/cw.yml"
  fi
}
mutate_cw "an action is pinned by a tag" 's/(uses: actions\/checkout@)[0-9a-f]{40}/${1}v7/'
mutate_cw "a checkout keeps credentials" 's/persist-credentials: false/persist-credentials: true/'
mutate_cw "a pull_request trigger" 's/(on:\n)/${1}  pull_request:\n/'
mutate_cw "the workflow gets contents: write" 's/(permissions:\n  contents: )read/${1}write/'
mutate_cw "a matrix value reaches a run: line" 's/run: just realnet "\$PART"/run: just realnet \${{ matrix.part }}/'
mutate_cw "a secret is used" 's/(PART: )/SECRET_X: \${{ secrets.X }}\n          $1/'
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
mutate_wf "workflow_dispatch is inline with inputs" 's/\n  workflow_dispatch:\n/\n  workflow_dispatch: { inputs: { x: { type: string } } }\n/'
mutate_wf "the push trigger is inline with a branch" 's/\n  push:\n    tags: \["v\*"\]\n/\n  push: { branches: [main], tags: ["v*"] }\n/'
mutate_wf "a secret by index" 's/(GH_TOKEN: )\$\{\{ github.token \}\}/$1\$\{\{ secrets[\x27RELEASE_TOKEN\x27] \}\}/'
mutate_wf "all secrets through toJSON" 's/(GH_TOKEN: )\$\{\{ github.token \}\}/$1\$\{\{ toJSON(secrets) \}\}/'
mutate_wf "a secret in capitals by index" 's/(GH_TOKEN: )\$\{\{ github.token \}\}/$1\$\{\{ SECRETS[\x27X\x27] \}\}/'
mutate_wf "a secret with a capital letter" 's/(GH_TOKEN: )\$\{\{ github.token \}\}/$1\$\{\{ Secrets.RELEASE_TOKEN \}\}/'
mutate_wf "a secret, space before the bracket" 's/(GH_TOKEN: )\$\{\{ github.token \}\}/$1\$\{\{ secrets [\x27X\x27] \}\}/'
mutate_wf "a secret inside toJSON with spaces" 's/(GH_TOKEN: )\$\{\{ github.token \}\}/$1\$\{\{ toJSON( secrets ) \}\}/'
mutate_wf "a secret passed to format" 's/(GH_TOKEN: )\$\{\{ github.token \}\}/$1\$\{\{ format(\x27{0}\x27, secrets) \}\}/'
mutate_wf "secrets: inherit" 's/(  prepare:\n)/$1    secrets: inherit\n/'
mutate_wf "prepare's actions: read becomes write" 's/(  prepare:.*?      actions: )read/${1}write/s'
mutate_wf "another job gets id-token: write" 's/(  packages-arm64:.*?    permissions:\n      contents: read\n)/$1      id-token: write\n/s'
mutate_wf "a step runs just itself (it would read the unowned justfile)" 's/run: tests\/release\/just.sh release-deps/run: just release-deps/'
mutate_wf "a run block runs just after &&" 's/(tests\/release\/lint-workflow.sh --check [^\n]*)/$1 && just release-test/'
mutate_wf "the self-test job no longer gates publish" 's/needs: \[prepare, release-test, package/needs: [prepare, package/'
mutate_wf "prepare runs the self-test again" 's/(run: tests\/release\/just.sh crates-check\n)/$1      - run: tests\/release\/just.sh release-test\n/'
mutate_wf "prepare no longer checks the lock's sources" 's/check bans licenses sources/check bans licenses/'
mutate_wf "prepare checks the lock without --locked" 's/cargo deny --locked check/cargo deny check/'
mutate_wf "the lock's check moved out of prepare" 's/      - name: The lock.s sources, bans and licenses[^\n]*\n        run: cargo deny --locked check bans licenses sources\n//'
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

# --ref (the commit that is rebuilt) must be a full 40-hex SHA, checked before anything else
for badref in 314e5bc main v1 "$(printf 'A%.0s' {1..40})" "$(printf 'a%.0s' {1..41})"; do
  expect_fail "release-verify refuses --ref $badref" rv 1 --dry-run --ref "$badref"
  grep -q '40-hex commit SHA' "$tmp/out.log" && pass "...with the 40-hex message" || { flunk "...with the 40-hex message"; sed "s/^/       | /" "$tmp/out.log"; }
done
# a release (not a dry run) rebuilds the tag's commit: another --ref is refused, the tag's own is fine
tagcommit=$(git -C "$tmp/origin" rev-parse "refs/tags/v1^{commit}")
expect_fail "release-verify refuses a --ref that is not the tag's commit" rv 1 --ref "$(printf 'c%.0s' {1..40})"
grep -q "is not the commit of v1" "$tmp/out.log" && pass "...with that message" || { flunk "...with that message"; sed "s/^/       | /" "$tmp/out.log"; }
rv 1 --ref "$tagcommit" > "$tmp/out.log" 2>&1 || true
if grep -q "is not the commit of" "$tmp/out.log"; then flunk "release-verify accepts --ref equal to the tag's commit"; else pass "release-verify accepts --ref equal to the tag's commit"; fi
goodref=$(printf 'a%.0s' {1..40})
rv 1 --dry-run --ref "$goodref" > "$tmp/out.log" 2>&1 || true
if grep -q '40-hex commit SHA' "$tmp/out.log"; then flunk "release-verify accepts a full 40-hex --ref"; else pass "release-verify accepts a full 40-hex --ref"; fi
expect_fail "rebuild.sh refuses a short ref" bash "$RELEASE_DIR/rebuild.sh" /nonexistent 314e5bc rebuild-x
grep -q '40-hex commit SHA' "$tmp/out.log" && pass "...before touching Docker or the repo" || { flunk "...before touching Docker or the repo"; sed "s/^/       | /" "$tmp/out.log"; }

# the draft release's attestation commands are the narrow ones
echo "== release notes (notes.sh)"
sha=$(printf 'b%.0s' {1..40})
"$RELEASE_DIR/notes.sh" 9.9.9 o/r "$sha" > "$tmp/notes.md"
for flag in '--repo o/r' '--cert-identity https://github.com/o/r/.github/workflows/release.yml@refs/tags/v9.9.9' '--source-ref refs/tags/v9.9.9' "--source-digest $sha" "--signer-digest $sha" '--deny-self-hosted-runners'; do
  expect_pass "notes.sh: every attestation command has $flag" test "$(grep -c -F -- "$flag" "$tmp/notes.md")" = "$(grep -c '^gh attestation verify' "$tmp/notes.md")"
done
expect_pass "notes.sh: there are two attestation commands" test "$(grep -c '^gh attestation verify' "$tmp/notes.md")" = 2
expect_fail "notes.sh: no commit is refused" "$RELEASE_DIR/notes.sh" 9.9.9 o/r
expect_fail "notes.sh: a short commit is refused" "$RELEASE_DIR/notes.sh" 9.9.9 o/r abc123
expect_fail "notes.sh: an uppercase commit is refused" "$RELEASE_DIR/notes.sh" 9.9.9 o/r "$(printf 'B%.0s' {1..40})"
expect_fail "notes.sh: a commit with a second line is refused" "$RELEASE_DIR/notes.sh" 9.9.9 o/r "$sha
main"

# the files that decide what a release is, or who signs it, need the owner's review, and only those
# (narrowed on 2026-11-11, owner)
echo "== CODEOWNERS"
for path in /Cargo.toml /deny.toml /about.toml /about.hbs /.cargo/ /.gitattributes '/packaging/**' /flake.nix /flake.lock '/docs/man/**' /.github/CODEOWNERS /.github/macos-team-id /.github/package-signing-key.asc /.github/release-signers '/.github/workflows/**' '/tests/release/**' /Cross.toml /rust-toolchain.toml /rust-toolchain; do
  expect_pass "CODEOWNERS lists $path" grep -qxF "$path @anthonytw" "$REPO_ROOT/.github/CODEOWNERS"
done
for path in /justfile; do
  expect_fail "CODEOWNERS does not list $path" grep -q "^$path" "$REPO_ROOT/.github/CODEOWNERS"
done
# codeowners_owner FILE PATH...: each PATH's owners under FILE (GitHub's rule: the last matching
# pattern wins, and a pattern with no owners leaves the path unowned), as "PATH OWNERS" or "PATH -".
# It knows the three pattern forms the file uses (/exact, /dir/, /dir/**) and refuses any other.
codeowners_owner() {
  python3 -I - "$@" << 'EOF2'
import sys
rules = []
for line in open(sys.argv[1]):
    f = line.split("#", 1)[0].split()
    if not f:
        continue
    p = f[0]
    if not p.startswith("/") or ("*" in p and not p.endswith("/**")) or p.count("*") > 2:
        sys.exit(f"codeowners_owner: a pattern this test cannot read: {p}")
    rules.append((p, f[1:]))
def match(p, path):
    if p.endswith("/**"):
        return path.startswith(p[:-2])
    if p.endswith("/"):
        return path.startswith(p)
    return path == p
for path in sys.argv[2:]:
    owners = None
    for p, o in rules:
        if match(p, path):
            owners = o
    print(path, " ".join(owners) if owners else "-")
EOF2
}
owned=() unowned=(/.github/workflows/linux-check.yml /.github/workflows/macos-check.yml /justfile /tests/release/selftest.sh /tests/release/selftest_macho.py /tests/release/selftest_pkg.py /docs/man/fil.1)
for f in "$REPO_ROOT"/tests/release/*; do
  case ${f##*/} in selftest.sh | selftest_macho.py | selftest_pkg.py) ;; *) owned+=("/tests/release/${f##*/}") ;; esac
done
# a file added later in an owned directory is owned until CODEOWNERS says otherwise
owned+=(/.github/workflows/release.yml /.github/workflows/new.yml /tests/release/new-step.sh /packaging/new/x /.cargo/config.toml /flake.nix /flake.lock /rust-toolchain /docs/man/strip-links.lua /docs/man/build.sh /docs/man/install-pandoc.sh /docs/man/new-filter.lua)
codeowners_owner "$REPO_ROOT/.github/CODEOWNERS" "${owned[@]}" "${unowned[@]}" > "$tmp/owners" 2>&1 || cat "$tmp/owners" >&2
for path in "${owned[@]}"; do
  expect_pass "CODEOWNERS: $path is the owner's" grep -qxF "$path @anthonytw" "$tmp/owners"
done
for path in "${unowned[@]}"; do
  expect_pass "CODEOWNERS: $path has no owner" grep -qxF "$path -" "$tmp/owners"
done
expect_pass "CODEOWNERS: release.just, just.sh, entry-check.sh and recipes.sh are among the owned files" sh -c 'for f in release.just just.sh entry-check.sh recipes.sh; do grep -qxF "/tests/release/$f @anthonytw" "$1" || exit 1; done' sh "$tmp/owners"
{ cat "$REPO_ROOT/.github/CODEOWNERS"; echo "/tests/release/release.just"; } > "$tmp/CODEOWNERS.mut"
expect_pass "mutation: an ownerless line after the directory unowns the file" test "$(codeowners_owner "$tmp/CODEOWNERS.mut" /tests/release/release.just)" = "/tests/release/release.just -"
# ...and every path it lists that has no glob exists (a rename would silently drop the protection),
# except .cargo/, .gitattributes and rust-toolchain, which are listed so that adding one needs the owner
codeowners_missing() { # FILE: the non-glob paths in a CODEOWNERS file that do not exist
  local cpath
  while read -r cpath _; do
    case $cpath in '#'* | '' | *'*'* | /.cargo/ | /.gitattributes | /rust-toolchain) continue ;; esac
    [ -e "$REPO_ROOT$cpath" ] || echo "$cpath"
  done < "$1"
}
expect_pass "every CODEOWNERS path exists (but .cargo/, .gitattributes and rust-toolchain)" test -z "$(codeowners_missing "$REPO_ROOT/.github/CODEOWNERS")"
{ cat "$REPO_ROOT/.github/CODEOWNERS"; echo "/no-such-file @anthonytw"; } > "$tmp/CODEOWNERS.mut"
expect_pass "mutation: a CODEOWNERS path that does not exist is caught" test "$(codeowners_missing "$tmp/CODEOWNERS.mut")" = /no-such-file

# the release path reads the owned release.just alone, never the unowned justfile
echo "== release recipes (recipes.sh, just.sh, entry-check.sh)"
expect_pass "recipes.sh passes on the tree" "$RELEASE_DIR/recipes.sh" "$REPO_ROOT"
rt=$tmp/recipes
# rt_tree NAME: a copy of the justfile and tests/release in $rt/NAME, to mutate
rt_tree() { mkdir -p "$rt/$1/tests" && cp "$REPO_ROOT/justfile" "$rt/$1/" && cp -R "$REPO_ROOT/tests/release" "$rt/$1/tests/"; }
rt_tree clean
expect_pass "recipes.sh passes on an unchanged copy" "$RELEASE_DIR/recipes.sh" "$rt/clean"
# rt_mut NAME PERL_EXPR [FILE]: a copy with FILE (default release.just) changed; recipes.sh must fail
rt_mut() {
  local name=$1 expr=$2 f=${3:-tests/release/release.just}
  rt_tree "m$((++rt_n))"
  perl -0pi -e "$expr" "$rt/m$rt_n/$f"
  if cmp -s "$REPO_ROOT/$f" "$rt/m$rt_n/$f"; then flunk "recipes mutation '$name' did not change $f (the test is stale)"; else
    expect_fail "mutation: $name" "$RELEASE_DIR/recipes.sh" "$rt/m$rt_n"
  fi
}
rt_n=0
rt_mut "release.just imports a file" 's/^(set lazy\n)/${1}import '"'"'x.just'"'"'\n/m'
rt_mut "release.just imports a file if it exists (import?)" 's/^(set lazy\n)/${1}import? '"'"'x.just'"'"'\n/m'
rt_mut "release.just mounts a module" 's/^(set lazy\n)/${1}mod zz\n/m'
rt_mut "release.just mounts a module if it exists (mod?)" 's/^(set lazy\n)/${1}mod? zz '"'"'zz.just'"'"'\n/m'
rt_mut "release.just without set lazy" 's/^set lazy\n//m'
rt_mut "release.just runs in its own directory" 's/^set working-directory := .*\n//m'
rt_mut "release.just runs one level up only" "s/^set working-directory := '\\.\\.\\/\\.\\.'/set working-directory := '..'/m"
rt_mut "a recipe in release.just that recipes.sh does not list" 's/\z/\nrelease-extra:\n    true\n/'
rt_mut "release without the entry check" 's/^(release version \*args:\n)    tests\/release\/entry-check.sh release\n/$1/m'
rt_mut "release-keys checks the wrong recipe's entry" 's/entry-check.sh release-keys$/entry-check.sh release/m'
rt_mut "build-binary.sh runs just itself" 's/"\$ROOT\/tests\/release\/just.sh" no-test-hooks/just no-test-hooks/' tests/release/build-binary.sh
rt_mut "check.sh runs just itself, in a subshell" 's/\("\$ROOT\/tests\/release\/just.sh" release-deps\)/(cd "\$ROOT" \&\& just release-deps)/' tests/release/check.sh
rt_mut "a release script runs just after a ;" 's/^(set -euo pipefail\n)/${1}true; just release-deps\n/m' tests/release/package.sh
expect_pass "recipes.sh: a message that names just is not a command" sh -c 'grep -q "\"run: just release-keys" "$1"; "$2" "$3"' sh "$RELEASE_DIR/release-keys.sh" "$RELEASE_DIR/recipes.sh" "$rt/clean"

# The reviewer's hijacks (PR #74): a justfile whose assignments run code, or that mounts or imports
# a file that does, must not run anything through tests/release/just.sh. Through the top-level
# justfile they do run (which shows the test is real), and the owner's commands refuse to.
# ec_h CMD...: CMD with entry-check.sh's walk ended at this script's process, so that the `just ci`
# (or anything else) above the self-test is not part of the test
ec_h() { env FIL_RELEASE_SELFTEST=1 FIL_ENTRY_CHECK_STOP=$$ "$@"; }
rt_tree hijack
h=$rt/hijack
{ printf '_x := `touch PWNED1`\n_y := shell("touch PWNED2")\n'; printf "mod? zz 'zz.just'\nimport? 'x.just'\n"; } >> "$h/justfile"
printf '_z := `touch PWNED3`\n' > "$h/zz.just"
printf '_w := `touch PWNED4`\n' > "$h/x.just"
expect_fail "hijack: tests/release/just.sh no-test-hooks fails on a missing binary..." "$h/tests/release/just.sh" no-test-hooks /nonexistent
expect_pass "hijack: ...and ran nothing of the justfile or its modules" sh -c 'cd "$1" && ! ls PWNED* > /dev/null 2>&1' sh "$h"
(cd "$h" && just no-test-hooks /nonexistent) > /dev/null 2>&1 || true
expect_pass "hijack: the same through the justfile runs all four (the test is real)" sh -c 'cd "$1" && test -f PWNED1 && test -f PWNED2 && test -f PWNED3 && test -f PWNED4' sh "$h"
rm -f "$h"/PWNED*
expect_pass "hijack: recipes.sh passes (the justfile is not on the release path)" "$RELEASE_DIR/recipes.sh" "$h"
expect_fail "hijack: just release-keys through the justfile is refused..." ec_h sh -c 'cd "$1" && just release-keys status' sh "$h"
grep -q 'release: STOP: release-keys was started through a justfile other than tests/release/release.just' "$tmp/out.log" && pass "...by the entry check, which names the owned command" || { flunk "...by the entry check, which names the owned command"; sed "s/^/       | /" "$tmp/out.log"; }
# just.sh runs release.just's recipes in the repository root, from wherever it is run
expect_pass "just.sh: the recipes run in the repository root, from any directory" sh -c 'cd / && "$1/tests/release/just.sh" --evaluate > /dev/null && cd "$1/tests" && test "$("$1/tests/release/just.sh" --dry-run release-test 2>&1 | head -n 1)" = "tests/release/lint-workflow.sh .github/workflows/release.yml"' sh "$h"
# entry-check.sh: refuses under any `just` ancestor that is not release.just; a direct call is fine
ec=$RELEASE_DIR/entry-check.sh
# fakejust JUSTFILE|- CMD...: CMD under a process whose argv is `just [--justfile JUSTFILE] ...`
cat > "$tmp/fakejust" << 'EOF2'
#!/usr/bin/env bash
jf=$1
shift
if [ "$jf" = - ]; then exec -a just bash -c '"$@"; rc=$?; exit $rc' bash "$@"; fi
exec -a just bash -c 'shift 2; "$@"; rc=$?; exit $rc' bash --justfile "$jf" "$@"
EOF2
chmod +x "$tmp/fakejust"
fj=$tmp/fakejust
expect_pass "entry-check: no just above it passes" ec_h "$ec" release
expect_fail "entry-check: a just ancestor with no --justfile is refused" ec_h "$fj" - "$ec" release
expect_fail "entry-check: a just ancestor with another justfile is refused" ec_h "$fj" /x/justfile "$ec" release
expect_pass "entry-check: a just ancestor running tests/release/release.just passes" ec_h "$fj" /x/tests/release/release.just "$ec" release
expect_fail "entry-check: release.just under the top-level justfile is refused" ec_h "$fj" - "$fj" /x/tests/release/release.just "$ec" release
expect_fail "entry-check: the stop override without FIL_RELEASE_SELFTEST=1 is refused" env -u FIL_RELEASE_SELFTEST FIL_ENTRY_CHECK_STOP=$$ "$ec" release
grep -q 'FIL_ENTRY_CHECK_STOP is set (a test-only override)' "$tmp/out.log" && pass "...with that message" || { flunk "...with that message"; sed "s/^/       | /" "$tmp/out.log"; }


# ============================================================== 4. release-check on synthetic releases
echo "== release-check"
# A scratch tree (a git repository holding the files check.sh reads), stand-in binaries (shell
# scripts that print the version line, so `--version` runs; a PATH shim answers file(1) for them),
# and archives made with whatever tar the machine has.
mk_tree() { # DIR VER
  local d=$1
  mkdir -p "$d/crates/fil-proto/src" "$d/crates/fil-proto/tests/golden" "$d/crates/fil-transport/src/pipe" "$d/tests/release"
  cp "$REPO_ROOT/Cargo.toml" "$REPO_ROOT/Cross.toml" "$REPO_ROOT/rust-toolchain.toml" "$REPO_ROOT/LICENSE" "$REPO_ROOT/README.md" "$REPO_ROOT/THIRD-PARTY-NOTICES.md" "$d/"
  cp "$REPO_ROOT/tests/release/release.just" "$REPO_ROOT/tests/release/just.sh" "$d/tests/release/"
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
  cp "$rel/fil-$VER-universal-apple-darwin.tar.gz" "$rel/$(ci_archive_name "$VER")"
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
cp "$rel/$(ci_archive_name 0.9.0)" "$tmp/ci.keep"
rm "$rel/$(ci_archive_name 0.9.0)"
write_sums
expect_fail "release-check: a release without CI's .ci macOS archive fails" check --dry-run --no-tree "$rel"
cp "$tmp/ci.keep" "$rel/$(ci_archive_name 0.9.0)"
write_sums
expect_pass "release-check: ...and passes with it again" check --dry-run --no-tree "$rel"
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
# SEC-2: whole multi-label names under any domain, and IPv6 literals outside the documentation ranges
new_case 0.9.0
mkdir -p "$tree/crates/x/tests"
printf 'let a = "user@build5.corp.acme.io:22"; let b = "ssh://build6.corp.acme.io/x"; let c = ["8080:fd00::6"];\n' > "$tree/crates/x/tests/fixture.rs"
printf 'host = "ci.lab.acme.net"\nnas = "nas.home.arpa"\nweb = "build.corp.acme.io"\nula = "fd00::1"\nfull = "fe80:0:0:0:1:2:3:4"\nok1 = "fil.anthonywertz.com"\nok2 = "2001:db8::1"\nok3 = "::1"\nok4 = "10:30:45"\nok5 = "38:2::10:200:30"\n' > "$tree/crates/x/tests/fixture.toml"
expect_pass "multi-label and IPv6 fixtures that are not in the binaries pass" check --dry-run --no-tree "$rel"
for leak in build5.corp.acme.io build6.corp.acme.io fd00::6 ci.lab.acme.net nas.home.arpa build.corp.acme.io fd00::1 fe80:0:0:0:1:2:3:4; do
  stage_release
  echo "# connects to $leak" >> "$stage/fil-0.9.0-universal-apple-darwin/bin/fil"
  pack_release
  expect_fail "mutation: the fixture $leak is in bin/fil" check --dry-run --no-tree "$rel"
  grep -qF "$leak" "$tmp/out.log" && pass "...and the failure names it" || flunk "...and the failure names it"
done
stage_release
echo '# fil.anthonywertz.com 2001:db8::1 ::1 10:30:45 fd00::2 ci.lab.acme.org' >> "$stage/fil-0.9.0-universal-apple-darwin/bin/fil"
pack_release
expect_pass "the project's own name, documentation and loopback addresses, and other names are not the fixtures" check --dry-run --no-tree "$rel"
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
expect_pass "sign-packages: ...and marks the directory .signed-locally" test -f "$rel/.signed-locally"
# release-verify in download mode must move a locally signed directory aside, never delete it; any other
# earlier download is replaced. A gh that downloads nothing and fails stands in for the network.
printf '#!/bin/sh\nexit 1\n' > "$st/gh-fail"
chmod +x "$st/gh-fail"
kw=$tmp/keepwork
mkdir -p "$kw"
cp -R "$rel" "$kw/files"
rvd() { env FIL_GH="$st/gh-fail" FIL_RELEASE_REPO_URL="$tmp/origin" "$RELEASE_DIR/verify.sh" 0.9.0 --no-rebuild --work "$kw"; }
expect_fail "release-verify (download mode, nothing to download) over locally signed files" rvd
grep -q 'were moved to .*files.signed-.*Z' "$tmp/out.log" && pass "...and says where (UTC name)" || { flunk "...and says where (UTC name)"; sed "s/^/       | /" "$tmp/out.log"; }
expect_pass "...keeps them: moved aside to files.signed-*, rpms and marker intact" sh -c 'cmp -s "$1"/files.signed-*/fil-0.9.0-1.aarch64.rpm "$2/fil-0.9.0-1.aarch64.rpm" && test -f "$1"/files.signed-*/.signed-locally' sh "$kw" "$rel"
rm -rf "$kw"
mkdir -p "$kw/files"
echo old > "$kw/files/x"
expect_fail "release-verify (download mode) over an unmarked earlier download" rvd
expect_pass "...replaces it (no files.signed-* appears, x is gone)" sh -c 'test ! -e "$1/x" && ! ls "$2"/files.signed-* > /dev/null 2>&1' sh "$kw/files" "$kw"
# A gh whose download succeeds, writing the released files (a copy of $rel; with GH_ALTER set, one rpm differs).
cat > "$st/gh-copy" << EOF2
#!/bin/sh
while [ \$# -gt 0 ]; do [ "\$1" = -D ] && d=\$2; shift; done
cp -R "$rel"/. "\$d"/
rm -f "\$d"/.signed-locally
[ -z "\$GH_ALTER" ] || echo altered >> "\$d/fil-0.9.0-1.aarch64.rpm"
exit 0
EOF2
chmod +x "$st/gh-copy"
rvc() { env FIL_GH="$st/gh-copy" FIL_RELEASE_REPO_URL="$tmp/origin" "$RELEASE_DIR/verify.sh" 0.9.0 --no-rebuild --work "$kw" "$@"; }
rm -rf "$kw"; mkdir -p "$kw"; cp -R "$rel" "$kw/files"
rvc > "$tmp/out.log" 2>&1 || true
grep -q 'identical to the download' "$tmp/out.log" && ! ls "$kw"/files.signed-* > /dev/null 2>&1 \
  && pass "download identical to the local copy: the moved-aside copy is removed" || { flunk "identical download: aside copy removed"; sed "s/^/       | /" "$tmp/out.log"; }
rm -rf "$kw"; mkdir -p "$kw"; cp -R "$rel" "$kw/files"
GH_ALTER=1 rvc > "$tmp/out.log" 2>&1 || true
grep -q 'differs: ./fil-0.9.0-1.aarch64.rpm' "$tmp/out.log" && ls "$kw"/files.signed-*/.signed-locally > /dev/null 2>&1 \
  && pass "download differing from the local copy: kept, and the differing file is named" || { flunk "differing download: kept and named"; sed "s/^/       | /" "$tmp/out.log"; }
# --reuse-rebuild: the comparison with an earlier full run's rebuild of the same commit and mode
# (WORK/rebuilt, named by WORK/rebuilt.commit), without the VM; any other stamp rebuilds (here: no VM)
rw=$tmp/reusework
rm -rf "$rw"
mkdir -p "$rw/rebuilt"
cp -R "$rel"/. "$rw/rebuilt"/
rref=$(printf 'e%.0s' {1..40})
rvr() { env FIL_VM=fil-selftest-no-such-vm "$RELEASE_DIR/verify.sh" 0.9.0 --dry-run --from-dir "$rel" --ref "$rref" --work "$rw" "$@"; }
echo "$rref dry=1" > "$rw/rebuilt.commit"
rvr --reuse-rebuild > "$tmp/out.log" 2>&1 || true
grep -q 'reusing the rebuild of' "$tmp/out.log" && grep -q 'REPRODUCED from source byte for byte' "$tmp/out.log" && ! grep -q 'Lima instance\|limactl not found' "$tmp/out.log" \
  && pass "release-verify --reuse-rebuild compares with the earlier rebuild of the same commit, without the VM" || { flunk "release-verify --reuse-rebuild reuses the earlier rebuild"; sed "s/^/       | /" "$tmp/out.log" | tail -15; }
echo "$rref dry=0" > "$rw/rebuilt.commit"
expect_fail "release-verify --reuse-rebuild: a rebuild of another mode is not reused (it rebuilds: no VM here)" rvr --reuse-rebuild
grep -q 'no rebuild of .* to reuse' "$tmp/out.log" && pass "...saying so" || { flunk "...saying so"; sed "s/^/       | /" "$tmp/out.log" | tail -5; }
rm -f "$rw/rebuilt.commit"
expect_fail "release-verify --reuse-rebuild: no stamp, no reuse" rvr --reuse-rebuild
expect_fail "release-verify: --reuse-rebuild does not go with --no-rebuild" rvr --reuse-rebuild --no-rebuild
# the stamp's life, with a limactl stub on PATH (its "VM" rebuild outputs a copy of the release;
# FAKE_REBUILD_RC fails the rebuild, FAKE_COPYOUT_RC the copy-out): a rebuild writes it, a failed
# rebuild or copy-out leaves none, a new rebuild removes the old one first, and a reuse runs no VM
mkdir -p "$rw/bin" "$rw/out"
cp -R "$rel"/. "$rw/out"/
cat > "$rw/bin/limactl" << EOF2
#!/bin/sh
echo "limactl \$*" >> "$rw/limactl.log"
case "\$*" in
  list*) echo Running ;;
  *"bash -s"*) cat > /dev/null; exit \${FAKE_REBUILD_RC:-0} ;;
  *"tar -cf"*) [ "\${FAKE_COPYOUT_RC:-0}" = 0 ] || exit 1; tar -cf - -C "$rw/out" . ;;
  *) cat > /dev/null ;;
esac
EOF2
chmod +x "$rw/bin/limactl"
rvl() { env PATH="$rw/bin:$PATH" FIL_VM=fil-stub "$RELEASE_DIR/verify.sh" 0.9.0 --dry-run --from-dir "$rel" --ref "$rref" --work "$rw" "$@"; }
rm -rf "$rw/rebuilt" "$rw/rebuilt.commit"
rvl > "$tmp/out.log" 2>&1 || true
expect_pass "release-verify: a rebuild that ran and was copied out writes the stamp" sh -c 'test "$(cat "$1/rebuilt.commit")" = "$2 dry=1" && test -f "$1/rebuilt/SHA256SUMS"' sh "$rw" "$rref"
: > "$rw/limactl.log"
rvl --reuse-rebuild > "$rw/reuse.log" 2>&1 || true
expect_pass "release-verify: ...the next run with --reuse-rebuild reuses it, with no VM call" sh -c 'grep -q "reusing the rebuild of" "$1" && test ! -s "$2"' sh "$rw/reuse.log" "$rw/limactl.log"
env FAKE_REBUILD_RC=1 PATH="$rw/bin:$PATH" FIL_VM=fil-stub "$RELEASE_DIR/verify.sh" 0.9.0 --dry-run --from-dir "$rel" --ref "$rref" --work "$rw" > "$tmp/out.log" 2>&1 || true
expect_pass "release-verify: ...a failed rebuild removes the old stamp and writes none" test ! -e "$rw/rebuilt.commit"
env PATH="$rw/bin:$PATH" FIL_VM=fil-stub "$RELEASE_DIR/verify.sh" 0.9.0 --dry-run --from-dir "$rel" --ref "$rref" --work "$rw" > /dev/null 2>&1 || true
env FAKE_COPYOUT_RC=1 PATH="$rw/bin:$PATH" FIL_VM=fil-stub "$RELEASE_DIR/verify.sh" 0.9.0 --dry-run --from-dir "$rel" --ref "$rref" --work "$rw" > "$tmp/out.log" 2>&1 || true
expect_pass "release-verify: ...so does a failed copy-out" test ! -e "$rw/rebuilt.commit"
rm -rf "$rw"
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
  "api repos/"*/deployment-branch-policies)
    case ${FAKE_POLICY:-tag} in tag) echo "tag:v*" ;; none) echo ;; branch) echo "branch:main tag:v*" ;; *) echo "HTTP 404" >&2; exit 1 ;; esac ;;
  "api repos/"*)
    case $FAKE_ENV in owner) echo "botuser anthonytw" ;; bot) echo botuser ;; none) echo ;; *) echo "HTTP 403" >&2; exit 1 ;; esac ;;
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
expect_fail "gate: an environment open to every ref is refused" g_run FAKE_ENV=owner FAKE_DRAFT=false FAKE_POLICY=none
expect_fail "gate: an environment that also deploys from a branch is refused" g_run FAKE_ENV=owner FAKE_DRAFT=false FAKE_POLICY=branch
expect_fail "gate: unreadable deployment policies are refused" g_run FAKE_ENV=owner FAKE_DRAFT=false FAKE_POLICY=missing
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
  # mutation: leave the tar mtimes alone. The tar recipe is pack_archives in common.sh, which
  # package.sh sources from its own directory, so the mutant is a copy of this directory with
  # common.sh changed (REPO_ROOT, and with it RELEASE_DIR, then points into the copy).
  mkdir -p "$tmp/mut/tests"
  cp -R "$RELEASE_DIR" "$tmp/mut/tests/release"
  sed 's/--mtime="@\$SOURCE_DATE_EPOCH" //' "$RELEASE_DIR/common.sh" > "$tmp/mut/tests/release/common.sh"
  if cmp -s "$RELEASE_DIR/common.sh" "$tmp/mut/tests/release/common.sh"; then flunk "mutation 'mtimes' did not change common.sh (the test is stale)"; fi
  PK="$tmp/mut/tests/release/package.sh" pk "$tmp/m1" > /dev/null 2>&1 || true
  sleep 2
  PK="$tmp/mut/tests/release/package.sh" pk "$tmp/m2" > /dev/null 2>&1 || true
  expect_pass "the mutant assemblies ran (a broken mutant would pass the next check vacuously)" test -s "$tmp/m1/SHA256SUMS" -a -s "$tmp/m2/SHA256SUMS"
  expect_fail "mutation: tar mtimes left alone, and the two assemblies' SHA256SUMS then differ" cmp "$tmp/m1/SHA256SUMS" "$tmp/m2/SHA256SUMS"
  expect_pass "the assembly writes the four packages too" test "$(grep -cE '\.(deb|rpm)$' "$tmp/r1/SHA256SUMS")" = 4
  # mutation: nfpm without the fixed mtime (and without SOURCE_DATE_EPOCH, which it also reads) stamps the packages with the time of the run
  sed -e '/mtime: /d' -e 's/-e SOURCE_DATE_EPOCH //' "$RELEASE_DIR/linux-packages.sh" > "$RELEASE_DIR/.pkg-mutant.sh"
  chmod +x "$RELEASE_DIR/.pkg-mutant.sh"
  trap 'rm -rf "$tmp" "$RELEASE_DIR/.pkg-mutant.sh"' EXIT INT TERM
  if cmp -s "$RELEASE_DIR/linux-packages.sh" "$RELEASE_DIR/.pkg-mutant.sh"; then flunk "mutation 'package mtime' did not change linux-packages.sh (the test is stale)"; fi
  mkdir -p "$tmp/p1" "$tmp/p2"
  env FIL_RELEASE_ROOT="$rt/tree" SOURCE_DATE_EPOCH=1700000000 "$RELEASE_DIR/.pkg-mutant.sh" --archives "$tmp/r1" --out "$tmp/p1" > /dev/null 2>&1 || true
  sleep 2
  env FIL_RELEASE_ROOT="$rt/tree" SOURCE_DATE_EPOCH=1700000000 "$RELEASE_DIR/.pkg-mutant.sh" --archives "$tmp/r1" --out "$tmp/p2" > /dev/null 2>&1 || true
  rm -f "$RELEASE_DIR/.pkg-mutant.sh"
  expect_pass "the mutant package builds ran (a broken mutant would pass the next check vacuously)" test -s "$tmp/p1/fil_0.9.0_amd64.deb" -a -s "$tmp/p2/fil_0.9.0_amd64.deb"
  expect_fail "mutation: packages without the fixed mtime, and two builds then differ" cmp "$tmp/p1/fil_0.9.0_amd64.deb" "$tmp/p2/fil_0.9.0_amd64.deb"
else
  echo "== reproducible archives: skipped (set FIL_RELEASE_TEST_DOCKER=1; needs Docker)"
fi

# ============================================================== macOS signing (sign-macos.sh, verify-macos.sh, macho_norm.py)
echo "== macOS signing"
# Synthetic fat Mach-O binaries with real signature structure (selftest_macho.py) are signed by a fake
# codesign; sign-macos.sh runs on them with fake ditto, notarytool, gh, verifier and packer (python3
# tarfile, no Docker). The keys are real (ssh-keygen). Nothing is uploaded anywhere: FIL_GH is a stub
# that copies into a scratch directory.
ms=$tmp/macsign
mkdir -p "$ms/bin"
SM="$RELEASE_DIR/selftest_macho.py"
cat > "$ms/bin/codesign" << EOF2
#!/bin/sh
exec python3 -I "$SM" codesign "\$@"
EOF2
cat > "$ms/bin/ditto" << 'EOF2'
#!/usr/bin/env python3
import sys, zipfile, os
a = [x for x in sys.argv[1:] if x not in ("-c", "-k", "-x")]
if "-c" in sys.argv:
    with zipfile.ZipFile(a[1], "w") as z:
        z.write(a[0], os.path.basename(a[0]))
else:
    with zipfile.ZipFile(a[0]) as z:
        z.extractall(a[1])
EOF2
cat > "$ms/bin/notarytool" << 'EOF2'
#!/bin/sh
echo "notarytool $*" >> "$FAKE_LOG"
case "$1" in
  log) for a; do f=$a; done; echo '{"issues": null}' > "$f" ;;
  *) printf '{"id": "sub-123", "status": "%s"}\n' "${FAKE_NOTARY_STATUS:-Accepted}" ;;
esac
EOF2
cat > "$ms/bin/gh" << 'EOF2'
#!/usr/bin/env python3
# fake gh: release view / upload / download against the scratch directory FAKE_DRAFT; attestation
# verify succeeds for the files whose sha256 is listed in FAKE_ATTESTED
import sys, os, json, shutil, hashlib
a = sys.argv[1:]
open(os.environ["FAKE_LOG"], "a").write("gh " + " ".join(a) + "\n")
draft = os.environ.get("FAKE_DRAFT", "")
sha = lambda p: hashlib.sha256(open(p, "rb").read()).hexdigest()
if a[:2] == ["release", "view"]:
    assets = []
    for n in sorted(os.listdir(draft)):
        d = {"name": n, "size": os.path.getsize(os.path.join(draft, n))}
        if not os.environ.get("FAKE_GH_NO_DIGEST"):
            d["digest"] = "sha256:" + sha(os.path.join(draft, n))
        assets.append(d)
    print(json.dumps({"isDraft": os.environ.get("FAKE_GH_DRAFT", "true") == "true", "assets": assets}))
elif a[:2] == ["release", "upload"]:
    files = [x for x in a[3:] if not x.startswith("--") and x != a[a.index("--repo") + 1]]
    n = 0
    for f in files:
        if n == int(os.environ.get("FAKE_GH_FAIL_AFTER", "-1")):
            sys.exit(1)
        shutil.copy(f, os.path.join(draft, os.path.basename(f)))
        n += 1
elif a[:2] == ["release", "download"]:
    name, dest = a[a.index("-p") + 1], a[a.index("-D") + 1]
    shutil.copy(os.path.join(draft, name), dest)
elif a[:2] == ["attestation", "verify"]:
    ok = os.path.exists(os.environ.get("FAKE_ATTESTED", "/nonexistent")) and sha(a[2]) in open(os.environ["FAKE_ATTESTED"]).read().split()
    sys.exit(0 if ok else 1)
EOF2
cat > "$ms/bin/verify" << 'EOF2'
#!/bin/sh
echo "verify $*" >> "$FAKE_LOG"
exit "${FAKE_VERIFY_EXIT:-0}"
EOF2
cat > "$ms/bin/pack" << 'EOF2'
#!/usr/bin/env python3
# fake pack_archives: the same tar.gz for the same tree, on whatever host
import sys, os, tarfile, gzip, io
stage, out = sys.argv[1:3]
sde = int(os.environ["SOURCE_DATE_EPOCH"])
for d in sorted(os.listdir(stage)):
    if not d.startswith("fil-"):
        continue
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.GNU_FORMAT) as t:
        for root, dirs, files in os.walk(os.path.join(stage, d)):
            dirs.sort()
            for n in [""] + sorted(files):
                p = os.path.join(root, n) if n else root
                ti = t.gettarinfo(p, os.path.relpath(p, stage))
                ti.mtime, ti.uid, ti.gid, ti.uname, ti.gname = sde, 0, 0, "", ""
                ti.mode = 0o755 if (ti.mode & 0o100 or ti.isdir()) else 0o644
                if ti.isdir():
                    t.addfile(ti)
                else:
                    t.addfile(ti, open(p, "rb"))
    with open(os.path.join(out, d + ".tar.gz"), "wb") as f:
        with gzip.GzipFile(fileobj=f, mode="wb", mtime=0, filename="") as g:
            g.write(buf.getvalue())
EOF2
chmod +x "$ms/bin"/*
ssh-keygen -q -t ed25519 -N '' -f "$ms/key" -C t
ssh-keygen -q -t ed25519 -N '' -f "$ms/otherkey" -C t
printf 'owner namespaces="fil-release" %s\n' "$(cut -d' ' -f1,2 "$ms/key.pub")" > "$ms/signers"
MV=0.9.0
MAC=fil-$MV-universal-apple-darwin
MACCI=$(ci_archive_name "$MV")
ID="Developer ID Application: Test (6X3PT3FXGA)"
mcs() { env FIL_CODESIGN="$ms/bin/codesign" FIL_RELEASE_PACK_CMD="$ms/bin/pack" "$@"; }

# macsign_fixture: the work directory of a verified draft: CI's macOS archive (its binary signed ad hoc,
# 4 KiB pages, as the fake of CI's codesign does), the same archive again as the .ci archive, the rest
# of the release, the verified marker, and a scratch "draft" holding the same files
macsign_fixture() {
  local w=$ms/work f
  rm -rf "$w" "$ms/stage" "$ms/packed" "$ms/draft" "$ms/files.verified"
  mkdir -p "$w/files" "$ms/stage/$MAC/bin" "$ms/stage/$MAC/share" "$ms/packed" "$ms/draft"
  python3 -I "$SM" build "$ms/stage/$MAC/bin/fil" 3 --linker
  chmod 755 "$ms/stage/$MAC/bin/fil"
  "$ms/bin/codesign" --force --sign - --identifier fil "$ms/stage/$MAC/bin/fil"
  echo readme > "$ms/stage/$MAC/share/README.md"
  SOURCE_DATE_EPOCH=1700000000 "$ms/bin/pack" "$ms/stage" "$ms/packed"
  cp "$ms/packed/$MAC.tar.gz" "$w/files/"
  cp "$ms/packed/$MAC.tar.gz" "$w/files/$MACCI"
  for f in fil-$MV-aarch64-linux-musl.tar.gz fil-$MV-source.tar.gz fil-$MV-x86_64-linux-musl.tar.gz $(package_names "$MV"); do echo "$f" > "$w/files/$f"; done
  rewrite_sums "$w/files" "$MV"
  cp "$ms/signers" "$w/signers"
  printf 'sums=%s\nrebuild=1\npresign=1\ndry=0\n' "$(sha256 "$w/files/SHA256SUMS")" > "$w/verified"
  cp "$w/files"/* "$ms/draft/"
  cp -R "$w/files" "$ms/files.verified"
  : > "$ms/log"
}
# sm [ENV=VALUE ...]: sign-macos.sh on the fixture, on the fakes
sm() {
  env FAKE_LOG="$ms/log" FAKE_DRAFT="$ms/draft" FIL_RELEASE_VERIFY="$ms/bin/verify" FIL_GH="$ms/bin/gh" FIL_NOTARYTOOL="$ms/bin/notarytool" \
    FIL_CODESIGN="$ms/bin/codesign" FIL_DITTO="$ms/bin/ditto" FIL_RELEASE_PACK_CMD="$ms/bin/pack" FIL_RELEASE_KEY="$ms/key" \
    FIL_RELEASE_SELFTEST=1 FIL_CODESIGN_IDENTITY="$ID" "$@" "${MACSIGN_SCRIPT:-$RELEASE_DIR/sign-macos.sh}" "$MV" --work "$ms/work" ${SM_ARGS:-}
}
no_upload() { ! grep -q 'release upload' "$ms/log"; }
no_notarization() { ! grep -q 'notarytool' "$ms/log"; }
no_signing() { ! grep -q 'notarytool\|release upload' "$ms/log"; }
nothing_sent() { no_notarization && no_upload; }
draft_untouched() { diff -rq "$ms/draft" "$ms/files.verified"; }
files_untouched() { diff -rq "$ms/work/files" "$ms/files.verified"; }
mac_kind() { python3 -I "$RELEASE_DIR/macho_norm.py" info "$1" | sed -n 's/.*kind=\([a-z]*\).*/\1/p' | sort -u | tr '\n' ' '; }
mac_bin() { tar -xzOf "$1" "$MAC/bin/fil" > "$2"; }

echo "-- macho_norm.py"
mn=$ms/mn
mkdir -p "$mn"
python3 -I "$SM" build "$mn/unsigned" 7
python3 -I "$SM" build "$mn/linker" 7 --linker
cp "$mn/unsigned" "$mn/adhoc4k"
"$ms/bin/codesign" --force --sign - --identifier fil --pagesize 4096 "$mn/adhoc4k"
cp "$mn/unsigned" "$mn/adhoc16k"
"$ms/bin/codesign" --force --sign - --identifier fil --pagesize 16384 "$mn/adhoc16k"
cp "$mn/linker" "$mn/devid"
"$ms/bin/codesign" --force --options runtime --timestamp --identifier fil --sign "$ID" "$mn/devid"
cp "$mn/adhoc4k" "$mn/resigned"
"$ms/bin/codesign" --force --options runtime --timestamp=none --identifier fil --sign - "$mn/resigned"
python3 -I "$SM" build "$mn/other" 8
MN() { python3 -I "$RELEASE_DIR/macho_norm.py" "$@"; }
for f in adhoc4k adhoc16k linker devid resigned; do
  expect_pass "macho_norm: $f normalizes to the same bytes as the unsigned build" MN same "$mn/unsigned" "$mn/$f"
done
expect_pass "macho_norm: the signed forms differ as files (page size, signature size)" sh -c '! cmp -s "$1" "$2" && ! cmp -s "$1" "$3"' sh "$mn/adhoc4k" "$mn/adhoc16k" "$mn/devid"
expect_fail "macho_norm: other code does not" MN same "$mn/unsigned" "$mn/other"
cp "$mn/devid" "$mn/flipped"
printf '\1' | dd of="$mn/flipped" bs=1 seek=$((0x600)) conv=notrunc 2> /dev/null
expect_fail "macho_norm: one changed byte of code is seen" MN same "$mn/unsigned" "$mn/flipped"
expect_pass "macho_norm: info reads the Developer ID signature" sh -c 'python3 -I "$1" info "$2" | grep -q "kind=signed identifier=fil flags=0x10000 team=6X3PT3FXGA entitlements=no cms=yes"' sh "$RELEASE_DIR/macho_norm.py" "$mn/devid"
expect_pass "macho_norm: info reads ad hoc and unsigned slices" sh -c 'python3 -I "$1" info "$2" | grep -q "kind=unsigned" && python3 -I "$1" info "$3" | grep -q "kind=adhoc"' sh "$RELEASE_DIR/macho_norm.py" "$mn/linker" "$mn/adhoc4k"
for k in vmsize segment ref cputype; do
  python3 -I "$SM" mutate "$mn/devid" "$k" "$mn/mut-$k"
  expect_fail "macho_norm: refuses a signed file with a bad $k" MN normalize "$mn/mut-$k" "$mn/out"
done
python3 -I "$SM" mutate "$mn/devid" vmsize "$mn/mut-vmsize2"
expect_pass "macho_norm: ...but not the unmutated file" MN normalize "$mn/devid" "$mn/out"
# refused, never passed: garbage, a truncated file, a signature that is not last, a thin 32-bit file
printf 'not a mach-o file at all, but long enough to be looked at' > "$mn/garbage"
expect_fail "macho_norm: refuses a file that is not a Mach-O" MN normalize "$mn/garbage" "$mn/out"
head -c 30000 "$mn/devid" > "$mn/truncated"
expect_fail "macho_norm: refuses a truncated file" MN normalize "$mn/truncated" "$mn/out"
{ cat "$mn/devid"; printf 'trailing'; } > "$mn/trailing"
expect_fail "macho_norm: refuses bytes after the last slice" MN normalize "$mn/trailing" "$mn/out"
printf '\316\372\355\376' > "$mn/m32"
head -c 60 /dev/zero >> "$mn/m32"
expect_fail "macho_norm: refuses a 32-bit Mach-O" MN normalize "$mn/m32" "$mn/out"

macsign_fixture
expect_pass "sign-macos: the happy path" sm
cp "$tmp/out.log" "$ms/last.log"
expect_pass "sign-macos: ...a NOTE for each test-only override" sh -c 'grep -q "NOTE: FIL_NOTARYTOOL is set" "$1" && grep -q "NOTE: FIL_CODESIGN is set" "$1"' sh "$ms/last.log"
expect_pass "sign-macos: ...the local checks were run again on the verified files, nothing downloaded" grep -q "verify .*--pre-sign --no-rebuild --from-dir $ms/work/files" "$ms/log"
expect_pass "sign-macos: ...notarized with the fil-notary profile" grep -q 'notarytool submit --wait --keychain-profile fil-notary --output-format json' "$ms/log"
expect_pass "sign-macos: ...uploaded to the draft, with --clobber, the archive, SHA256SUMS and its signature" grep -q "release upload v$MV --repo anthonytw/fil --clobber $MAC.tar.gz SHA256SUMS SHA256SUMS.sig" "$ms/log"
expect_pass "sign-macos: ...the submission id and Apple's log are kept locally" sh -c 'grep -l sub-123 "$1"/macsign-logs/notary-*.json > /dev/null && ls "$1"/macsign-logs/*.log.json > /dev/null' sh "$ms/work"
expect_pass "sign-macos: ...SHA256SUMS checks" sums_check "$ms/work/files" SHA256SUMS
expect_pass "sign-macos: ...and lists the .ci archive" grep -q "  $MACCI\$" "$ms/work/files/SHA256SUMS"
expect_pass "sign-macos: ...the .ci archive is as CI built it" cmp "$ms/work/files/$MACCI" "$ms/files.verified/$MACCI"
signed_bin_is_signed() { mac_bin "$1" "$2" && test "$(mac_kind "$2")" = "signed "; }
expect_pass "sign-macos: ...the signed archive's binary is signed with the Developer ID" signed_bin_is_signed "$ms/work/files/$MAC.tar.gz" "$ms/signed.bin"
expect_pass "sign-macos: ...the signed archive equals the .ci archive in all but the signature of bin/fil" MN archives "$ms/work/files/$MACCI" "$ms/work/files/$MAC.tar.gz"
expect_pass "sign-macos: ...SHA256SUMS.sig verifies against the signers" sh -c 'ssh-keygen -Y verify -f "$1/signers" -I owner -n fil-release -s "$1/files/SHA256SUMS.sig" < "$1/files/SHA256SUMS"' sh "$ms/work"
expect_pass "sign-macos: ...the draft got the same files" sh -c 'cmp "$1/draft/SHA256SUMS" "$1/work/files/SHA256SUMS" && cmp "$1/draft/$2" "$1/work/files/$2"' sh "$ms" "$MAC.tar.gz"
expect_pass "sign-macos: ...the marker follows the new SHA256SUMS" grep -qx "sums=$(sha256 "$ms/work/files/SHA256SUMS")" "$ms/work/verified"
cp -R "$ms/work/files" "$ms/files.signed"
: > "$ms/log"
expect_fail "sign-macos: a release this script already signed is not signed again without --force" sm
cp "$tmp/out.log" "$ms/last.log"
expect_pass "sign-macos: ...the refusal says so" grep -q 'already signed' "$ms/last.log"
SM_ARGS=--force expect_pass "sign-macos: signing again, with --force, works" sm
expect_pass "sign-macos: ...the .ci archive is still CI's" cmp "$ms/work/files/$MACCI" "$ms/files.verified/$MACCI"

echo "-- verify-macos.sh on the signed release"
VM() { env FIL_CODESIGN="$ms/bin/codesign" "$RELEASE_DIR/verify-macos.sh" "$@"; }
expect_pass "verify-macos: the signed archive agrees with the .ci archive" VM agree "$ms/files.signed" "$MV"
expect_pass "verify-macos: ...as does the unsigned release (the two are one file)" VM agree "$ms/files.verified" "$MV"
expect_pass "verify-macos: the signature report passes (team pinned)" VM report "$ms/files.signed" "$MV" --team 6X3PT3FXGA
cp "$tmp/out.log" "$ms/last.log"
expect_pass "verify-macos: ...it names the flags, the Team ID and no entitlements" sh -c 'grep -q "flags are exactly the hardened runtime" "$1" && grep -q "Team ID 6X3PT3FXGA" "$1" && grep -q "no entitlements" "$1"' sh "$ms/last.log"
expect_fail "verify-macos: the wrong Team ID is refused" VM report "$ms/files.signed" "$MV" --team ZZZZZZZZZZ
expect_fail "verify-macos: no Team ID to check against is refused" VM report "$ms/files.signed" "$MV" --team ''
expect_pass "verify-macos: --pre-sign accepts the signed release too (a re-run after signing)" VM report "$ms/files.signed" "$MV" --team 6X3PT3FXGA --pre-sign
expect_pass "verify-macos: --pre-sign accepts CI's ad hoc build" VM report "$ms/files.verified" "$MV" --team 6X3PT3FXGA --pre-sign
expect_fail "verify-macos: a final verification refuses the ad hoc build" VM report "$ms/files.verified" "$MV" --team 6X3PT3FXGA
python3 -I "$SM" build "$mn/rebuild" 3 --linker
expect_pass "verify-macos: --mac-rebuild of the same code (linker-signed) is byte-identical once normalized" VM report "$ms/files.signed" "$MV" --team 6X3PT3FXGA --mac-rebuild "$mn/rebuild"
cp "$tmp/out.log" "$ms/last.log"
expect_pass "verify-macos: ...it says so" grep -q 'BYTE-IDENTICAL' "$ms/last.log"
python3 -I "$SM" build "$mn/rebuild-other" 5 --linker
expect_fail "verify-macos: --mac-rebuild of other code is reported as different" VM report "$ms/files.signed" "$MV" --team 6X3PT3FXGA --mac-rebuild "$mn/rebuild-other"
cp "$tmp/out.log" "$ms/last.log"
expect_pass "verify-macos: ...with the slices compared" grep -q 'NOT byte-identical' "$ms/last.log"
expect_pass "verify-macos: without codesign it passes and lists what it could not check (Linux)" env FIL_CODESIGN=/nonexistent/codesign "$RELEASE_DIR/verify-macos.sh" report "$ms/files.signed" "$MV" --team 6X3PT3FXGA
cp "$tmp/out.log" "$ms/last.log"
expect_pass "verify-macos: ...the list" grep -q 'not checked here (no codesign; needs a Mac)' "$ms/last.log"
expect_fail "verify-macos: a codesign --verify --strict that fails is a failure" env FAKE_CODESIGN_VERIFYBAD=1 FIL_CODESIGN="$ms/bin/codesign" "$RELEASE_DIR/verify-macos.sh" report "$ms/files.signed" "$MV" --team 6X3PT3FXGA
# a release signed wrongly: each variant of the signed archive differs in one thing
mk_variant() { # NAME ENV=VALUE...: files.signed with bin/fil signed again under the environment
  local n=$1
  shift
  rm -rf "$ms/v-$n" "$ms/v-$n.bin"
  cp -R "$ms/files.verified" "$ms/v-$n"
  mac_bin "$ms/files.verified/$MACCI" "$ms/v-$n.bin"
  env "$@" "$ms/bin/codesign" --force --options runtime --timestamp --identifier fil --sign "$ID" "$ms/v-$n.bin"
  mcs bash -c 'source "$1/common.sh"; repack_macos_archive "$2" "$3" "$4"' sh "$RELEASE_DIR" "$ms/files.verified/$MACCI" "$ms/v-$n.bin" "$ms/v-$n/$MAC.tar.gz"
}
mk_variant ents FAKE_CODESIGN_ENTITLEMENTS=1
expect_fail "verify-macos: a signature with entitlements is refused" VM report "$ms/v-ents" "$MV" --team 6X3PT3FXGA
mk_variant noruntime FAKE_CODESIGN_NORUNTIME=1
expect_fail "verify-macos: a signature without exactly the hardened runtime flag is refused" VM report "$ms/v-noruntime" "$MV" --team 6X3PT3FXGA
mk_variant team FAKE_CODESIGN_TEAM=ABCDE12345
expect_fail "verify-macos: a signature by another team is refused" VM report "$ms/v-team" "$MV" --team 6X3PT3FXGA
mk_variant certteam FAKE_CODESIGN_CERT_TEAM=ABCDE12345
expect_fail "verify-macos: a CodeDirectory Team ID that is not the certificate's is refused (the designated requirement)" VM report "$ms/v-certteam" "$MV" --team 6X3PT3FXGA
cp "$tmp/out.log" "$ms/last.log"
expect_pass "verify-macos: ...and the message says which requirement failed" grep -q "Developer ID Application certificate whose OU is 6X3PT3FXGA" "$ms/last.log"
expect_pass "verify-macos: the designated requirement is checked on the good release" sh -c 'env FIL_CODESIGN="$1/bin/codesign" "$2/verify-macos.sh" report "$3" "$4" --team 6X3PT3FXGA 2>&1 | grep -q "the designated requirement holds"' sh "$ms" "$RELEASE_DIR" "$ms/files.signed" "$MV"
mk_variant tamper FAKE_CODESIGN_TAMPER=1
expect_fail "verify-macos: a signed binary that differs from CI's by more than its signature is refused (agree)" VM agree "$ms/v-tamper" "$MV"
# the archive differs in a member other than bin/fil
rm -rf "$ms/v-member" && cp -R "$ms/files.signed" "$ms/v-member"
mkdir "$ms/v-member.x" && tar -xzpf "$ms/v-member/$MAC.tar.gz" -C "$ms/v-member.x" && echo changed > "$ms/v-member.x/$MAC/share/README.md"
SOURCE_DATE_EPOCH=1700000000 "$ms/bin/pack" "$ms/v-member.x" "$ms/v-member.x" && cp "$ms/v-member.x/$MAC.tar.gz" "$ms/v-member/$MAC.tar.gz"
expect_fail "verify-macos: a signed archive that differs in another member is refused (agree)" VM agree "$ms/v-member" "$MV"
for k in trailing pax; do
  rm -rf "$ms/v-$k" && cp -R "$ms/files.signed" "$ms/v-$k"
  python3 -I "$SM" tarmutate "$ms/files.signed/$MAC.tar.gz" "$k" "$ms/v-$k/$MAC.tar.gz"
  expect_fail "verify-macos: a signed archive with $k tar data the CI archive lacks is refused (agree)" VM agree "$ms/v-$k" "$MV"
done
# a missing .ci archive
rm -rf "$ms/v-noci" && cp -R "$ms/files.signed" "$ms/v-noci" && rm "$ms/v-noci/$MACCI"
expect_fail "verify-macos: a release without the .ci archive is refused (agree)" VM agree "$ms/v-noci" "$MV"

echo "-- sign-macos: refusals"
macsign_fixture
expect_fail "sign-macos: a failed verification stops everything" sm FAKE_VERIFY_EXIT=1
expect_pass "sign-macos: ...nothing was notarized or uploaded" nothing_sent
macsign_fixture
rm "$ms/work/verified"
expect_fail "sign-macos: files that were not verified in full are refused" sm
expect_pass "sign-macos: ...nothing was verified, signed or uploaded" sh -c '! grep -q "verify \|notarytool\|release upload" "$1"' sh "$ms/log"
macsign_fixture
echo tampered >> "$ms/work/files/SHA256SUMS"
expect_fail "sign-macos: files that changed since they were verified are refused" sm
macsign_fixture
expect_fail "sign-macos: a release that is not a draft is refused" sm FAKE_GH_DRAFT=false
expect_pass "sign-macos: ...nothing was notarized or uploaded" nothing_sent
macsign_fixture
echo "replaced" > "$ms/draft/fil-$MV-x86_64-linux-musl.tar.gz"
expect_fail "sign-macos: a draft whose Linux archive was replaced since the verification is refused" sm
expect_pass "sign-macos: ...nothing was signed or uploaded" nothing_sent
macsign_fixture
rm "$ms/draft/fil-$MV-source.tar.gz"
expect_fail "sign-macos: a draft that lost an asset is refused" sm
macsign_fixture
echo x > "$ms/draft/fil-$MV-source.tar.gz"
expect_fail "sign-macos: a drifted draft is caught when gh reports no digests too (the assets are downloaded)" sm FAKE_GH_NO_DIGEST=1
macsign_fixture
expect_pass "sign-macos: a gh that reports no digests still works (the assets are downloaded and hashed)" sm FAKE_GH_NO_DIGEST=1
expect_pass "sign-macos: ...by downloading them" grep -q 'release download' "$ms/log"
macsign_fixture
expect_fail "sign-macos: a faked step outside the self-test is refused (no real signing and upload with a fake)" sm FIL_RELEASE_SELFTEST=
cp "$tmp/out.log" "$ms/last.log"
expect_pass "sign-macos: ...says why, and nothing ran" sh -c 'grep -q "with a faked step is refused" "$1" && ! test -s "$2"' sh "$ms/last.log" "$ms/log"
macsign_fixture
expect_fail "sign-macos: a notarytool override alone is refused too" env -u FIL_GH FIL_NOTARYTOOL="$ms/bin/notarytool" FIL_RELEASE_KEY="$ms/key" "$RELEASE_DIR/sign-macos.sh" "$MV" --work "$ms/work"
expect_fail "sign-macos: nor can FIL_GH point at the real gh and fake the rest" env FIL_GH=gh FIL_NOTARYTOOL="$ms/bin/notarytool" FIL_RELEASE_KEY="$ms/key" "$RELEASE_DIR/sign-macos.sh" "$MV" --work "$ms/work"
macsign_fixture
expect_fail "sign-macos: the signature on the wrong file is caught" sm FAKE_CODESIGN_WRONGFILE=1
expect_pass "sign-macos: ...nothing was notarized or uploaded" nothing_sent
macsign_fixture
expect_fail "sign-macos: an identity that was not applied is caught" sm FAKE_CODESIGN_IGNORE_IDENTITY=1
expect_pass "sign-macos: ...nothing was notarized or uploaded" nothing_sent
macsign_fixture
expect_fail "sign-macos: no hardened runtime is caught" sm FAKE_CODESIGN_NORUNTIME=1
expect_pass "sign-macos: ...nothing was notarized or uploaded" nothing_sent
macsign_fixture
expect_fail "sign-macos: entitlements are caught" sm FAKE_CODESIGN_ENTITLEMENTS=1
macsign_fixture
expect_fail "sign-macos: a certificate of another team is caught" sm FAKE_CODESIGN_TEAM=ABCDE12345
macsign_fixture
expect_fail "sign-macos: a CodeDirectory Team ID that is not the certificate's is caught before the notarization" sm FAKE_CODESIGN_CERT_TEAM=ABCDE12345
expect_pass "sign-macos: ...nothing was notarized or uploaded" nothing_sent
macsign_fixture
expect_fail "sign-macos: a signature that does not verify is caught" sm FAKE_CODESIGN_VERIFYBAD=1
macsign_fixture
expect_fail "sign-macos: signing that changes more than the signature is caught BEFORE the notarization" sm FAKE_CODESIGN_TAMPER=1
expect_pass "sign-macos: ...nothing was notarized or uploaded" nothing_sent
macsign_fixture
expect_fail "sign-macos: notarization rejected (Invalid, with exit status 0) uploads nothing" sm FAKE_NOTARY_STATUS=Invalid
expect_pass "sign-macos: ...nothing was uploaded" no_upload
expect_pass "sign-macos: ...the draft is as it was" draft_untouched
expect_pass "sign-macos: ...and so are the verified files" files_untouched
expect_pass "sign-macos: ...the rejection's submission id is kept locally" sh -c 'grep -l sub-123 "$1"/macsign-logs/notary-*.json > /dev/null' sh "$ms/work"
macsign_fixture
expect_fail "sign-macos: a SHA256SUMS signature that does not verify against the signers uploads nothing" sm FIL_RELEASE_KEY="$ms/otherkey"
expect_pass "sign-macos: ...nothing was uploaded" no_upload
macsign_fixture
expect_fail "sign-macos: a packing that is not CI's stops before signing" sm FIL_RELEASE_PACK_CMD="$ms/bin/verify"
expect_pass "sign-macos: ...nothing was notarized or uploaded" nothing_sent

echo "-- sign-macos: recovery from a half-finished upload"
macsign_fixture
expect_fail "sign-macos: an upload that stops after the first file fails" sm FAKE_GH_FAIL_AFTER=1
cp "$tmp/out.log" "$ms/last.log"
expect_pass "sign-macos: ...the draft now holds the signed archive and the old SHA256SUMS" sh -c 'cmp "$1/draft/SHA256SUMS" "$1/files.verified/SHA256SUMS" && ! cmp -s "$1/draft/$2" "$1/files.verified/$2"' sh "$ms" "$MAC.tar.gz"
expect_pass "sign-macos: ...the verified files were not touched" files_untouched
expect_pass "sign-macos: ...and it says to run it again" grep -q 'run .tests/release/just.sh release-sign-macos 0.9.0. again' "$ms/last.log"
expect_pass "sign-macos: a rerun, from the draft alone, completes the signing" sm
expect_pass "sign-macos: ...the draft has the new SHA256SUMS and its signature" sh -c 'cmp "$1/draft/SHA256SUMS" "$1/work/files/SHA256SUMS" && test -s "$1/draft/SHA256SUMS.sig"' sh "$ms"
expect_pass "sign-macos: ...the signed release verifies (agree)" VM agree "$ms/work/files" "$MV"
macsign_fixture
expect_fail "sign-macos: an upload that stops before the first file fails" sm FAKE_GH_FAIL_AFTER=0
expect_pass "sign-macos: a rerun after it completes" sm

# mutation: SHA256SUMS is not rebuilt
MACSIGN_SCRIPT=$RELEASE_DIR/.macsign-mutant.sh
sed '/^rewrite_sums /d' "$RELEASE_DIR/sign-macos.sh" > "$MACSIGN_SCRIPT"
chmod +x "$MACSIGN_SCRIPT"
trap 'rm -rf "$tmp" "$RELEASE_DIR/.pkg-mutant.sh" "$RELEASE_DIR/.macsign-mutant.sh"' EXIT INT TERM
if cmp -s "$RELEASE_DIR/sign-macos.sh" "$MACSIGN_SCRIPT"; then flunk "mutation 'SHA256SUMS not rebuilt' did not change sign-macos.sh (the test is stale)"; else
  macsign_fixture
  expect_fail "mutation: SHA256SUMS not rebuilt is caught before anything is uploaded" sm
  expect_pass "mutation: ...nothing was uploaded" no_upload
fi
rm -f "$MACSIGN_SCRIPT"
unset MACSIGN_SCRIPT


# ============================================================== release keys (release-keys.sh)
# A real ssh-agent (started here, killed by its pid), real ssh-keygen keys and, when gpg is
# installed, real gpg in a throwaway GNUPGHOME; the Apple tools are fakes. Never the caller's
# agent or keyring: SSH_AUTH_SOCK and GNUPGHOME are always this block's own.
echo "== release keys"
rk=$tmp/rk
mkdir -p "$rk/bin"
RK_SCRIPT=$RELEASE_DIR/release-keys.sh
rk_agent_pid='' rk_gpg=0 rk_sock=$rk/none.sock
rk_cleanup() {
  [ -z "$rk_agent_pid" ] || kill "$rk_agent_pid" 2> /dev/null || true
  local p
  for p in $(cat "$rk"/cache/fil/release-keys/* 2> /dev/null | sed -n 's/^pid=//p'); do
    if ps -p "$p" -o command= 2> /dev/null | grep -q fil-release-keys-reaper; then kill "$p" 2> /dev/null || true; fi
  done
  [ "$rk_gpg" = 0 ] || GNUPGHOME=$rk/gnupg gpgconf --kill all 2> /dev/null || true
}
trap 'rk_cleanup; rm -rf "$tmp"' EXIT INT TERM
# rks ENV=VALUE... -- ARGS...: release-keys.sh in this block's world (its own agent, keyring, state).
rks() {
  local envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done
  shift
  env FIL_RELEASE_SELFTEST=1 SSH_AUTH_SOCK="$rk_sock" GNUPGHOME="$rk/gnupg" XDG_CACHE_HOME="$rk/cache" FIL_RELEASE_KEY="$rk/fil-release" \
    FIL_SSH_ADD="$rk/bin/ssh-add" FIL_PACKAGE_KEY_FILE="$rk/package-key.asc" ${envs[@]+"${envs[@]}"} \
    ${RK_SHELL:-} "$RK_SCRIPT" "$@" 2>&1 | tee "$rk/last.log"
}
# A logging ssh-add (the real one underneath); RK_NO_T=1 makes it an ssh-add without -T.
cat > "$rk/bin/ssh-add" << EOF
#!/bin/sh
echo "ssh-add \$*" >> "$rk/ssh-add.log"
if [ "\${RK_NO_T:-}" = 1 ]; then for a; do [ "\$a" = -T ] && { echo "ssh-add: illegal option -- T" >&2; exit 1; }; done; fi
exec ssh-add "\$@"
EOF
chmod +x "$rk/bin/ssh-add"

expect_fail "release-keys: a test-only override outside the self-test is refused" env FIL_SSH_ADD="$rk/bin/ssh-add" FIL_RELEASE_SELFTEST= "$RK_SCRIPT" status --need ssh
expect_fail "release-keys: an unknown key is refused" env FIL_RELEASE_SELFTEST=1 "$RK_SCRIPT" status --need ssh,pgp
for t in 0 25h 8x h8 8hh "" 123456 s 1d; do
  expect_fail "release-keys: --ttl '$t' is refused" env FIL_RELEASE_SELFTEST=1 "$RK_SCRIPT" status --need ssh --ttl "$t"
done

if command -v ssh-agent > /dev/null && command -v ssh-add > /dev/null; then
  rk_sock=$rk/agent.sock
  eval "$(ssh-agent -s -a "$rk_sock")" > /dev/null
  rk_agent_pid=$SSH_AGENT_PID
  unset SSH_AGENT_PID
  ssh-keygen -q -t ed25519 -N '' -C fil-release-test -f "$rk/fil-release"
  : > "$rk/ssh-add.log"
  expect_fail "release-keys ssh: status before unlock says locked (exit 1)" rks -- status --need ssh
  expect_pass "release-keys ssh: ...and names it locked" grep -q '^ssh    locked' "$rk/last.log"
  expect_pass "release-keys ssh: unlock adds the key" rks -- unlock --need ssh
  expect_pass "release-keys ssh: ...with the default 8h lifetime" grep -qx "ssh-add -t 28800 $rk/fil-release" "$rk/ssh-add.log"
  expect_pass "release-keys ssh: ...and the real agent signs with it (ssh-add -T)" env SSH_AUTH_SOCK="$rk_sock" ssh-add -T "$rk/fil-release.pub"
  expect_pass "release-keys ssh: status says ready" rks -- status --need ssh
  expect_pass "release-keys ssh: ...with the key's fingerprint" grep -q "^ssh    ready .*$(ssh-keygen -l -f "$rk/fil-release.pub" | cut -d' ' -f2)" "$rk/last.log"
  expect_pass "release-keys ssh: a second unlock asks nothing" rks -- unlock --need ssh
  expect_pass "release-keys ssh: ...no second ssh-add of the key" test "$(grep -c '^ssh-add -t' "$rk/ssh-add.log")" = 1
  expect_pass "release-keys ssh: an ssh-add without -T: a test signature proves the key instead" rks RK_NO_T=1 -- status --need ssh
  expect_pass "release-keys ssh: ...it did fall back" grep -q '^ssh    ready' "$rk/last.log"
  expect_pass "release-keys ssh: forget removes the key" rks -- forget --need ssh
  expect_fail "release-keys ssh: ...the real agent no longer has it" env SSH_AUTH_SOCK="$rk_sock" ssh-add -T "$rk/fil-release.pub"
  expect_fail "release-keys ssh: ...status says locked" rks -- status --need ssh
  expect_fail "release-keys ssh: ...and so does the fallback (the private file next to the .pub is not used)" rks RK_NO_T=1 -- status --need ssh
  expect_pass "release-keys ssh: --ttl 90m is 5400 seconds" rks -- unlock --need ssh --ttl 90m
  expect_pass "release-keys ssh: ...passed to ssh-add" grep -qx "ssh-add -t 5400 $rk/fil-release" "$rk/ssh-add.log"
  rks -- forget --need ssh > /dev/null 2>&1
  expect_pass "release-keys ssh: --ttl 010h is ten hours, in base 10" rks -- unlock --need ssh --ttl 010h
  expect_pass "release-keys ssh: ...passed to ssh-add" grep -qx "ssh-add -t 36000 $rk/fil-release" "$rk/ssh-add.log"
  expect_pass "release-keys ssh: a key already in the agent: its lifetime cannot be read" rks -- unlock --need ssh
  expect_pass "release-keys ssh: ...and the message says so" grep -q 'ssh-add cannot show it' "$rk/last.log"
  rks -- forget --need ssh > /dev/null 2>&1
  expect_fail "release-keys ssh: a missing key file is refused" rks FIL_RELEASE_KEY="$rk/nokey" -- unlock --need ssh
  expect_fail "release-keys ssh: no agent is an error" rks SSH_AUTH_SOCK="$rk/none.sock" -- unlock --need ssh
  expect_pass "release-keys ssh: ...that says so" grep -q 'no ssh-agent' "$rk/last.log"
else
  echo "  skip release-keys ssh: no ssh-agent"
fi

rk_preset=''
if command -v gpg > /dev/null && command -v gpgconf > /dev/null && command -v gpg-connect-agent > /dev/null; then
  rk_preset=$(gpgconf --list-dirs libexecdir 2> /dev/null)/gpg-preset-passphrase
  [ -x "$rk_preset" ] || rk_preset=''
fi
if [ -n "$rk_preset" ]; then
  rk_gpg=1
  rk_sock=${rk_sock:-$rk/none.sock}
  export_gnupg() { GNUPGHOME=$rk/gnupg LC_ALL=C "$@"; }
  mkdir -m 700 "$rk/gnupg"
  RK_PASS='correct horse rk-test-passphrase'
  export_gnupg gpg --batch --pinentry-mode loopback --passphrase "$RK_PASS" --quick-generate-key "fil test package key" ed25519 sign 1y > /dev/null 2>&1
  rk_fpr=$(export_gnupg gpg -K --with-colons 2> /dev/null | awk -F: '$1 == "fpr" { print $10; exit }')
  export_gnupg gpg --armor --export "$rk_fpr" > "$rk/package-key.asc" 2> /dev/null
  # A logging gpg-preset-passphrase (the real one underneath): it records its arguments, never stdin.
  cat > "$rk/bin/preset" << EOF
#!/bin/sh
echo "preset \$*" >> "$rk/preset.log"
exec "$rk_preset" "\$@"
EOF
  chmod +x "$rk/bin/preset"
  rkg() { rks FIL_GPG_PRESET="$rk/bin/preset" "$@"; }
  rk_sign() { rm -f "$rk/m.sig" && echo m > "$rk/m" && export_gnupg gpg --batch --pinentry-mode error -u "$rk_fpr" --detach-sign -o "$rk/m.sig" "$rk/m"; }
  rk_reaper() { cat "$rk"/cache/fil/release-keys/* 2> /dev/null | sed -n 's/^pid=//p'; }
  rk_alive() { [ -n "$1" ] && kill -0 "$1" 2> /dev/null; }

  printf '%s\n' "$RK_PASS" | rkg -- unlock --need gpg > "$tmp/out.log" 2>&1 && flunk "release-keys gpg: without allow-preset-passphrase, unlock stops (it passed)" || pass "release-keys gpg: without allow-preset-passphrase, unlock stops"
  expect_pass "release-keys gpg: ...and prints the one-time setup" grep -q 'echo allow-preset-passphrase >>' "$rk/last.log"
  expect_pass "release-keys gpg: ...before any preset" test ! -s "$rk/preset.log"
  echo allow-preset-passphrase > "$rk/gnupg/gpg-agent.conf"
  export_gnupg gpgconf --reload gpg-agent 2> /dev/null || true

  expect_fail "release-keys gpg: status before unlock says locked" rkg -- status --need gpg
  printf 'wrong passphrase\n' | rkg -- unlock --need gpg > "$tmp/out.log" 2>&1 && flunk "release-keys gpg: a wrong passphrase fails (it passed)" || pass "release-keys gpg: a wrong passphrase fails at the test signature"
  expect_pass "release-keys gpg: ...and says so" grep -q 'wrong passphrase' "$rk/last.log"
  expect_fail "release-keys gpg: ...and leaves nothing cached" rkg -- status --need gpg
  expect_pass "release-keys gpg: ...the agent says locked (not NO-REAPER)" grep -q '^gpg    locked' "$rk/last.log"
  expect_pass "release-keys gpg: ...and no reaper" test -z "$(rk_reaper)"

  : > "$rk/preset.log"
  printf '%s\n' "$RK_PASS" | rkg -- unlock --need gpg > "$rk/unlock.log" 2>&1 && pass "release-keys gpg: unlock presets the passphrase" || { flunk "release-keys gpg: unlock presets the passphrase"; sed 's/^/       | /' "$rk/unlock.log"; }
  expect_pass "release-keys gpg: ...a signature now needs no pinentry" rk_sign
  rk_pid=$(rk_reaper)
  expect_pass "release-keys gpg: ...the reaper runs" rk_alive "$rk_pid"
  expect_pass "release-keys gpg: status says ready, with the time left and the reaper" rkg -- status --need gpg
  expect_pass "release-keys gpg: ...in its line" grep -q "^gpg    ready      the package key $rk_fpr is cached, forgotten in [78]h[0-9][0-9]m (reaper pid $rk_pid)" "$rk/last.log"
  expect_pass "release-keys gpg: the passphrase is in no argument list" sh -c '! grep -qF "$1" "$2"' sh "$RK_PASS" "$rk/preset.log"
  expect_pass "release-keys gpg: ...the preset tool was called (with the keygrip only)" grep -q '^preset --preset [0-9A-F]\{40\}$' "$rk/preset.log"
  expect_pass "release-keys gpg: ...nor in the reaper record or the output" sh -c '! grep -rqF "$1" "$2" "$3"' sh "$RK_PASS" "$rk/cache" "$rk/unlock.log"
  : > "$rk/preset.log"
  expect_pass "release-keys gpg: a second unlock within the TTL asks nothing" rkg -- unlock --need gpg < /dev/null
  expect_pass "release-keys gpg: ...presets nothing" test ! -s "$rk/preset.log"
  expect_pass "release-keys gpg: ...and keeps the reaper" test "$(rk_reaper)" = "$rk_pid"
  expect_pass "release-keys gpg: forget" rkg -- forget --need gpg
  expect_fail "release-keys gpg: ...the reaper is gone" rk_alive "$rk_pid"
  expect_fail "release-keys gpg: ...a signature fails without a pinentry" rk_sign
  expect_fail "release-keys gpg: ...status says locked" rkg -- status --need gpg
  expect_pass "release-keys gpg: ...and no reaper record is left" test -z "$(rk_reaper)"

  # xtrace must never print the passphrase: SHELLOPTS=xtrace and bash -x.
  for mode in shellopts bash-x; do
    if [ $mode = shellopts ]; then rkx() { rkg SHELLOPTS=xtrace "$@"; }; else rkx() { RK_SHELL="bash -x" rkg "$@"; }; fi
    printf '%s\n' "$RK_PASS" | rkx -- unlock --need gpg > "$rk/xtrace.log" 2>&1 && pass "release-keys gpg: unlock under xtrace ($mode)" || { flunk "release-keys gpg: unlock under xtrace ($mode)"; tail -5 "$rk/xtrace.log" | sed 's/^/       | /'; }
    expect_pass "release-keys gpg: ...it did trace ($mode)" grep -q '^+' "$rk/xtrace.log"
    expect_pass "release-keys gpg: ...and the passphrase is not in the trace ($mode)" sh -c '! grep -qF "$1" "$2"' sh "$RK_PASS" "$rk/xtrace.log"
    rkg -- forget --need gpg > /dev/null 2>&1
  done

  # One reaper per keygrip: a preset whose reaper is gone is reported as such, and a new unlock
  # leaves exactly one reaper.
  printf '%s\n' "$RK_PASS" | rkg -- unlock --need gpg > /dev/null 2>&1
  rk_pid=$(rk_reaper)
  kill "$rk_pid" 2> /dev/null || true
  i=0
  while kill -0 "$rk_pid" 2> /dev/null && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
  expect_fail "release-keys gpg: a cached passphrase with no reaper is not ready" rkg -- status --need gpg
  expect_pass "release-keys gpg: ...and says it does not expire" grep -q '^gpg    NO-REAPER .*does not expire' "$rk/last.log"
  printf '%s\n' "$RK_PASS" | rkg -- unlock --need gpg > /dev/null 2>&1
  rk_pid=$(rk_reaper)
  expect_pass "release-keys gpg: ...a new unlock starts one reaper" test "$(printf '%s\n' "$rk_pid" | wc -l | tr -d ' ')" = 1
  rkg -- unlock --need gpg < /dev/null > /dev/null 2>&1
  expect_pass "release-keys gpg: ...unlock again: still that one" test "$(rk_reaper)" = "$rk_pid"
  expect_pass "release-keys gpg: ...and status is ready" rkg -- status --need gpg
  expect_pass "release-keys gpg: the record directory is 700" sh -c 'ls -ld "$1" | grep -q "^drwx------"' sh "$rk/cache/fil/release-keys"
  expect_pass "release-keys gpg: ...and the record 600" sh -c 'for f in "$1"/*; do ls -l "$f" | grep -q "^-rw-------" || exit 1; done' sh "$rk/cache/fil/release-keys"
  # A garbled record: status says CORRUPT, with no bash error.
  rk_rec=$(ls "$rk"/cache/fil/release-keys/* | head -n 1)
  cp "$rk_rec" "$rk/rec.saved"
  printf 'pid=x\ndeadline=a[$(touch %s)]\n' "$rk/pwned" > "$rk_rec"
  expect_fail "release-keys gpg: a garbled reaper record fails status" rkg -- status --need gpg
  expect_pass "release-keys gpg: ...as CORRUPT" grep -q '^gpg    CORRUPT' "$rk/last.log"
  expect_pass "release-keys gpg: ...with no shell error and nothing run" sh -c '! grep -q "unbound variable\|syntax error\|line [0-9]" "$1" && test ! -e "$2"' sh "$rk/last.log" "$rk/pwned"
  cp "$rk/rec.saved" "$rk_rec"
  # Two unlocks at once: they serialize, and one reaper is left.
  ( printf '%s\n' "$RK_PASS" | rkg -- forget --need gpg > /dev/null 2>&1 )
  printf '%s\n' "$RK_PASS" | rkg -- unlock --need gpg --ttl 5m > /dev/null 2>&1 &
  rk_u1=$!
  printf '%s\n' "$RK_PASS" | rkg -- unlock --need gpg --ttl 6m > /dev/null 2>&1 &
  rk_u2=$!
  wait "$rk_u1" || true
  wait "$rk_u2" || true
  expect_pass "release-keys gpg: two concurrent unlocks leave one live reaper" sh -c 'n=0; for p in $(ps -A -ww -o pid= -o command= | grep "[f]il-release-keys-reaper" | grep -F "$1" | awk "{print \$1}"); do n=$((n + 1)); done; test "$n" = 1' sh "$rk/cache"
  expect_pass "release-keys gpg: ...and no lock is left behind" sh -c '! ls -d "$1"/*.lock > /dev/null 2>&1' sh "$rk/cache/fil/release-keys"
  rkg -- forget --need gpg > /dev/null 2>&1

  # A TERM while the test signature runs: nothing stays cached (a slowed gpg).
  cat > "$rk/bin/slowgpg" << EOF
#!/bin/sh
for a; do [ "\$a" = --detach-sign ] && { echo \$\$ > "$rk/signing"; sleep 5; }; done
exec gpg "\$@"
EOF
  chmod +x "$rk/bin/slowgpg"
  rm -f "$rk/signing"
  printf '%s\n' "$RK_PASS" | env FIL_RELEASE_SELFTEST=1 GNUPGHOME="$rk/gnupg" XDG_CACHE_HOME="$rk/cache" FIL_GPG="$rk/bin/slowgpg" FIL_PACKAGE_KEY_FILE="$rk/package-key.asc" "$RK_SCRIPT" unlock --need gpg > /dev/null 2>&1 &
  rk_bg=$!
  i=0
  while [ ! -f "$rk/signing" ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
  kill -TERM "$rk_bg" 2> /dev/null || true
  wait "$rk_bg" 2> /dev/null || true
  rk_slow=$(cat "$rk/signing" 2> /dev/null || true)
  if [ -n "$rk_slow" ] && ps -p "$rk_slow" -o command= 2> /dev/null | grep -q slowgpg; then kill "$rk_slow" 2> /dev/null || true; fi
  expect_pass "release-keys gpg: a TERM during the test signature..." test -f "$rk/signing"
  expect_fail "release-keys gpg: ...leaves nothing cached" rkg -- status --need gpg
  expect_pass "release-keys gpg: ...the agent says locked (not NO-REAPER)" grep -q '^gpg    locked' "$rk/last.log"
  expect_pass "release-keys gpg: ...and no reaper" test -z "$(rk_reaper)"

  # The reaper itself: a 1-second TTL, then the passphrase is gone with no forget (bounded wait).
  printf '%s\n' "$RK_PASS" | rkg -- unlock --need gpg --ttl 1s > "$tmp/out.log" 2>&1 && pass "release-keys gpg: unlock with --ttl 1s" || flunk "release-keys gpg: unlock with --ttl 1s"
  rk_pid=$(rk_reaper)
  i=0
  while rk_alive "$rk_pid" && [ $i -lt 60 ]; do sleep 0.5; i=$((i + 1)); done
  expect_fail "release-keys gpg: ...the reaper finished" rk_alive "$rk_pid"
  expect_fail "release-keys gpg: ...and forgot the passphrase" rk_sign

  expect_fail "release-keys gpg: a key that is not in the keyring is refused" rkg FIL_PACKAGE_KEY_FILE="$REPO_ROOT/.github/package-signing-key.asc" -- unlock --need gpg < /dev/null
  expect_pass "release-keys gpg: ...saying so" grep -q 'is not a secret key in this keyring' "$rk/last.log"
else
  echo "  skip release-keys gpg: no gpg with gpg-preset-passphrase"
fi

# Apple: fakes for security, notarytool and codesign, which log their arguments.
cat > "$rk/bin/security" << EOF
#!/bin/sh
echo "security \$*" >> "$rk/apple.log"
[ "\$1" = find-identity ] && [ "\${RK_NO_ID:-}" != 1 ] && echo '  1) ABCDEF "Developer ID Application: Test (TEAM)"' && echo '     1 valid identities found'
exit 0
EOF
cat > "$rk/bin/notarytool" << EOF
#!/bin/sh
echo "notarytool \$*" >> "$rk/apple.log"
[ "\${RK_NOTARY_FAIL:-}" != 1 ]
EOF
cat > "$rk/bin/codesign" << EOF
#!/bin/sh
echo "codesign \$*" >> "$rk/apple.log"
exit 0
EOF
chmod +x "$rk/bin/security" "$rk/bin/notarytool" "$rk/bin/codesign"
rka() { rks FIL_SECURITY="$rk/bin/security" FIL_NOTARYTOOL="$rk/bin/notarytool" FIL_CODESIGN="$rk/bin/codesign" FIL_CODESIGN_IDENTITY="Developer ID Application: Test (TEAM)" "$@"; }
: > "$rk/apple.log"
expect_pass "release-keys apple: identity, notary profile and a test codesign" rka SSH_CONNECTION= -- unlock --need apple
expect_pass "release-keys apple: ...the profile was checked" grep -q '^notarytool history --keychain-profile fil-notary$' "$rk/apple.log"
expect_pass "release-keys apple: ...the test codesign is hardened and timestamped, of a scratch copy" sh -c 'grep "^codesign --force --sign" "$1" | grep -- "--timestamp --options runtime" | grep -q "/fil-keycheck$"' sh "$rk/apple.log"
expect_pass "release-keys apple: status" rka -- status --need apple
expect_fail "release-keys apple: a missing identity is refused" rka RK_NO_ID=1 -- unlock --need apple
expect_fail "release-keys apple: a notary profile that fails is refused" rka RK_NOTARY_FAIL=1 -- unlock --need apple
expect_pass "release-keys apple: ...saying which" grep -q "profile 'fil-notary' does not work" "$rk/last.log"

rk_cleanup
rk_agent_pid='' rk_gpg=0 rk_sock=$rk/none.sock
trap 'rm -rf "$tmp"' EXIT INT TERM

# ============================================================== 9. release-preflight.sh (stubs only)
# A clone with a Gitea-like `origin` and a GitHub-like `github` (local bare repos), real git, real
# ssh-keygen keys and real commit signing; every other tool is a stub driven by FAKE_* variables.
# Nothing here reaches GitHub, Gitea, crates.io or Docker. Each failure class must fail with its message.
echo "== release-preflight.sh"
pf=$tmp/preflight
PF=$RELEASE_DIR/release-preflight.sh
mkdir -p "$pf/bin" "$pf/path"
ssh-keygen -q -t ed25519 -N '' -f "$pf/key" -C t
ssh-keygen -q -t ed25519 -N '' -f "$pf/otherkey" -C t
git init -q --bare "$pf/origin.git"
git init -q --bare "$pf/github.git"
git init -q -b main "$pf/clone"
pg() { git -C "$pf/clone" "$@"; }
# GitHub's main starts one commit behind the release commit (as after the agents' last fast-forward
# of it): the history is `base`, then the release commit `r`
echo base > "$pf/clone/README"
pg add README
pg commit -q --no-gpg-sign -m base
PF_BASE=$(pg rev-parse HEAD)
mkdir -p "$pf/clone/.github" "$pf/clone/tests/terminals/exporter"
printf '[workspace.package]\nversion = "0.9.0"\n\n[workspace.dependencies]\nfil-a = { path = "a", version = "=0.9.0" }\nfil-b = { path = "b", version = "=0.9.0" }\n' > "$pf/clone/Cargo.toml"
printf '# Changelog\n\n## [Unreleased]\n\n## [0.9.0] - 2026-11-09\n\n- x\n' > "$pf/clone/CHANGELOG.md"
printf 'release namespaces="git" %s\nrelease namespaces="fil-release" %s\n' "$(cut -d' ' -f1,2 "$pf/key.pub")" "$(cut -d' ' -f1,2 "$pf/key.pub")" > "$pf/clone/.github/release-signers"
echo 'fake key' > "$pf/clone/.github/package-signing-key.asc"
printf '[target.x86_64-unknown-linux-musl]\nimage = "example.invalid/cross@sha256:%064d"\n' 0 > "$pf/clone/Cross.toml"
echo '[package]' > "$pf/clone/tests/terminals/exporter/Cargo.toml"
# the recipes check reads the real release.just and release scripts (with the real just, below)
mkdir -p "$pf/clone/tests"
cp -R "$REPO_ROOT/tests/release" "$pf/clone/tests/"
pg add -A
pg commit -q -m r
pg remote add origin "$pf/origin.git"
pg remote add github "$pf/github.git"
pg push -q origin main
pg push -q github "$PF_BASE:refs/heads/main"
pg fetch -q github
pg config gpg.format ssh
pg config user.signingkey "$pf/key.pub"
# The stubs. gh answers by endpoint with what its --jq would print.
cat > "$pf/bin/gh" << 'EOF2'
#!/bin/sh
echo "$*" >> "$FAKE_GH_LOG"
case "$*" in
  --version) echo "gh version 2.0 (stub)" ;;
  "api user"*) echo "${FAKE_GH_USER-anthonytw}" ;;
  "api repos/o/r/environments/crates-io/deployment-branch-policies"*) echo "tag:v*" ;;
  "api repos/o/r/environments/crates-io"*prevent_self_review*) echo "${FAKE_SELF_REVIEW:-false}" ;;
  "api repos/o/r/environments/crates-io"*) echo "${FAKE_REVIEWERS-anthonytw}" ;;
  "api repos/o/r/rulesets/7"*) echo "${FAKE_RULESET_REF-refs/tags/v*}" ;;
  "api repos/o/r/rulesets"*) echo 7 ;;
  "api repos/o/r/immutable-releases"*) [ -z "${FAKE_IMMUTABLE_OFF:-}" ] || { echo "gh: Not Found (HTTP 404)" >&2; exit 1; }; echo true ;;
  "api repos/o/r"*) echo "${FAKE_ADMIN:-true}" ;;
  "release view"*)
    [ -z "${FAKE_RELEASE:-}" ] || exit 0
    [ -z "${FAKE_GH_DOWN:-}" ] || { echo "HTTP 502: Bad Gateway" >&2; exit 1; }
    echo "release not found" >&2; exit 1 ;;
  *) echo "gh stub: unexpected: $*" >&2; exit 2 ;;
esac
EOF2
cat > "$pf/bin/ssh" << 'EOF2'
#!/bin/sh
[ "$1" = -V ] && { echo "OpenSSH stub" >&2; exit 0; }
echo "Hi ${FAKE_SSH_USER:-anthonytw}! You've successfully authenticated, but GitHub does not provide shell access." >&2
exit 1
EOF2
cat > "$pf/bin/docker" << 'EOF2'
#!/bin/sh
[ "$1" = --version ] && { echo "Docker stub"; exit 0; }
[ -z "${FAKE_DOCKER_DOWN:-}" ]
EOF2
cat > "$pf/bin/limactl" << 'EOF2'
#!/bin/sh
case "$1" in
  --version) echo "limactl stub" ;;
  list) echo "${FAKE_VM_STATUS:-Running}" ;;
  shell)
    shift 2
    case "$*" in
      "docker info") [ -z "${FAKE_VM_DOCKER_DOWN:-}" ] ;;
      "df -Pk /") printf 'Filesystem 1024-blocks Used Available Capacity Mounted\n/dev/vda1 100 1 %s 1%% /\n' "${FAKE_VM_KIB:-104857600}" ;;
      "docker image inspect"*) [ -z "${FAKE_VM_NO_IMAGE:-}" ] ;;
      "docker run"*) case "$*" in *--pull=never*) echo "${FAKE_VM_ARCH:-x86_64}" ;; *) exit 3 ;; esac ;;
      "sh -c"*) echo "${FAKE_BINFMT:-rosetta=enabled qemu=}" ;;
      *) exit 2 ;;
    esac ;;
  *) exit 2 ;;
esac
EOF2
cat > "$pf/bin/df" << 'EOF2'
#!/bin/sh
printf 'Filesystem 1024-blocks Used Available Capacity Mounted\n/dev/disk1 100 1 %s 1%% /\n' "${FAKE_KIB:-104857600}"
EOF2
cat > "$pf/bin/cargo" << 'EOF2'
#!/bin/sh
[ "$1" = --version ] && { echo "cargo stub"; exit 0; }
case "$*" in *"${FAKE_STALE_LOCK:-none}"*) echo "error: the lock file needs to be updated but --locked was passed" >&2; exit 101 ;; esac
EOF2
cat > "$pf/bin/curl" << 'EOF2'
#!/bin/sh
[ "$1" = --version ] && { echo "curl stub"; exit 0; }
printf '%s' "${FAKE_CRATES_CODE:-404}"
EOF2
cat > "$pf/bin/gpg" << 'EOF2'
#!/bin/sh
case "$*" in
  --version) echo "gpg stub" ;;
  --show-keys*)
    printf 'pub:-:255:22:K::::::sc::::\nfpr:::::::::ABCDEF0123456789ABCDEF0123456789ABCDEF01:\n'
    [ -z "${FAKE_TWO_KEYS:-}" ] || printf 'pub:-:255:22:L::::::sc::::\nfpr:::::::::1111111111111111111111111111111111111111:\n' ;;
  --list-secret-keys*) [ -z "${FAKE_NO_SECRET:-}" ] && printf 'sec:%s:255:22:K:1:%s::u:::sc::::\n' "${FAKE_KEY_VALID:-u}" "${FAKE_KEY_EXPIRES-}" ;;
  *) exit 2 ;;
esac
EOF2
cat > "$pf/bin/security" << 'EOF2'
#!/bin/sh
[ -n "${FAKE_NO_IDENTITY:-}" ] || echo '  1) ABC "Developer ID Application: Test (TEAM)"'
EOF2
cat > "$pf/bin/notarytool" << 'EOF2'
#!/bin/sh
[ -z "${FAKE_NOTARY_FAIL:-}" ]
EOF2
cat > "$pf/bin/codesign" << 'EOF2'
#!/bin/sh
[ -z "${FAKE_CODESIGN_FAIL:-}" ] || { echo "errSecInternalComponent" >&2; exit 1; }
EOF2
for t in rpmsign rpmkeys xcrun ssh-add; do printf '#!/bin/sh\necho %s stub\n' "$t" > "$pf/path/$t"; done
# just: a stub, but recipes.sh's --dump goes to the real one
printf '#!/bin/sh\ncase " $* " in *" --dump "*) exec %s "$@" ;; esac\necho just stub\n' "$(command -v just)" > "$pf/path/just"
chmod +x "$pf/bin"/* "$pf/path"/*
# pfx [ENV=VALUE ...] CMD...: CMD with the fixture's stubs; pfr [ENV=VALUE ...]: the preflight itself
pfx() {
  env FAKE_GH_LOG="$pf/gh.log" PATH="$pf/path:$PATH" FIL_RELEASE_SELFTEST=1 FIL_RELEASE_ROOT="$pf/clone" FIL_RELEASE_KEY="$pf/key" \
    FIL_GH="$pf/bin/gh" FIL_SSH="$pf/bin/ssh" FIL_DOCKER="$pf/bin/docker" FIL_LIMACTL="$pf/bin/limactl" FIL_DF="$pf/bin/df" \
    FIL_CARGO="$pf/bin/cargo" FIL_CRATES_CURL="$pf/bin/curl" FIL_GPG="$pf/bin/gpg" FIL_SECURITY="$pf/bin/security" \
    FIL_NOTARYTOOL="$pf/bin/notarytool" FIL_CODESIGN="$pf/bin/codesign" FIL_CODESIGN_IDENTITY="Developer ID Application: Test (TEAM)" \
    "$@"
}
pfr() { pfx "$@" "$PF" 0.9.0 --repo o/r; }
# pf_class NAME MESSAGE [ENV=VALUE ...]: fails, in exactly one check (the rest of the fixture is
# sound), and prints MESSAGE (a fixed string)
pf_class() {
  local name=$1 msg=$2
  shift 2
  if pfr "$@" > "$tmp/out.log" 2>&1; then
    flunk "preflight: $name (it should have failed)"
    sed 's/^/       | /' "$tmp/out.log"
  elif ! grep -Eq '^preflight: (1 of [0-9]+ checks FAILED|stopped: test-only)' "$tmp/out.log"; then
    flunk "preflight: $name (more than one check failed)"
    grep '^preflight: FAIL' "$tmp/out.log" | sed 's/^/       | /'
  elif grep -qF -- "$msg" "$tmp/out.log"; then pass "preflight: $name"; else
    flunk "preflight: $name (failed, but not with '$msg')"
    sed 's/^/       | /' "$tmp/out.log" | tail -12
  fi
}
pf_refs() { pg for-each-ref --format='%(refname) %(objectname)' | LC_ALL=C sort; pg status --porcelain; }
pf_refs > "$pf/refs.before"
expect_pass "preflight: everything in order passes" pfr
expect_pass "preflight: ...and changed no ref and no file of the clone" sh -c 'd=$1; pg() { git -C "$d/clone" "$@"; }; { pg for-each-ref --format="%(refname) %(objectname)" | LC_ALL=C sort; pg status --porcelain; } | cmp -s - "$d/refs.before"' sh "$pf"
expect_pass "preflight: --list shows the step map" sh -c '"$1" --list | grep -q "^sign-packages  *package-key$"' sh "$PF"
expect_pass "preflight: sourcing it runs nothing and defines the API" bash -c 'source "$1" && pf_init 0.9.0 && test "$(pf_checks_from crates)" = "overrides tools lock disk tree recipes gh github-ssh commit-signing crates-env"' sh "$PF"
expect_pass "preflight: the recipes check passes on the tree" pfx env FIL_RELEASE_ROOT="$rt/clean" bash -c 'source "$1" && pf_init 0.9.0 && PF_CUR=recipes && pf_check_recipes' sh "$PF"
expect_fail "preflight: the recipes check fails when release.just imports a file" pfx env FIL_RELEASE_ROOT="$rt/m1" bash -c 'source "$1" && pf_init 0.9.0 && PF_CUR=recipes && pf_check_recipes' sh "$PF"
grep -q 'FAIL  recipes: .*release.just imports or mounts another file' "$tmp/out.log" && pass "...with that message" || { flunk "...with that message"; sed "s/^/       | /" "$tmp/out.log"; }
expect_fail "preflight: an unknown step is refused" pfx "$PF" 0.9.0 --repo o/r --from nosuch
expect_fail "preflight: an unknown check is refused" pfx "$PF" 0.9.0 --repo o/r --only nosuch
# the override check (outside the self-test, a fake is fatal)
pf_class "a test-only override outside the self-test is fatal" "a preflight that asks a fake proves nothing" FIL_RELEASE_SELFTEST=
# each class
pf_class "a missing tool" "rpmsign is not installed" FIL_RPMSIGN="$pf/nosuch"
mkdir -p "$pf/clone/target/release-verify/0.9.0/.lock"
sleep 300 > /dev/null 2>&1 &
pf_sleeper=$!
echo "$pf_sleeper" > "$pf/clone/target/release-verify/0.9.0/.lock/pid"
pf_class "a live lock" "another release run (pid $pf_sleeper)"
kill "$pf_sleeper" 2> /dev/null || true
wait "$pf_sleeper" 2> /dev/null || true
expect_pass "preflight: the lock of the run that calls it is not another run's" pfx sh -c 'echo $$ > "$1/target/release-verify/0.9.0/.lock/pid" && exec "$2" 0.9.0 --repo o/r' sh "$pf/clone" "$PF"
rm -rf "$pf/clone/target"
pf_class "low disk on the Mac" "disk full (97% on 2026-11-07)" FAKE_KIB=1048576
pf_class "low disk in the VM" "GiB free in VM fil" FAKE_VM_KIB=1048576
echo dirty >> "$pf/clone/CHANGELOG.md"
pf_class "a dirty tree" "uncommitted changes"
pg checkout -q -- CHANGELOG.md
pf_class "HEAD is not Gitea's main" "is not origin's main" sh -c 'git -C "$1" commit -q --allow-empty --no-gpg-sign -m x && exec "$2" 0.9.0 --repo o/r' sh "$pf/clone" "$PF"
pg reset -q --hard origin/main
pg remote set-url origin "$pf/nosuch.git"
pf_class "Gitea unreachable" "cannot read main on remote 'origin'"
pg remote set-url origin "$pf/origin.git"
pf_class "--commit binds the run to another commit" "the release commit this run is bound to" sh -c 'exec "$1" 0.9.0 --repo o/r --commit 0123456789012345678901234567890123456789' sh "$PF"
pf_class "the wrong version" "Cargo.toml's version is '0.9.0', not 0.9.1" sh -c 'exec "$1" 0.9.1 --repo o/r --only version' sh "$PF"
pf_class "...and no changelog section for it" "CHANGELOG.md has no section for 0.9.1" sh -c 'exec "$1" 0.9.1 --repo o/r --only version' sh "$PF"
pf_class "a stale exporter lock" "catches: stale exporter lock" FAKE_STALE_LOCK=exporter
pf_class "gh logged in as someone else" "gh is logged in as 'other'" FAKE_GH_USER=other
pf_class "gh without admin" "no admin rights" FAKE_ADMIN=false
pf_class "ssh to GitHub as someone else" "did not say 'Hi anthonytw'" FAKE_SSH_USER=other
pf_class "crates-io without the owner as reviewer" "catches: crates-io reviewers missing" FAKE_REVIEWERS=
pf_class "crates-io preventing self-review" "prevents self-review" FAKE_SELF_REVIEW=true
pf_class "no tag ruleset on v*" "no active tag ruleset" FAKE_RULESET_REF=refs/tags/x*
pf_class "a release that exists" "already has a release" FAKE_RELEASE=1
if pfr FAKE_GH_DOWN=1 > "$tmp/out.log" 2>&1 && grep -q 'WARN  tag-free: could not ask GitHub whether a release' "$tmp/out.log"; then pass "preflight: a gh error is not read as no release (a warning)"; else flunk "preflight: a gh error is not read as no release (a warning)"; sed 's/^/       | /' "$tmp/out.log" | tail -8; fi
if pfr FAKE_IMMUTABLE_OFF=1 > "$tmp/out.log" 2>&1 && grep -q 'WARN  tag-ruleset: immutable releases are off' "$tmp/out.log"; then pass "preflight: immutable releases off (404) is a warning"; else flunk "preflight: immutable releases off (404) is a warning"; sed 's/^/       | /' "$tmp/out.log" | tail -8; fi
pf_class "a crates.io version that exists" "already on crates.io" FAKE_CRATES_CODE=200
pf_class "Docker down on the Mac" "catches: Docker not started" FAKE_DOCKER_DOWN=1
pf_class "...says how to start it" "open -a Docker" FAKE_DOCKER_DOWN=1
pf_class "the VM stopped" "limactl start fil" FAKE_VM_STATUS=Stopped
pf_class "the VM without the docker group" "catches: VM docker group" FAKE_VM_DOCKER_DOWN=1
pf_class "the VM under qemu, though uname -m says x86_64" "no enabled Rosetta binfmt registration" FAKE_BINFMT="rosetta= qemu=enabled"
pf_class "the VM with qemu-x86_64 registered beside Rosetta" "both Rosetta and qemu-x86_64" FAKE_BINFMT="rosetta=enabled qemu=enabled"
pf_class "the cross image not running as amd64" "does not run as amd64" FAKE_VM_ARCH=aarch64
if pfr FAKE_VM_NO_IMAGE=1 > "$tmp/out.log" 2>&1 && grep -q 'note  vm: the cross image is not in VM' "$tmp/out.log"; then pass "preflight: no cross image in the VM: nothing pulled, a note"; else flunk "preflight: no cross image in the VM: nothing pulled, a note"; sed 's/^/       | /' "$tmp/out.log" | tail -8; fi
pf_class "no release public key" "no release public key" FIL_RELEASE_KEY="$pf/nosuch"
pf_class "a release key the signers do not list" "is not listed for both git and fil-release" FIL_RELEASE_KEY="$pf/otherkey"
pf_class "no package secret key" "catches: lost package key" FAKE_NO_SECRET=1
pf_class "an expired package key" "has expired" FAKE_KEY_EXPIRES=1000
pf_class "a revoked package key" "is revoked" FAKE_KEY_VALID=r
pf_class "a package key file with two keys" "is not exactly one OpenPGP key" FAKE_TWO_KEYS=1
if pfr FAKE_KEY_EXPIRES=$(($(date +%s) + 86400 * 10)) > "$tmp/out.log" 2>&1 && grep -q 'WARN  package-key: .* expires in' "$tmp/out.log"; then pass "preflight: ...it passes with a warning"; else flunk "preflight: ...it passes with a warning"; sed 's/^/       | /' "$tmp/out.log" | tail -8; fi
pf_class "no Developer ID" "no valid codesigning identity" FAKE_NO_IDENTITY=1
pf_class "no notary profile" "store-credentials fil-notary" FAKE_NOTARY_FAIL=1
pf_class "a codesign that fails (locked keychain)" "security unlock-keychain" FAKE_CODESIGN_FAIL=1
pf_class "git cannot sign" "git cannot sign a commit" sh -c 'git -C "$1" config user.signingkey "$1/nosuch.pub"; "$2" 0.9.0 --repo o/r; s=$?; git -C "$1" config user.signingkey "$3"; exit $s' sh "$pf/clone" "$PF" "$pf/key.pub"
pf_class "GitHub's main not fetched" "git fetch github" sh -c 'echo extra > "$1/extra" && git -C "$1" add extra && git -C "$1" commit -q --no-gpg-sign -m m && git -C "$1" push -q github HEAD:main && git -C "$1" reset -q --hard origin/main && git -C "$1" update-ref -d refs/remotes/github/main && git -C "$1" reflog expire --expire=now --all && git -C "$1" gc -q --prune=now && exec "$2" 0.9.0 --repo o/r' sh "$pf/clone" "$PF"
pg fetch -q github
# the mirror is a fast-forward of GitHub's main to the release commit itself; pf_gh_main WHAT: the
# preflight's one line about it, with GitHub's main at WHAT
pf_gh_main() {
  pg push -q -f github "$1:refs/heads/main" && pg fetch -q github
  pfr > "$tmp/out.log" 2>&1
}
pf_gh_line() {
  if [ "$1" = 0 ] && grep -q -- "$3" "$tmp/out.log"; then pass "preflight: $2"; else flunk "preflight: $2 (exit $1)"; sed 's/^/       | /' "$tmp/out.log" | tail -8; fi
}
# GitHub's main past the release commit (the commit with a file the release commit lacks, above):
# a warning (the dry run needs main at R), no failure
st=0; pf_gh_main "$(pg rev-parse github/main)" || st=$?
pf_gh_line "$st" "github/main past the release commit is a warning, not a failure" 'WARN  github-main: .* has moved on past the release commit'
st=0; pf_gh_main "$PF_BASE" || st=$?
pf_gh_line "$st" "github/main behind the release commit: the mirror fast-forwards it (a note)" 'note  github-main: .* is behind the release commit (new commits: 1): step mirror fast-forwards it'
st=0; pf_gh_main "$(pg rev-parse HEAD)" || st=$?
pf_gh_line "$st" "github/main at the release commit: the mirror is done" 'ok    github-main: github/main is the release commit'
pg push -q -f github "$(pg commit-tree "$(pg rev-parse 'HEAD^{tree}')" -p "$PF_BASE" -m 'pushed to GitHub only'):refs/heads/main"
pg fetch -q github
pf_class "github/main diverged from the release commit" "step mirror only fast-forwards (it never forces)"
pg push -q -f github "$PF_BASE:refs/heads/main"
pg fetch -q github
pf_class "the tag on GitHub already exists" "already exists on GitHub" sh -c 'git -C "$1" push -q github origin/main:refs/tags/v0.9.0 && "$2" 0.9.0 --repo o/r; s=$?; git -C "$1" push -q github :refs/tags/v0.9.0; exit $s' sh "$pf/clone" "$PF"
# after the tag: main moves on, and a run bound to the release commit still passes
R=$(pg rev-parse HEAD)
echo later > "$pf/clone/later"
pg add later
pg commit -q --no-gpg-sign -m later
pg push -q origin HEAD:main
pg reset -q --hard "$R"
expect_pass "preflight: --commit R --from crates passes with Gitea's main ahead of R" pfx "$PF" 0.9.0 --repo o/r --commit "$R" --from crates
expect_pass "preflight: --from crates without --commit binds to HEAD, on main" pfx "$PF" 0.9.0 --repo o/r --from crates
pf_class "...but an unbound run before the tag still needs HEAD == main" "is not origin's main"
pg fetch -q origin
pf_class "a bound commit that is not on main" "is not on origin's main" sh -c 'git -C "$1" commit -q --allow-empty --no-gpg-sign -m side && exec "$2" 0.9.0 --repo o/r --commit "$(git -C "$1" rev-parse HEAD)" --from crates' sh "$pf/clone" "$PF"
pg reset -q --hard "$R"
pg push -q -f origin "$R:main"
# a resume at `crates` needs neither the VM nor Docker nor the keys
expect_pass "preflight: --from crates does not look at the VM, Docker or the keys" pfx FAKE_VM_STATUS=Stopped FAKE_DOCKER_DOWN=1 FAKE_NO_SECRET=1 FAKE_NO_IDENTITY=1 "$PF" 0.9.0 --repo o/r --from crates
expect_pass "preflight: every gh call was a read (no -X, -f, -F, --input; no release create/edit/delete/upload)" sh -c 'test -s "$1" && ! grep -Eq -- "(^| )(-X|--method|-f|-F|--field|--raw-field|--input)( |=|$)|^release (create|edit|delete|upload)" "$1"' sh "$pf/gh.log"

# ============================================================== 10. release.sh (the state machine, and the actions of steps 1-12)
# On the preflight's fixture (the clone, its bare `origin` and `github`, the keys, the stubs): a fake
# GitHub world (runs, releases, assets, dispatches) in a directory, a tap, synthetic release files.
# Steps 1-4 are done by the command's actions (failures injected at each, then the same command
# again); for the later steps the test does each step's action by hand, then requires the command to
# see it, and nothing more.
echo "== release.sh"
rs=$tmp/rel
RL=$RELEASE_DIR/release.sh
mkdir -p "$rs/world/runs" "$rs/world/releases" "$rs/gnupg"
chmod 700 "$rs/gnupg"
cat > "$rs/gh.py" << 'EOF2'
import hashlib, json, os, re, shutil, subprocess, sys
a = sys.argv[1:]
w = os.environ["FAKE_WORLD"]
with open(os.environ["FAKE_GH_LOG"], "a") as f:
    f.write(" ".join(a) + "\n")
def opt(n):
    return a[a.index(n) + 1] if n in a else None
def down():
    if os.environ.get("FAKE_RL_GH_DOWN"):
        print("HTTP 502: Bad Gateway", file=sys.stderr); sys.exit(1)
def runs():
    d = os.path.join(w, "runs")
    return [json.load(open(os.path.join(d, n))) for n in sorted(os.listdir(d))]
def created(r):
    # a run's creation time: "created" if the test set one, else after the release commit, ordered by id
    return r.get("created") or "2099-01-01T00:%02d:00Z" % (r["id"] % 60)
def run_file(i):
    return os.path.join(w, "runs", "%s.json" % i)
def load_run(i):
    # a rerun shows only after one more look (GitHub's lag): "next" holds what it becomes
    p = run_file(i)
    if not os.path.exists(p):
        print("HTTP 404: Not Found (https://api.github.com/repos/o/r/actions/runs/%s)" % i, file=sys.stderr); sys.exit(1)
    r = json.load(open(p))
    if "next" in r:
        if r.get("lag", 0) > 0:
            r["lag"] -= 1
        else:
            r.update(r.pop("next"))
            r.pop("lag", None)
        json.dump(r, open(p, "w"))
    return r
def rels(tag):
    d = os.path.join(w, "releases")
    out = []
    for n in sorted(os.listdir(d), key=int):
        m = json.load(open(os.path.join(d, n, "meta.json")))
        if m["tag"] == tag:
            out.append((n, m, os.path.join(d, n, "assets")))
    return out
def calls(line):
    with open(os.environ["FAKE_CALLS"], "a") as f:
        f.write(line + "\n")
def new_draft(tag, partial):
    # the publish job's draft: the release files of FAKE_RELEASE_FILES (half of them for a publish
    # that failed part way)
    d = os.path.join(w, "releases")
    # ids are never reused (as on GitHub)
    nf = os.path.join(w, "next-release-id")
    n = max([int(x) for x in os.listdir(d)] + [int(open(nf).read()) if os.path.exists(nf) else 0]) + 1
    open(nf, "w").write(str(n))
    os.makedirs(os.path.join(d, str(n), "assets"))
    json.dump(dict(tag=tag, draft=True, created="2099-01-01T01:%02d:00Z" % n), open(os.path.join(d, str(n), "meta.json"), "w"))
    src = os.environ["FAKE_RELEASE_FILES"]
    names = sorted(os.listdir(src))
    for f in (names[: len(names) // 2] if partial else names):
        shutil.copy(os.path.join(src, f), os.path.join(d, str(n), "assets"))
    calls("draft create %d%s" % (n, " (partial)" if partial else ""))
CRATES = ["fil-proto", "fil-shell"]
def crates_published():
    p = os.path.join(w, "crates")
    return open(p).read().split() if os.path.exists(p) else []
CRATES_PENDING = dict(status="waiting", conclusion="", pending=[7], jobs=[["prepare", "success"], ["publish", "success"], ["crates-io", "-"]])
def advance(r):
    # a tag push's run, one stage per look: prepare, then publish (FAKE_TAG_RUN_FAIL=publish: the
    # first attempt's publish fails after making half a draft; =build: a build job fails), then the
    # crates-io job waits for its deployment's review (pending environment 7)
    if not r.get("auto") or "next" in r:
        return
    if r.get("phase") == "crates":
        # approved (in progress; "crates_lag" more looks first): publish-crates.sh uploads the crates
        # crates.io does not have yet ($FAKE_WORLD/crates); FAKE_CRATES_FAIL: the first time, the job
        # fails after one upload
        if r["status"] != "in_progress":
            return
        if r.get("crates_lag", 0) > 0:
            r["crates_lag"] -= 1
            return
        missing = [c for c in CRATES if c not in crates_published()]
        fail = bool(os.environ.get("FAKE_CRATES_FAIL")) and not r.get("crates_failed")
        for c in missing[:1] if fail else missing:
            open(os.path.join(w, "crates"), "a").write(c + "\n")
            calls("crate upload %s" % c)
        if fail:
            r["crates_failed"] = True
        res = "failure" if fail else "success"
        r.update(status="completed", conclusion=res, jobs=[["prepare", "success"], ["publish", "success"], ["crates-io", res]])
        return
    if r["status"] in ("completed", "waiting"):
        return
    fail = os.environ.get("FAKE_TAG_RUN_FAIL", "") if r.get("attempt", 1) == 1 else ""
    if r["status"] == "queued":
        r.update(status="in_progress", jobs=[["prepare", "success"], ["publish", "-"], ["crates-io", "-"]])
    elif fail == "leg" and not r.get("leg"):
        # a matrix leg fails while the other legs still run: the run stays in progress for a look
        r.update(leg=True, jobs=[["prepare", "success"], ["build-linux (a)", "failure"], ["build-linux (b)", "-"], ["publish", "-"], ["crates-io", "-"]])
    elif fail in ("build", "leg"):
        r.update(status="completed", conclusion="failure", jobs=[["prepare", "success"], ["build-linux", "failure"], ["publish", "skipped"], ["crates-io", "skipped"]])
    elif fail == "publish":
        new_draft(r["branch"], True)
        r.update(status="completed", conclusion="failure", jobs=[["prepare", "success"], ["publish", "failure"], ["crates-io", "skipped"]])
    else:
        new_draft(r["branch"], False)
        r.update(phase="crates", **CRATES_PENDING)
if a[:2] == ["run", "list"]:
    down()
    out = [dict(databaseId=r["id"], status=r["status"], conclusion=r["conclusion"], createdAt=created(r))
           for r in runs() if r["workflow"] == opt("--workflow") and r["event"] == opt("--event") and r["head"] == opt("--commit")
           and (opt("--branch") is None or r["branch"] == opt("--branch"))]
    print(json.dumps(out))
elif a[:2] == ["workflow", "run"]:
    # a dispatch: a queued run on GitHub's main, with the next id; gh prints its URL (newer gh)
    if os.environ.get("FAKE_DISPATCH_FAIL"):
        print("HTTP 422: Workflow does not have 'workflow_dispatch' trigger", file=sys.stderr); sys.exit(1)
    wf = a[2]
    fields = dict(a[i + 1].split("=", 1) for i, x in enumerate(a) if x == "-f")
    head = subprocess.check_output(["git", "--git-dir", os.environ["FAKE_GITHUB_GIT"], "rev-parse", "refs/heads/main"]).decode().strip()
    nf = os.path.join(w, "next-id")
    nid = int(open(nf).read()) if os.path.exists(nf) else 101
    open(nf, "w").write(str(nid + 1))
    json.dump(dict(id=nid, workflow=wf, name=wf[:-4], event="workflow_dispatch", branch="main", head=head, status="queued", conclusion="",
                   jobs=[], suite=fields.get("suite", ""), attempt=1), open(os.path.join(w, "runs", "%d.json" % nid), "w"))
    if os.environ.get("FAKE_NO_RUN_URL"):
        print("Created workflow_dispatch event for %s at main" % wf)
    else:
        print("https://github.com/o/r/actions/runs/%d" % nid)
elif a[:2] in (["run", "watch"], ["run", "rerun"]):
    p = run_file(a[2])
    r = load_run(a[2])
    if a[1] == "rerun":
        if r["status"] != "completed":
            print("run %s cannot be rerun; its workflow run is still in progress (HTTP 403)" % r["id"], file=sys.stderr); sys.exit(1)
        r.pop("leg", None)
        r.pop("lag_left", None)
        if r.get("phase") == "crates":
            # rerun --failed of the crates-io job: it waits for a review again
            r.update(next=dict(CRATES_PENDING, attempt=r.get("attempt", 1) + 1), lag=1)
        else:
            r.update(next=dict(status="queued", conclusion="", jobs=[], attempt=r.get("attempt", 1) + 1), lag=1)
    else:
        if os.environ.get("FAKE_WATCH_FAIL"):
            print("HTTP 502: Bad Gateway", file=sys.stderr); sys.exit(1)
        if r["status"] != "completed":
            # the run finishes; FAKE_RUN_FAIL names the workflows whose first attempt fails
            bad = r["workflow"] in os.environ.get("FAKE_RUN_FAIL", "").split() and r.get("attempt", 1) == 1
            res = "failure" if bad else "success"
            allsuite = r.get("suite") == "all"
            skip = lambda c: c if allsuite else "skipped"
            r["jobs"] = {"release.yml": [["build-linux", res], ["publish", "skipped"]],
                         "linux-check.yml": [["check", "success"], ["realnet (a)", skip("success")], ["aarch64", skip("success")], ["terminals", skip(res)]],
                         "macos-check.yml": [["check (arm64)", res], ["remote (openssh)", skip("success")]]}[r["workflow"]]
            r.update(status="completed", conclusion=res)
        print("run %s completed: %s" % (r["id"], r["conclusion"]))
    json.dump(r, open(p, "w"))
    sys.exit(0 if a[1] == "rerun" or r["conclusion"] == "success" else 1)
elif a[:2] == ["run", "view"] and "--log-failed" in a:
    print("terminals\tRun tests\t##[error]Process completed with exit code 1.")
elif a[:2] == ["run", "view"]:
    down()
    r = load_run(a[2])
    if r.get("auto"):
        advance(r)
        json.dump(r, open(run_file(a[2]), "w"))
    print(json.dumps(dict(headSha=r["head"], event=r["event"], workflowName=r["name"], status=r["status"], conclusion=r["conclusion"], attempt=r.get("attempt", 1),
                          jobs=[dict(name=n, conclusion=c, status="completed") for n, c in r["jobs"]])))
elif a[:2] == ["release", "view"]:
    down()
    rs = rels(a[2])
    if not rs:
        print("release not found", file=sys.stderr); sys.exit(1)
    n, m, d = rs[0]
    assets = []
    for f in sorted(os.listdir(d)):
        e = dict(name=f)
        if not os.environ.get("FAKE_NO_DIGEST"):
            e["digest"] = "sha256:" + hashlib.sha256(open(os.path.join(d, f), "rb").read()).hexdigest()
        assets.append(e)
    print(json.dumps(dict(isDraft=m["draft"], assets=assets)))
elif a[:2] == ["release", "download"]:
    n, m, d = rels(a[2])[0]
    os.makedirs(opt("-D"), exist_ok=True)
    shutil.copy(os.path.join(d, opt("-p")), opt("-D"))
elif a[:1] == ["api"] and re.match(r"repos/o/r/actions/runs/[0-9]+/pending_deployments$", a[1]):
    # what the --jq of pending_crates prints; a deployment shows after FAKE_PENDING_LAG looks (not
    # created yet); FAKE_CANNOT_APPROVE: current_user_can_approve is false
    down()
    i = a[1].split("/")[5]
    r = load_run(i)
    if "lag_left" not in r:
        r["lag_left"] = int(os.environ.get("FAKE_PENDING_LAG") or 0)
    if r["status"] == "waiting" and r.get("pending"):
        if r["lag_left"] > 0:
            r["lag_left"] -= 1
        else:
            for e in r["pending"]:
                print("%d\tcrates-io\t%s" % (e, "false" if os.environ.get("FAKE_CANNOT_APPROVE") else "true"))
    json.dump(r, open(run_file(i), "w"))
elif a[:3] == ["api", "-X", "POST"] and re.match(r"repos/o/r/actions/runs/[0-9]+/pending_deployments$", a[3]):
    i = a[3].split("/")[5]
    r = load_run(i)
    if r["status"] != "waiting" or not r.get("pending"):
        print("HTTP 422: No pending deployment requests to approve or reject", file=sys.stderr); sys.exit(1)
    if [x for x in a if x.startswith("environment_ids[]=")] != ["environment_ids[]=%d" % r["pending"][0]] or "state=approved" not in a:
        print("fake gh: a wrong approval: %s" % a, file=sys.stderr); sys.exit(1)
    r.update(status="in_progress", pending=[])
    r.pop("lag_left", None)
    json.dump(r, open(run_file(i), "w"))
    calls("approve %s attempt %d" % (i, r.get("attempt", 1)))
elif a[:2] == ["release", "edit"]:
    # publish (FAKE_PUBLISH_FAIL: GitHub fails first)
    down()
    if os.environ.get("FAKE_PUBLISH_FAIL"):
        print("HTTP 502: Bad Gateway", file=sys.stderr); sys.exit(1)
    if "--draft=false" not in a:
        print("fake gh: release edit without --draft=false: %s" % a, file=sys.stderr); sys.exit(1)
    n, m, d = rels(a[2])[0]
    m["draft"] = False
    json.dump(m, open(os.path.join(w, "releases", n, "meta.json"), "w"))
    calls("release publish %s" % n)
elif a[:1] == ["api"] and re.match(r"repos/o/r/actions/runs/[0-9]+$", a[1]):
    # what the --jq of run_ours prints: the triggering actor and the creation time
    r = load_run(a[1].rsplit("/", 1)[1])
    print("%s\t%s" % (r.get("actor", "anthonytw"), created(r)))
elif a[:1] == ["api"] and re.match(r"repos/o/r/releases$", a[1]):
    down()
    tag = re.search(r'tag_name == "([^"]*)"', opt("--jq")).group(1)
    for n, m, d in rels(tag):
        print("%s\t%s\t%s" % (n, "true" if m["draft"] else "false", m.get("created", "2099-01-01T00:00:%02dZ" % (int(n) % 60))) if "@tsv" in opt("--jq") else n)
elif a[:1] == ["api"] and re.match(r"repos/o/r/releases/[0-9]+$", a[1]):
    # what the --jq '.assets[].name' prints
    d = os.path.join(w, "releases", a[1].rsplit("/", 1)[1], "assets")
    for f in sorted(os.listdir(d)):
        print(f)
elif a[:3] == ["api", "-X", "DELETE"] and re.match(r"repos/o/r/releases/[0-9]+$", a[3]):
    n = a[3].rsplit("/", 1)[1]
    shutil.rmtree(os.path.join(w, "releases", n))
    calls("draft delete %s" % n)
elif a[:2] == ["release", "upload"]:
    # from the current directory, into the (oldest) release of the tag; FAKE_UPLOAD_FAIL: the first
    # file goes up, then the network fails
    n, m, d = rels(a[2])[0]
    files = [x for x in a[3:] if not x.startswith("-") and x not in ("o/r",)]
    for i, f in enumerate(files):
        if os.environ.get("FAKE_UPLOAD_FAIL") and i == 1:
            print("HTTP 502: Bad Gateway (upload of %s)" % f, file=sys.stderr); sys.exit(1)
        shutil.copy(f, os.path.join(d, f))
else:
    os.execv(os.environ["FAKE_PF_GH"], [os.environ["FAKE_PF_GH"]] + a)
EOF2
printf '#!/bin/sh\nexec python3 -I "%s" "$@"\n' "$rs/gh.py" > "$rs/gh"
cat > "$rs/cargo" << EOF2
#!/bin/sh
case "\$*" in
  *--no-deps*) echo '{"packages":[{"name":"fil-shell","publish":["crates-io"]},{"name":"fil-proto","publish":["crates-io"]},{"name":"fil-testkit","publish":[]}]}' ;;
  *) exec "$pf/bin/cargo" "\$@" ;;
esac
EOF2
cat > "$rs/curl" << 'EOF2'
#!/bin/sh
[ "$1" = --version ] && { echo "curl stub"; exit 0; }
for u; do :; done
c=${u#https://crates.io/api/v1/crates/}; c=${c%%/*}
case " ${FAKE_CRATES_PUBLISHED:-} $(cat "$FAKE_WORLD/crates" 2> /dev/null | tr '\n' ' ') " in *" $c "*) printf 200 ;; *) printf '%s' "${FAKE_CRATES_CODE:-404}" ;; esac
EOF2
# gpg: a detached "signature" is the line "gpgsig <sha256 of the data>"; the rest is the preflight's stub.
cat > "$rs/gpg" << EOF2
#!/bin/sh
case " \$* " in
  *" --import "*) exit 0 ;;
  *" --verify "*)
    for x; do s=\$d; d=\$x; done
    [ "\$(cat "\$s")" = "gpgsig \$(shasum -a 256 "\$d" 2> /dev/null | cut -d' ' -f1 || sha256sum "\$d" | cut -d' ' -f1)" ] || exit 1
    echo "[GNUPG:] VALIDSIG ABCDEF0123456789ABCDEF0123456789ABCDEF01 2026-11-09 0 4 0 22 8 00 ABCDEF0123456789ABCDEF0123456789ABCDEF01" ;;
  *) exec "$pf/bin/gpg" "\$@" ;;
esac
EOF2
chmod +x "$rs/gh" "$rs/cargo" "$rs/curl" "$rs/gpg"
# the tap: a bare remote and a clone
git init -q --bare "$rs/tap.git"
git init -q -b main "$rs/tap"
echo tap > "$rs/tap/README.md"
git -C "$rs/tap" add -A && git -C "$rs/tap" commit -q --no-gpg-sign -m tap
git -C "$rs/tap" remote add origin "$rs/tap.git"
git -C "$rs/tap" push -q origin main
git -C "$rs/tap" fetch -q origin
TAP0=$(git -C "$rs/tap" rev-parse HEAD)
# each push to the tap is logged; refused while $rs/reject-tap exists
cat > "$rs/tap.git/hooks/pre-receive" << EOF2
#!/bin/sh
while read -r old new ref; do
  if [ -e "$rs/reject-tap" ]; then echo "remote: error: refused (selftest)" >&2; exit 1; fi
  echo "tap push \$ref \$new" >> "$rs/calls.log"
done
EOF2
chmod +x "$rs/tap.git/hooks/pre-receive"
# Homebrew: brew logs its calls; install and upgrade put a fil that prints the version line into
# FAKE_BREW_PREFIX/bin (FAKE_BREW_FAIL: they fail)
cat > "$rs/brew-stub" << 'EOF2'
#!/bin/sh
echo "brew $*" >> "$FAKE_CALLS"
case $1 in
  --prefix) echo "$FAKE_BREW_PREFIX" ;;
  update) : ;;
  list) [ -x "$FAKE_BREW_PREFIX/bin/fil" ] ;;
  install | upgrade)
    [ -z "${FAKE_BREW_FAIL:-}" ] || { echo "Error: fil: SHA256 mismatch (stub)" >&2; exit 1; }
    mkdir -p "$FAKE_BREW_PREFIX/bin"
    printf '#!/bin/sh\necho "fil 0.9.0 (wire 9.9)"\n' > "$FAKE_BREW_PREFIX/bin/fil"
    chmod +x "$FAKE_BREW_PREFIX/bin/fil"
    ;;
esac
EOF2
chmod +x "$rs/brew-stub"
: > "$pf/gh.log"
# The step scripts' stubs (fake.py): release-verify logs its arguments (FAKE_VERIFY_FAIL makes it fail
# as verify.sh does) and, for the draft, "downloads" it into the work directory and records the
# verification as verify.sh does; sign-macos and sign-packages change the work directory (and, for
# sign-macos, the draft) as the scripts do (FAKE_SIGN_FAIL=macos|packages: they fail first); and the
# tag push's run, made by the github hook.
cat > "$rs/fake.py" << 'EOF2'
import hashlib, json, os, shutil, subprocess, sys
def sha(p):
    return hashlib.sha256(open(p, "rb").read()).hexdigest()
def calls(line):
    with open(os.environ["FAKE_CALLS"], "a") as f:
        f.write(line + "\n")
cmd, args = sys.argv[1], sys.argv[2:]
if cmd == "tag-run":
    w, head = args
    nf = os.path.join(w, "next-id")
    nid = int(open(nf).read()) if os.path.exists(nf) else 101
    open(nf, "w").write(str(nid + 1))
    json.dump(dict(id=nid, workflow="release.yml", name="release", event="push", branch="v0.9.0", head=head, status="queued", conclusion="",
                   jobs=[], attempt=1, auto=True), open(os.path.join(w, "runs", "%d.json" % nid), "w"))
    sys.exit(0)
W = os.environ["FAKE_RW"]
F = os.path.join(W, "files")
world = os.environ["FAKE_WORLD"]
MAC = "fil-0.9.0-universal-apple-darwin.tar.gz"
def draft():
    d = os.path.join(world, "releases")
    ns = [n for n in sorted(os.listdir(d), key=int) if json.load(open(os.path.join(d, n, "meta.json")))["tag"] == "v0.9.0"]
    return os.path.join(d, ns[-1], "assets")
def resum():
    p = os.path.join(F, "SHA256SUMS")
    out = []
    for l in open(p).read().splitlines():
        h, n = l.split("  ", 1)
        out.append("%s  %s" % (sha(os.path.join(F, n)), n))
    open(p, "w").write("\n".join(out) + "\n")
def set_verified(**kv):
    p = os.path.join(W, "verified")
    lines = [l for l in open(p).read().splitlines() if l.split("=", 1)[0] not in kv]
    open(p, "w").write("\n".join(lines + ["%s=%s" % x for x in kv.items()]) + "\n")
if cmd == "verify":
    calls("verify " + " ".join(args))
    fail = os.environ.get("FAKE_VERIFY_FAIL", "")
    if fail == "lima":
        print("release-verify: Lima instance 'fil' is not running (limactl start fil)", file=sys.stderr); sys.exit(1)
    if fail == "mismatch":
        print("release-verify: FAIL fil-0.9.0-source.tar.gz: released aaaa, rebuilt bbbb\nrelease-verify: 1 check(s) FAILED: do not sign", file=sys.stderr); sys.exit(1)
    if "--dry-run" not in args:
        presign = "--pre-sign" in args
        shutil.rmtree(F, ignore_errors=True)
        shutil.copytree(draft(), F)
        open(os.path.join(W, "verified"), "w").write("sums=%s\nrebuild=1\npresign=%d\ndry=0\n" % (sha(os.path.join(F, "SHA256SUMS")), presign))
        if not presign:
            print("release-verify: all checks passed", file=sys.stderr); sys.exit(0)
    print("release-verify: all checks passed EXCEPT the signature, which a dry run or --pre-sign does not check", file=sys.stderr)
elif cmd == "sign-macos":
    calls("sign-macos " + " ".join(args))
    if os.environ.get("FAKE_SIGN_FAIL") == "macos":
        print('notarytool: "status": "Invalid"', file=sys.stderr); sys.exit(1)
    key = os.environ["FIL_RELEASE_KEY"]
    assert key.endswith(".pub"), key
    open(os.path.join(F, MAC), "w").write("signed-mac%s\n" % (" again" if "--force" in args else ""))
    resum()
    if os.path.exists(os.path.join(F, "SHA256SUMS.sig")):
        os.remove(os.path.join(F, "SHA256SUMS.sig"))
    subprocess.check_call(["ssh-keygen", "-q", "-Y", "sign", "-f", key, "-n", "fil-release", os.path.join(F, "SHA256SUMS")], stdin=subprocess.DEVNULL)
    for f in (MAC, "SHA256SUMS", "SHA256SUMS.sig"):
        shutil.copy(os.path.join(F, f), draft())
    set_verified(sums=sha(os.path.join(F, "SHA256SUMS")), macos="signed")
elif cmd == "sign-packages":
    calls("sign-packages " + " ".join(args))
    if os.environ.get("FAKE_SIGN_FAIL") == "packages":
        print("gpg: signing failed: No pinentry", file=sys.stderr); sys.exit(1)
    rd = os.environ["FAKE_RELEASE_DIR"]
    for a in ("x86_64", "aarch64"):
        p = os.path.join(F, "fil-0.9.0-1.%s.rpm" % a)
        info = subprocess.check_output(["python3", "-I", os.path.join(rd, "pkg_inspect.py"), p, p + ".x"]).decode()
        if "signed yes" not in info.splitlines():
            subprocess.check_call(["python3", "-I", os.path.join(rd, "selftest_pkg.py"), "sign", p])
        shutil.rmtree(p + ".x", ignore_errors=True)
    open(os.path.join(F, ".signed-locally"), "w").close()
    resum()
    open(os.path.join(F, "SHA256SUMS.asc"), "w").write("gpgsig %s\n" % sha(os.path.join(F, "SHA256SUMS")))
EOF2
for t in verify sign-macos sign-packages; do printf '#!/bin/sh\nexec python3 -I "%s" %s "$@"\n' "$rs/fake.py" "$t" > "$rs/$t"; done
# release-keys.sh: status says ready unless $rs/keys-locked exists; unlock removes it, forget makes it
cat > "$rs/keys" << 'EOF2'
#!/bin/sh
echo "keys $*" >> "$FAKE_CALLS"
case $1 in
  status) if [ -e "$FAKE_KEYS_LOCKED" ]; then echo "ssh    locked     (stub)"; exit 1; fi; echo "ssh    ready      (stub)" ;;
  unlock) [ -z "${FAKE_KEYS_UNLOCK_FAIL:-}" ] || { echo "release-keys: ssh: ssh-add failed (stub)" >&2; exit 1; }; rm -f "$FAKE_KEYS_LOCKED" ;;
  forget) [ -z "${FAKE_KEYS_FORGET_FAIL:-}" ] || { echo "release-keys: forget failed (stub)" >&2; exit 1; }; : > "$FAKE_KEYS_LOCKED" ;;
esac
EOF2
chmod +x "$rs/verify" "$rs/sign-macos" "$rs/sign-packages" "$rs/keys"
: > "$rs/calls.log"
RL_SOCK=$rs/none.sock
rlx() {
  pfx env FIL_GH="$rs/gh" FIL_GPG="$rs/gpg" FIL_CARGO="$rs/cargo" FIL_CRATES_CURL="$rs/curl" FAKE_WORLD="$rs/world" FAKE_PF_GH="$pf/bin/gh" \
    FIL_RELEASE_VERIFY="$rs/verify" FAKE_CALLS="$rs/calls.log" FAKE_GITHUB_GIT="$pf/github.git" \
    FIL_SIGN_MACOS="$rs/sign-macos" FIL_SIGN_PACKAGES="$rs/sign-packages" FIL_RELEASE_KEYS="$rs/keys" FIL_RELEASE_POLL=0 \
    FAKE_RW="$RW" FAKE_RELEASE_FILES="$rs/files0" FAKE_RELEASE_DIR="$RELEASE_DIR" FAKE_KEYS_LOCKED="$rs/keys-locked" \
    FIL_BREW="$rs/brew-stub" FAKE_BREW_PREFIX="$rs/brewprefix" FIL_RELEASE_SHARED="$rs/shared/fil-0.9.0" \
    SSH_AUTH_SOCK="$RL_SOCK" GNUPGHOME="$rs/gnupg" "$@"
}
# rl [ENV=VALUE ...] [-- ARGS ...]: a run of the command in this world
rl() {
  local -a e=() x=()
  while [ $# -gt 0 ] && [ "$1" != -- ]; do e+=("$1"); shift; done
  [ $# = 0 ] || { shift; x=("$@"); }
  rlx ${e[@]+"${e[@]}"} "$RL" 0.9.0 --repo o/r --tap "$rs/tap" ${x[@]+"${x[@]}"}
}
# last: the output of the last expect_pass/expect_fail, kept before the next one overwrites it
last() { cp "$tmp/out.log" "$rs/last.log"; }
rls() { rlx "$@" "$RL" 0.9.0 --repo o/r --tap "$rs/tap" --status; }
RW=$pf/clone/target/release-verify/0.9.0
RSTATE=$RW/release.state
RR=$(pg rev-parse HEAD)
# rl_status_is N STEP KIND: --status says step N is KIND ("done (checked)", "next", ...)
rl_line() { rls "${@:3}" > "$rs/status.out" 2>&1; grep -E "^ +$1/17 $2 " "$rs/status.out" | head -n 1; }
rl_kind() {
  local line
  line=$(rl_line "$1" "$2" "${@:5}")
  case $line in *" $2 "*"$3"*) pass "release status: $2 is $3${4:+ ($4)}" ;; *) flunk "release status: $2 is $3${4:+ ($4)} (it says: ${line:-nothing})"; sed 's/^/       | /' "$rs/status.out" | head -30 ;; esac
}
# rl_paused N STEP: a run stops at step N with exit 2
rl_paused() {
  local st=0
  rl "${@:3}" > "$rs/run.out" 2>&1 || st=$?
  if [ $st = 2 ] && grep -q "^release: PAUSED at step $1/17 $2\$" "$rs/run.out"; then pass "release run: paused at $1 $2"; else
    flunk "release run: paused at $1 $2 (exit $st)"
    sed 's/^/       | /' "$rs/run.out" | tail -15
  fi
}
rl_refs() { pg for-each-ref --format='%(refname) %(objectname)' | grep -v '^refs/remotes/' | LC_ALL=C sort; pg status --porcelain; }

expect_fail "release: a test-only override outside the self-test is refused" rl FIL_RELEASE_SELFTEST=
last
expect_pass "release: ...saying so" grep -q 'a release that asks a fake proves nothing' "$rs/last.log"
# each override alone, with no other set, is refused by name
ov_unset=$(for o in $TEST_OVERRIDES; do printf -- '-u %s ' "$o"; done)
ov_bad=''
for v in $TEST_OVERRIDES; do
  [ "$v" != FIL_RELEASE_SELFTEST ] || continue
  # shellcheck disable=SC2086 # the -u words
  if env $ov_unset "$v=x" "$RL" 0.9.0 --status > "$rs/ov.out" 2>&1 || ! grep -q "STOP: $v is set" "$rs/ov.out"; then ov_bad="$ov_bad $v"; fi
done
if [ -z "$ov_bad" ]; then pass "release: every test-only override, set alone, is refused by name"; else flunk "release: every test-only override, set alone, is refused by name (not:$ov_bad)"; fi
expect_fail "release: --redo tag is refused" rlx "$RL" 0.9.0 --redo tag
last
expect_pass "release: ...it cannot be redone" grep -q 'it cannot be redone' "$rs/last.log"
for x in mirror tag-run publish crates finish candidate nosuch; do expect_fail "release: --redo $x is refused" rlx "$RL" 0.9.0 --redo "$x"; done
expect_fail "release: --redo does not go with --from" rlx "$RL" 0.9.0 --redo upload --from tag
expect_fail "release: --redo does not go with --status" rlx "$RL" 0.9.0 --redo upload --status
expect_fail "release: --from names a step" rlx "$RL" 0.9.0 --from nosuch
expect_fail "release: --status does not go with --from" rlx "$RL" 0.9.0 --status --from mirror

# 0: nothing done but the candidate
rl_refs > "$rs/refs.before"
rl_kind 1 candidate "done (checked)"
rl_kind 2 mirror next
rl_kind 3 dry-run pending
expect_pass "release status: wrote no state" test ! -e "$RSTATE"
expect_pass "release status: ...and changed no ref or file of the clone" sh -c 'rl_refs() { git -C "$1" for-each-ref --format="%(refname) %(objectname)" | grep -v "^refs/remotes/" | LC_ALL=C sort; git -C "$1" status --porcelain; }; rl_refs "$1" | cmp -s - "$2"' sh "$pf/clone" "$rs/refs.before"
rl_paused 2 mirror
expect_pass "release run: ...at the confirmation, with no terminal, in the failure format" sh -c 'grep -q "input is not a terminal: nothing was done" "$1" && grep -q "^  do:    run it in a terminal (there is no --yes)" "$1" && grep -q "^  then:  tests/release/just.sh release 0.9.0 .*(steps 1-1 are done" "$1"' sh "$rs/run.out"
expect_pass "release run: ...the preflight ran for mirror and after" grep -q '^preflight: all .* checks passed for 0.9.0' "$rs/run.out"
expect_pass "release run: ...the state is bound to the version and HEAD" sh -c 'grep -qx "version=0.9.0" "$1" && grep -qx "release_commit=$2" "$1"' sh "$RSTATE" "$RR"
expect_pass "release run: ...and the lock is gone" test ! -e "$RW/.lock"

# binding
cp "$RSTATE" "$rs/state.saved"
sed "s/^release_commit=.*/release_commit=0123456789012345678901234567890123456789/" "$rs/state.saved" > "$RSTATE"
expect_fail "release run: a state bound to another commit is refused" rl
last
expect_pass "release run: ...saying there is no tag yet and --reset-state rebinds" sh -c 'grep -q "no v0.9.0 on GitHub yet" "$1" && grep -q -- "--reset-state (it rebinds)" "$1"' sh "$rs/last.log"
rl_paused 2 mirror -- --reset-state
expect_pass "release run: ...--reset-state before the tag rebinds" grep -qx "release_commit=$RR" "$RSTATE"
# the lock
mkdir -p "$RW/.lock"
sleep 300 > /dev/null 2>&1 &
rl_sleeper=$!
echo "$rl_sleeper" > "$RW/.lock/pid"
expect_fail "release run: a live lock stops a second run" rl
last
expect_pass "release run: ...naming its pid" grep -q "another release run (pid $rl_sleeper)" "$rs/last.log"
kill "$rl_sleeper" 2> /dev/null || true
wait "$rl_sleeper" 2> /dev/null || true
rl_paused 2 mirror
expect_pass "release run: ...a stale lock is removed with a note" grep -q 'removed a stale lock' "$rs/run.out"
# three runs against a stale lock, all waiting on the held reap mutex, then released together: the
# mutex lets exactly one reap and take the lock, which it holds (FIL_RELEASE_LOCK_HOLD) until the
# other two have found it held; then it goes on and pauses at mirror; none leaves a lock behind
mkdir -p "$RW/.lock" "$RW/.lock.reap"
echo "$rl_sleeper" > "$RW/.lock/pid"
touch "$rs/lock-hold"
for n in 1 2 3; do rm -f "$rs/race.$n.st"; (st=0; rl FIL_RELEASE_LOCK_HOLD="$rs/lock-hold" > "$rs/race.$n" 2>&1 || st=$?; echo $st > "$rs/race.$n.st") & done
sleep 1
rmdir "$RW/.lock.reap"
n=0
until [ "$(cat "$rs"/race.?.st 2> /dev/null | grep -c .)" -ge 2 ] || [ $n -ge 250 ]; do n=$((n + 1)); sleep 0.1; done
rm -f "$rs/lock-hold"
wait
expect_pass "release run: three runs released together onto a stale lock: exactly one wins (paused at mirror), two find it held" sh -c 'w=0 h=0; for n in 1 2 3; do case $(cat "$1/race.$n.st") in 2) grep -q "PAUSED at step 2/17 mirror" "$1/race.$n" && w=$((w + 1)) ;; 1) grep -q "another release run (pid" "$1/race.$n" && h=$((h + 1)) ;; esac; done; test "$w $h" = "1 2"' sh "$rs"
expect_pass "release run: ...and left no lock or mutex behind" sh -c '! ls -d "$1"/.lock* > /dev/null 2>&1' sh "$RW"
mkdir "$RW/.lock.reap"
expect_fail "release run: a reap mutex left by a killed run is reported (after 10 s)" rl
last
expect_pass "release run: ...saying how to remove it" grep -q "rmdir '.*/.lock.reap'" "$rs/last.log"
rmdir "$RW/.lock.reap"
# release.sh is one { ... } group ending in `exit`, so bash has read all of it before it runs any of
# it: a copy rewritten IN PLACE (same inode, as an editor does) while it waits in the lock hook goes
# on unaffected and pauses at mirror. The new text would exit 97, or fail on a fragment of a line,
# wherever bash resumed reading: the control, the same copy without the group, is derailed by it.
ri=$tmp/rel-immune
for kind in wrapped unwrapped; do
  rm -rf "$ri" "$RW/.lock" "$rs/immune.$kind.st"
  mkdir -p "$ri/tests"
  cp -R "$RELEASE_DIR" "$ri/tests/release"
  cp -R "$REPO_ROOT/packaging" "$ri/packaging"
  if [ $kind = unwrapped ]; then
    python3 -I -c 'import sys
p = sys.argv[1]
s = open(p).read()
i = s.index("\n{\nset -euo pipefail\n")
s = s[:i + 1] + s[i + 3:]
assert s.endswith("exit\n}\n")
open(p, "w").write(s[: -len("exit\n}\n")])' "$ri/tests/release/release.sh"
  fi
  touch "$rs/lock-hold"
  (st=0; rlx FIL_RELEASE_LOCK_HOLD="$rs/lock-hold" "$ri/tests/release/release.sh" 0.9.0 --repo o/r --tap "$rs/tap" > "$rs/immune.$kind" 2>&1 || st=$?; echo $st > "$rs/immune.$kind.st") &
  n=0
  until [ -s "$RW/.lock/pid" ] || [ $n -ge 200 ]; do n=$((n + 1)); sleep 0.1; done
  python3 -I -c 'import sys
f = open(sys.argv[1], "r+b")
n = len(f.read())
f.seek(0)
f.write(b"exit 97\n" * (n // 8 + 64))
f.truncate()
f.close()' "$ri/tests/release/release.sh"
  rm -f "$rs/lock-hold"
  wait
done
expect_pass "release run: a copy of release.sh rewritten in place while it runs: unaffected, paused at mirror" sh -c 'test "$(cat "$1.st")" = 2 && grep -q "PAUSED at step 2/17 mirror" "$1" && ! grep -q "command not found" "$1"' sh "$rs/immune.wrapped"
expect_pass "release run: ...the control, the copy without the { ... } group, is derailed by the same rewrite" sh -c 'st=$(cat "$1.st"); test "$st" = 97 || test "$st" = 127' sh "$rs/immune.unwrapped"
rm -rf "$ri"

# ---- the actions of steps 1-4 (PR 4), on a test ssh-agent's commit-signing key (git config names
# the .pub, whose private half is only in the agent), with a pre-receive hook on the github bare repo
# that logs each push to main (and refuses it while $rs/reject-push exists), the gh world's dispatch,
# watch and rerun, and the release-verify stub. Each case starts from GitHub's main before the mirror.
mkdir -p "$rs/agentkey" "$rs/agentpub"
cp "$pf/key" "$rs/agentkey/key"
cp "$pf/key.pub" "$rs/agentpub/key.pub"
ssh-agent -a "$rs/agent.sock" > "$rs/agent.env"
rs_agent=$(sed -n 's/^SSH_AGENT_PID=\([0-9]*\);.*/\1/p' "$rs/agent.env")
trap 'kill "$rs_agent" 2> /dev/null; rm -rf "$tmp"' EXIT INT TERM
SSH_AUTH_SOCK=$rs/agent.sock ssh-add -q "$rs/agentkey/key" 2> /dev/null
RL_SOCK=$rs/agent.sock
pg config user.signingkey "$rs/agentpub/key.pub"
# A tag push: refused while $rs/reject-tag exists (the tag ruleset); with $rs/tag-run-on-push, it
# starts the tag's release.yml run in the gh world, which the fake advances one stage per look.
cat > "$pf/github.git/hooks/pre-receive" << EOF2
#!/bin/sh
while read -r old new ref; do
  if [ "\$ref" = refs/heads/main ] && [ -e "$rs/reject-push" ]; then
    echo "remote: error: GH006: Protected branch update failed for refs/heads/main." >&2
    exit 1
  fi
  if [ "\$ref" = refs/tags/v0.9.0 ] && [ -e "$rs/reject-tag" ]; then
    echo "remote: error: GH013: Repository rule violations found for refs/tags/v0.9.0." >&2
    exit 1
  fi
  echo "push \$ref \$new" >> "$rs/calls.log"
  if [ "\$ref" = refs/tags/v0.9.0 ] && [ -e "$rs/tag-run-on-push" ]; then
    python3 -I "$rs/fake.py" tag-run "$rs/world" "\$(git rev-parse "\$new^{commit}")"
  fi
done
EOF2
chmod +x "$pf/github.git/hooks/pre-receive"
GMAIN0=$(pg rev-parse github/main)
cp -R "$pf/github.git" "$rs/github.saved"
ra_reset() {
  rm -rf "$pf/github.git" "$rs/world/runs" "$RW/logs" "$rs/reject-push"
  cp -R "$rs/github.saved" "$pf/github.git"
  mkdir "$rs/world/runs"
  rm -f "$rs/world/next-id" "$RSTATE"
  : > "$rs/calls.log"
  : > "$pf/gh.log"
  pg fetch -q github
}
# ra_expect NAME STATUS PATTERN [ENV=VALUE ...] [-- ARGS ...]: a run whose input counts as a terminal
# (stdin answers), exiting STATUS with a line matching PATTERN (extended regex)
ra_expect() {
  local name=$1 want=$2 pat=$3 st=0
  shift 3
  rl FIL_RELEASE_TTY=1 "$@" > "$rs/ra.out" 2>&1 || st=$?
  if [ "$st" = "$want" ] && grep -Eq -- "$pat" "$rs/ra.out"; then pass "$name"; else
    flunk "$name (exit $st, wanted $want and /$pat/)"
    sed 's/^/       | /' "$rs/ra.out" | tail -25
  fi
}
n_push() { grep -c '^push refs/heads/main ' "$rs/calls.log" || true; }
n_dispatch() { grep -c '^workflow run ' "$pf/gh.log" || true; }
n_rerun() { grep -c '^run rerun ' "$pf/gh.log" || true; }
n_verify() { grep -c '^verify ' "$rs/calls.log" || true; }
# calls_are PUSHES DISPATCHES RERUNS VERIFIES: the call log, so far in this case
calls_are() {
  local got
  got="$(n_push) $(n_dispatch) $(n_rerun) $(n_verify)"
  if [ "$got" = "$1 $2 $3 $4" ]; then pass "release actions: ...calls: $1 push, $2 dispatches, $3 reruns, $4 verifies"; else
    flunk "release actions: ...calls: wanted $1 push, $2 dispatches, $3 reruns, $4 verifies; got $got"
  fi
}

ra_reset
ra_expect "release actions: no terminal at the confirmation: paused, exit 2" 2 'PAUSED at step 2/17 mirror' FIL_RELEASE_TTY= < /dev/null
calls_are 0 0 0 0
ra_expect "release actions: a declined confirmation: paused, exit 2" 2 'not confirmed: nothing was done' <<< "yes"
expect_pass "release actions: ...the confirmation shows what becomes public, mirror and tag" sh -c 'grep -q "CONFIRM tag, for 0.9.0" "$1" && grep -q "step 2 mirror: GitHub.s main fast-forwarded from .* to the release commit" "$1" && grep -q "step 5 tag (this answer covers it too)" "$1"' sh "$rs/ra.out"
calls_are 0 0 0 0
ra_expect "release actions: --confirm-each asks for the mirror on its own" 2 'CONFIRM mirror, for 0.9.0' -- --confirm-each <<< ""
expect_pass "release actions: ...and says the tag is asked for at step 5" grep -q 'the tag is asked for on its own, at step 5' "$rs/ra.out"
ra_expect "release actions: --from dry-run with the mirror undone is refused" 1 'FAILED at step 2/17 mirror' -- --from dry-run < /dev/null
expect_pass "release actions: ...saying the step before is not done" grep -q -- '--from dry-run: step 2 mirror is not done' "$rs/ra.out"
calls_are 0 0 0 0
# step 2 fails (GitHub refuses the push), then the same command again
touch "$rs/reject-push"
ra_expect "release actions: a refused mirror push fails at step 2" 1 'FAILED at step 2/17 mirror' <<< "0.9.0"
expect_pass "release actions: ...with the known fix for a ruleset refusal" grep -q 'ruleset refused a fast-forward push' "$rs/ra.out"
expect_pass "release actions: ...and the log's tail and path" sh -c 'grep -q "GH006" "$1" && grep -q "logs/02-mirror.log" "$1"' sh "$rs/ra.out"
calls_are 0 0 0 0
rm -f "$rs/reject-push"
ra_expect "release actions: ...the same command again goes on to step 5 (run ids found by listing)" 2 'PAUSED at step 5/17 tag' FAKE_NO_RUN_URL=1 -- --confirm-each <<< "0.9.0"
calls_are 1 3 0 1
expect_pass "release actions: ...and recorded the runs it found by listing" sh -c 'grep -qx dry_run=101 "$1" && grep -qx checks_linux=102 "$1" && grep -qx checks_macos=103 "$1"' sh "$RSTATE"

# step 3 fails (the wait dies), then the state is lost while the runs are still going
ra_reset
ra_expect "release actions: a wait that dies fails at step 3" 1 'FAILED at step 3/17 dry-run' FAKE_WATCH_FAIL=1 <<< "0.9.0"
expect_pass "release actions: ...saying a re-run re-attaches" grep -q 're-attaches to the run' "$rs/ra.out"
calls_are 1 3 0 0
rm -f "$RSTATE"
ra_expect "release actions: ...the state deleted, the same command again attaches to the running runs" 2 'PAUSED at step 5/17 tag' < /dev/null
calls_are 1 3 0 1
expect_pass "release actions: ...and re-derived the state" sh -c 'grep -qx "github_commit=$(git -C "$2" rev-parse refs/heads/main)" "$1" && grep -qx dry_run=101 "$1" && grep -q "^dry_verify=ok 101 " "$1"' sh "$RSTATE" "$pf/github.git"

# step 3 fails (a job fails): the rerun offer declined, then no terminal, then accepted
ra_reset
ra_expect "release actions: a failed check fails at step 3, the rerun declined" 1 'FAILED at step 3/17 dry-run' FAKE_RUN_FAIL=linux-check.yml <<< "$(printf '0.9.0\nn')"
expect_pass "release actions: ...with the failed job's log" grep -q 'Process completed with exit code 1' "$rs/ra.out"
calls_are 1 3 0 0
ra_expect "release actions: ...again without a terminal: paused (exit 2) for the decision, saying how to rerun" 2 'gh run rerun ID --failed' FIL_RELEASE_TTY= < /dev/null
expect_pass "release actions: ...and that whether it blocks the release is decided at a terminal" grep -q 'whether they block the release is your decision, at a terminal' "$rs/ra.out"
calls_are 1 3 0 0
ra_expect "release actions: ...again, the rerun accepted: on to step 5" 2 'PAUSED at step 5/17 tag' <<< "y"
calls_are 1 3 1 1

# step 4 fails (a rebuild that differs; the VM down), then the same command again
ra_reset
ra_expect "release actions: a dry run that does not verify fails at step 4" 1 'FAILED at step 4/17 dry-verify' FAKE_VERIFY_FAIL=mismatch <<< "0.9.0"
expect_pass "release actions: ...saying never to tag" grep -q 'STOP: do NOT tag' "$rs/ra.out"
expect_pass "release actions: ...and recording nothing for it" sh -c '! grep -q "^dry_verify=" "$1"' sh "$RSTATE"
ra_expect "release actions: ...the VM down fails at step 4 too" 1 'do: +limactl start fil' FAKE_VERIFY_FAIL=lima < /dev/null
calls_are 1 3 0 2
ra_expect "release actions: ...the same command again: on to step 5" 2 'PAUSED at step 5/17 tag' < /dev/null
calls_are 1 3 0 3
# a state bound to another commit, mid-way: refused, nothing done
cp "$RSTATE" "$rs/state.mid"
sed "s/^release_commit=.*/release_commit=0123456789012345678901234567890123456789/" "$rs/state.mid" > "$RSTATE"
ra_expect "release actions: a state bound to another commit is refused mid-way" 1 'the state is for 0.9.0 at 012345678901' < /dev/null
calls_are 1 3 0 3
cp "$rs/state.mid" "$RSTATE"

# ra_midprompt ACTION [ENV=VALUE ...]: a run that waits at the confirmation (after the preflight)
# while the test changes the world (ACTION, shell code), then gets 0.9.0 typed; RA_ST is its exit
ra_midprompt() {
  local act=$1 n=0 pid
  shift
  rm -f "$rs/fifo" "$rs/ra.out"
  mkfifo "$rs/fifo"
  rl FIL_RELEASE_TTY=1 "$@" < "$rs/fifo" > "$rs/ra.out" 2>&1 &
  pid=$!
  exec 8> "$rs/fifo"
  until grep -q 'type 0.9.0 to go ahead' "$rs/ra.out" 2> /dev/null || [ $n -ge 600 ] || ! kill -0 "$pid" 2> /dev/null; do
    n=$((n + 1))
    sleep 0.1
  done
  eval "$act"
  (echo 0.9.0 >&8) 2> /dev/null || true
  exec 8>&-
  RA_ST=0
  wait "$pid" || RA_ST=$?
}
# ra_check NAME STATUS PATTERN: the last ra_midprompt exited STATUS with a line matching PATTERN
ra_check() {
  if [ "$RA_ST" = "$2" ] && grep -Eq -- "$3" "$rs/ra.out"; then pass "$1"; else
    flunk "$1 (exit $RA_ST, wanted $2 and /$3/)"
    sed 's/^/       | /' "$rs/ra.out" | tail -25
  fi
}
# gh_move_main PARENT: GitHub's main moves to a new commit on PARENT, with PARENT's tree (not a push:
# the hook does not log it)
gh_move_main() {
  local c
  c=$(git --git-dir="$pf/github.git" commit-tree "$1^{tree}" -p "$1" -m moved)
  git --git-dir="$pf/github.git" update-ref refs/heads/main "$c"
  echo "$c"
}
printf '#!/bin/sh\nfor a; do f=$a; done\n: > "$f.sig"\n' > "$rs/emptysig"
chmod +x "$rs/emptysig"

# GitHub's main moves between the fetch and the push to a commit the release commit lacks: the push
# is refused (fetch first), never forced; the same command again refuses the diverged main
ra_reset
ra_midprompt 'GMOVED=$(gh_move_main "$GMAIN0")'
ra_check "release actions: main moved during the confirmation: the mirror push fails at step 2" 1 'FAILED at step 2/17 mirror'
expect_pass "release actions: ...with the known fix for a moved main" grep -q "GitHub's main moved since the fetch" "$rs/ra.out"
calls_are 0 0 0 0
ra_expect "release actions: ...the same command again: a diverged main is refused at step 2" 1 'FAILED at step 2/17 mirror' -- --confirm-each <<< "0.9.0"
expect_pass "release actions: ...saying the mirror only fast-forwards" grep -q 'step mirror only fast-forwards (it never forces)' "$rs/ra.out"
calls_are 0 0 0 0
expect_pass "release actions: ...and GitHub's main is untouched" test "$(git --git-dir="$pf/github.git" rev-parse refs/heads/main)" = "$GMOVED"
rl_kind 2 mirror next "github's main diverged"
expect_pass "release status: ...saying it cannot be fast-forwarded" grep -q 'has commits the release commit lacks: it cannot be fast-forwarded' "$rs/status.out"
# ... and to the release commit itself (the agents pushed it meanwhile): the push is a no-op
ra_reset
ra_midprompt 'git --git-dir="$pf/github.git" update-ref refs/heads/main "$RR"' -- --confirm-each
ra_check "release actions: main moved to the release commit during the confirmation: on to step 5" 2 'PAUSED at step 5/17 tag'
calls_are 0 3 0 1
# GitHub's main already at the release commit before the run (the agents' fast-forward): the mirror
# is done, nothing is pushed, and the tag is confirmed at step 5 on its own
ra_reset
git --git-dir="$pf/github.git" update-ref refs/heads/main "$RR"
pg fetch -q github
rl_kind 2 mirror "done (checked)" "github's main is the release commit"
ra_expect "release actions: GitHub's main already the release commit: --from dry-run is accepted" 2 'PAUSED at step 5/17 tag' -- --from dry-run < /dev/null
calls_are 0 3 0 1
expect_pass "release actions: ...recording it as GitHub's commit, and asking for the tag at step 5" sh -c 'grep -qx "github_commit=$2" "$1" && grep -q "CONFIRM tag, for 0.9.0" "$3" && ! grep -q "CONFIRM mirror" "$3"' sh "$RSTATE" "$RR" "$rs/ra.out"
# ... and moved on past it (the agents pushed later merges): the mirror is done, but the dry run,
# which builds main's head, is not dispatched
ra_reset
git --git-dir="$pf/github.git" update-ref refs/heads/main "$RR"
GMOVED=$(gh_move_main "$RR")
pg fetch -q github
rl_kind 2 mirror "done (checked)" "github's main past the release commit"
ra_expect "release actions: ...GitHub's main past the release commit: no dispatch" 1 'not the release commit .*: a dispatch on main would test another commit' < /dev/null
expect_pass "release actions: ...saying to hold the pushes of main" grep -q "hold the agents' pushes of main" "$rs/ra.out"
calls_are 0 0 0 0
# the dispatch refused (422), then GitHub's main moves past the release commit: no dispatch
ra_reset
ra_expect "release actions: a refused dispatch fails at step 3" 1 'FAILED at step 3/17 dry-run' FAKE_DISPATCH_FAIL=1 <<< "0.9.0"
expect_pass "release actions: ...with the known fix for it" grep -q 'needs workflow_dispatch' "$rs/ra.out"
calls_are 1 1 0 0
GMOVED=$(gh_move_main "$(git --git-dir="$pf/github.git" rev-parse refs/heads/main)")
ra_expect "release actions: ...GitHub's main past the release commit: no dispatch" 1 'not the release commit .*: a dispatch on main would test another commit' < /dev/null
calls_are 1 1 0 0
# the state lost while the runs go on: a run someone else dispatched, or one older than the mirror
# commit, is not attached to (a new one is dispatched); this release's is
ra_reset
ra_expect "release actions: a wait that dies, again" 1 'FAILED at step 3/17 dry-run' FAKE_WATCH_FAIL=1 <<< "0.9.0"
python3 -I -c 'import json, sys
for f, k, v in ((sys.argv[1], "actor", "someone-else"), (sys.argv[2], "created", "2020-01-01T00:00:00Z")):
    r = json.load(open(f)); r[k] = v; json.dump(r, open(f, "w"))' "$rs/world/runs/101.json" "$rs/world/runs/102.json"
rm -f "$RSTATE"
ra_expect "release actions: ...the state deleted: foreign and older running runs are not attached to" 2 'PAUSED at step 5/17 tag' < /dev/null
calls_are 1 5 0 1
expect_pass "release actions: ...two new runs, and this release's macos-check run kept" sh -c 'grep -qx dry_run=104 "$1" && grep -qx checks_linux=105 "$1" && grep -qx checks_macos=103 "$1"' sh "$RSTATE"
# a recorded run GitHub no longer has: forgotten, and the step goes on as if none was recorded
ra_reset
ra_expect "release actions: a wait that dies, once more" 1 'FAILED at step 3/17 dry-run' FAKE_WATCH_FAIL=1 <<< "0.9.0"
rm -f "$rs/world/runs/101.json"
ra_expect "release actions: ...a recorded run deleted on GitHub is forgotten, and dispatched again" 2 'PAUSED at step 5/17 tag' < /dev/null
calls_are 1 4 0 1
expect_pass "release actions: ...recording the new run" grep -qx dry_run=104 "$RSTATE"

# the happy path, from the start
ra_reset
ra_expect "release actions: the happy path through dry-verify, paused at step 5 (--confirm-each: the tag asks again)" 2 'PAUSED at step 5/17 tag' -- --confirm-each <<< "0.9.0"
calls_are 1 3 0 1
RG=$(git -C "$pf/github.git" rev-parse refs/heads/main)
# the signers file of the agent's commit-signing key (the tap's formula commit is checked with it)
printf 't@example.invalid namespaces="git" %s\n' "$(cut -d' ' -f1,2 "$pf/key.pub")" > "$rs/allowed"
expect_pass "release actions: ...GitHub's main is the release commit itself, fast-forwarded from its old main" sh -c 'test "$1" = "$2" && grep -qx "push refs/heads/main $2" "$3" && git -C "$4" merge-base --is-ancestor "$5" "$2"' sh "$RG" "$RR" "$rs/calls.log" "$pf/clone" "$GMAIN0"
expect_pass "release actions: ...dispatched the three on main, the checks suite=all at the release commit" sh -c 'grep -qx "workflow run release.yml --repo o/r --ref main" "$1" && grep -qx "workflow run linux-check.yml --repo o/r --ref main -f suite=all -f ref=$2" "$1" && grep -qx "workflow run macos-check.yml --repo o/r --ref main -f suite=all -f ref=$2" "$1"' sh "$pf/gh.log" "$RG"
expect_pass "release actions: ...verified the dry run at the release commit" grep -qx "verify 0.9.0 --dry-run --from-run 101 --ref $RG --repo o/r" "$rs/calls.log"
expect_pass "release actions: ...and recorded it all" sh -c 'grep -qx "github_commit=$2" "$1" && grep -qx dry_run=101 "$1" && grep -qx checks_linux=102 "$1" && grep -qx checks_macos=103 "$1" && grep -Eq "^dry_verify=ok 101 $2 [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z$" "$1"' sh "$RSTATE" "$RG"
expect_pass "release actions: ...one log per step that acted" sh -c 'test -s "$1/02-mirror.log" && test -s "$1/03-dry-run.log" && test -s "$1/04-dry-verify.log"' sh "$RW/logs"
expect_pass "release actions: ...and asked for the tag on its own, with nothing pushed" sh -c 'grep -q "CONFIRM tag, for 0.9.0" "$1" && grep -q "not confirmed: nothing was done" "$1" && ! grep -q "^push refs/tags/" "$2"' sh "$rs/ra.out" "$rs/calls.log"
ra_expect "release actions: the same command again does nothing new" 2 'PAUSED at step 5/17 tag' < /dev/null
calls_are 1 3 0 1
ra_expect "release actions: --from tag with steps 1-4 done is accepted" 2 'PAUSED at step 5/17 tag' -- --from tag < /dev/null
# the status after dry-verify, against its golden
rls 2>&1 | awk '$1 ~ /^[0-9]+\/17$/ { k = $3; if (k == "done") k = k " " $4; print $1, $2, k }' > "$rs/golden4.out"
cat > "$rs/golden4" << 'EOF2'
1/17 candidate done (checked)
2/17 mirror done (checked)
3/17 dry-run done (checked)
4/17 dry-verify done (recorded)
5/17 tag next
6/17 tag-run pending
7/17 verify-presign pending
8/17 sign-macos pending
9/17 sign-packages pending
10/17 sign-sums pending
11/17 upload pending
12/17 verify-signed pending
13/17 publish pending
14/17 verify-published pending
15/17 crates pending
16/17 brew pending
17/17 finish pending
EOF2
expect_pass "release status: the listing after dry-verify matches its golden" diff "$rs/golden4" "$rs/golden4.out"
pg config user.signingkey "$pf/key.pub"

# detection, on the world the actions left (the later steps by hand, as the owner's scripts leave
# things; the keys locked, so a run without a terminal pauses at a step that signs): GitHub's main
# moved on to a commit the clone has not fetched: the recorded GitHub commit is not proof enough
# (status does not fetch)
: > "$rs/keys-locked"
rl_kind 2 mirror "done (checked)"
git clone -q -b main "$pf/github.git" "$rs/other"
git -C "$rs/other" commit -q --allow-empty --no-gpg-sign -m later
git -C "$rs/other" push -q origin HEAD:main
rl_kind 2 mirror "next (unknown)" "github's main not in the clone"
expect_pass "release status: ...says to fetch it" grep -q 'git fetch github' "$rs/status.out"
git -C "$rs/other" push -q -f origin "$RG:refs/heads/main"
rm -rf "$rs/other"
pg fetch -q github

# the runs: found again without the state, a newer suite=check run is passed over
mkrun() { # ID WORKFLOW NAME EVENT BRANCH HEAD STATUS CONCLUSION JOB=CONCLUSION...
  python3 -I -c 'import json, sys
a = sys.argv[1:]
print(json.dumps(dict(id=int(a[0]), workflow=a[1], name=a[2], event=a[3], branch=a[4], head=a[5], status=a[6], conclusion=a[7],
                      jobs=[j.split("=", 1) for j in a[8:]])))' "$@" > "$rs/world/runs/$1.json"
}
cp "$RSTATE" "$rs/state.k"
mkrun 105 linux-check.yml linux-check workflow_dispatch main "$RG" completed success check=success "realnet (a)=skipped" aarch64=skipped terminals=skipped
grep -v '^checks_linux=' "$rs/state.k" > "$RSTATE"
rl_kind 3 dry-run "done (checked)" "a newer suite=check run is passed over"
expect_pass "release status: ...with the runs' URLs" sh -c 'grep -q "https://github.com/o/r/actions/runs/101" "$1" && grep -q "actions/runs/102" "$1"' sh "$rs/status.out"
cp "$rs/world/runs/102.json" "$rs/run102.saved"
python3 -I -c 'import json, sys; r = json.load(open(sys.argv[1])); r["actor"] = "someone-else"; json.dump(r, open(sys.argv[1], "w"))' "$rs/world/runs/102.json"
rl_kind 3 dry-run next "...but not one someone else dispatched"
cp "$rs/run102.saved" "$rs/world/runs/102.json"
cp "$rs/state.k" "$RSTATE"
echo "dry_verify=ok 999 $RG 2026-11-09T12:00:00Z" >> "$RSTATE"
rl_kind 4 dry-verify next "a record for another run does not count"
cp "$rs/state.k" "$RSTATE"
# a recorded dry-verify never makes a red check read as done (implied)
cp "$rs/world/runs/102.json" "$rs/run102.saved"
mkrun 102 linux-check.yml linux-check workflow_dispatch main "$RG" completed failure check=success "realnet (a)=success" aarch64=success terminals=failure
rl_kind 3 dry-run next "the suite=all check failed, dry-verify recorded"
rl_kind 4 dry-verify pending "...and dry-verify does not count without it"
cp "$rs/run102.saved" "$rs/world/runs/102.json"
rl_paused 5 tag

# 4: burned tags: one the signers on main do not list, one on a commit of another tree
pg -c user.signingkey="$pf/otherkey.pub" tag -s -m 'fil 0.9.0' v0.9.0-gh "$RG"
pg push -q github v0.9.0-gh:refs/tags/v0.9.0
rl_kind 5 tag BURNED "signed by a key not on main"
expect_fail "release run: a burned tag is a hard stop" rl
last
expect_pass "release run: ...this version is burned" grep -q 'this version is burned: bump to the next patch' "$rs/last.log"
pg push -q github :refs/tags/v0.9.0
pg tag -d v0.9.0-gh > /dev/null
# (a commit on GitHub's main past the release commit: the release commit's tree and one more file,
# a tree no commit on origin's main has)
RX=$({ git --git-dir="$pf/github.git" ls-tree "$RG"; printf '100644 blob %s\tside\n' "$(echo side | git --git-dir="$pf/github.git" hash-object -w --stdin)"; } | git --git-dir="$pf/github.git" mktree)
RX=$(git --git-dir="$pf/github.git" commit-tree "$RX" -p "$RG" -m side)
git --git-dir="$pf/github.git" update-ref refs/heads/main "$RX"
pg fetch -q github
pg tag -s -m 'fil 0.9.0' v0.9.0-gh "$RX"
pg push -q github v0.9.0-gh:refs/tags/v0.9.0
rl_kind 5 tag BURNED "on a commit without the release commit's tree"
# the same tag with nothing bound: a run has fetched origin and finds no commit on its main with the
# tag's tree, so the tag is burned (not a checkout of another commit)
cp "$RSTATE" "$rs/state.b"
rm -f "$RSTATE"
expect_fail "release run: a good tag of a tree no commit on origin's main has, nothing bound, is burned" rl
last
expect_pass "release run: ...saying why, and that the version is burned" sh -c 'grep -q "no commit on origin.s main has that tree" "$1" && grep -q "this version is burned: bump to the next patch" "$1"' sh "$rs/last.log"
cp "$rs/state.b" "$RSTATE"
pg push -q github :refs/tags/v0.9.0
pg tag -d v0.9.0-gh > /dev/null
git --git-dir="$pf/github.git" update-ref refs/heads/main "$RG"
pg fetch -q github
# the real tag
pg tag -s -m 'fil 0.9.0' v0.9.0-gh "$RG"
pg push -q github v0.9.0-gh:refs/tags/v0.9.0
rl_kind 5 tag "done (checked)"
rl_kind 6 tag-run next
expect_fail "release run: --reset-state after the tag is refused" rl -- --reset-state
last
expect_pass "release run: ...the tag binds its commit" grep -q 'reset-state is refused (the tag binds' "$rs/last.log"
expect_fail "release run: the tag's run never appears: fails at 6 after its polls" rl
last
expect_pass "release run: ...saying so" sh -c 'grep -q "FAILED at step 6/17 tag-run" "$1" && grep -q "no release.yml run of the tag push" "$1"' sh "$rs/last.log"
cp "$rs/last.log" "$rs/run.out"
expect_pass "release run: ...recorded the tag object" grep -qx "tag_object=$(pg rev-parse v0.9.0-gh)" "$RSTATE"
expect_pass "release run: ...and the preflight was bound to the release commit" grep -q 'HEAD is the bound release commit' "$rs/run.out"

# 5: the tag's run and its draft (synthetic release files)
mkrun 201 release.yml release push v0.9.0 "$RG" waiting - prepare=success publish=success crates-io=-
rf=$rs/files0
mkdir -p "$rf"
printf 'x\n' > "$rs/payload"
printf '%s\t/usr/bin/fil\t755\n' "$rs/payload" > "$rs/manifest"
for f in fil-0.9.0-aarch64-linux-musl.tar.gz fil-0.9.0-source.tar.gz fil-0.9.0-x86_64-linux-musl.tar.gz fil_0.9.0_amd64.deb fil_0.9.0_arm64.deb; do echo "$f" > "$rf/$f"; done
echo ci-mac > "$rf/fil-0.9.0-universal-apple-darwin.tar.gz"
cp "$rf/fil-0.9.0-universal-apple-darwin.tar.gz" "$rf/fil-0.9.0-universal-apple-darwin.ci.tar.gz"
for a in x86_64 aarch64; do python3 -I "$RELEASE_DIR/selftest_pkg.py" rpm "$rf/fil-0.9.0-1.$a.rpm" fil 0.9.0 1 "$a" 0 "$rs/manifest"; done
rewrite_sums "$rf" 0.9.0
mkdraft() { # ID
  mkdir -p "$rs/world/releases/$1/assets"
  echo '{"tag": "v0.9.0", "draft": true}' > "$rs/world/releases/$1/meta.json"
  cp "$rf"/* "$rs/world/releases/$1/assets/"
}
mkdraft 1
DA=$rs/world/releases/1/assets
rl_kind 6 tag-run "done (checked)"
mkdraft 2
rl_kind 6 tag-run next "two drafts"
expect_pass "release status: ...names the double-draft trap" grep -q 'double-draft trap' "$rs/status.out"
rm -rf "$rs/world/releases/2"
expect_fail "release run: on to verify-presign (the VM down: it fails there)" rl FAKE_VERIFY_FAIL=lima
last
expect_pass "release run: ...at step 7, with the fix" sh -c 'grep -q "FAILED at step 7/17 verify-presign" "$1" && grep -q "do: *limactl start fil" "$1"' sh "$rs/last.log"
expect_pass "release run: ...recorded the tag run" grep -qx tag_run=201 "$RSTATE"

# 6-12, as the owner's scripts leave the work directory
mkdir -p "$RW"
cp -R "$rf" "$RW/files"
printf 'sums=%s\nrebuild=1\npresign=1\ndry=0\n' "$(sha256 "$RW/files/SHA256SUMS")" > "$RW/verified"
rl_kind 7 verify-presign "done (checked)"
rl_paused 8 sign-macos
# sign-macos: a new macOS archive, SHA256SUMS and .sig, in the draft and the work directory
echo signed-mac > "$RW/files/fil-0.9.0-universal-apple-darwin.tar.gz"
rewrite_sums "$RW/files" 0.9.0
ssh-keygen -q -Y sign -f "$pf/key" -n fil-release "$RW/files/SHA256SUMS"
cp "$RW/files/fil-0.9.0-universal-apple-darwin.tar.gz" "$DA/"
rl_kind 8 sign-macos next "an upload that stopped after the archive"
cp "$RW/files/SHA256SUMS" "$DA/"
rl_kind 8 sign-macos pending "...and before the .sig (verify-presign no longer matches the draft either)"
cp "$RW/files/SHA256SUMS.sig" "$DA/"
printf 'sums=%s\nrebuild=1\npresign=1\ndry=0\nmacos=signed\n' "$(sha256 "$RW/files/SHA256SUMS")" > "$RW/verified"
rl_kind 8 sign-macos "done (checked)"
rl_kind 7 verify-presign "done (checked)" "still, after sign-macos"
rl_kind 10 sign-sums pending "sign-macos's .sig alone is not sign-sums"
# sign-packages
for a in x86_64 aarch64; do python3 -I "$RELEASE_DIR/selftest_pkg.py" sign "$RW/files/fil-0.9.0-1.$a.rpm"; done
touch "$RW/files/.signed-locally"
rewrite_sums "$RW/files" 0.9.0
echo "gpgsig $(sha256 "$RW/files/SHA256SUMS")" > "$RW/files/SHA256SUMS.asc"
rl_kind 9 sign-packages "done (checked)"
rl_kind 10 sign-sums next "the old .sig covers the old SHA256SUMS"
echo "gpgsig 0" > "$RW/files/SHA256SUMS.asc"
rl_kind 9 sign-packages next "an .asc that does not verify"
echo "gpgsig $(sha256 "$RW/files/SHA256SUMS")" > "$RW/files/SHA256SUMS.asc"
rl_paused 10 sign-sums
# sign-sums
rm -f "$RW/files/SHA256SUMS.sig"
ssh-keygen -q -Y sign -f "$pf/otherkey" -n fil-release "$RW/files/SHA256SUMS"
rl_kind 10 sign-sums next "a .sig by a key not on main"
rm -f "$RW/files/SHA256SUMS.sig"
ssh-keygen -q -Y sign -f "$pf/key" -n fil-release "$RW/files/SHA256SUMS"
rl_kind 10 sign-sums "done (checked)"
rl_kind 11 upload next
# upload
for f in fil-0.9.0-1.x86_64.rpm fil-0.9.0-1.aarch64.rpm SHA256SUMS SHA256SUMS.asc; do cp "$RW/files/$f" "$DA/"; done
rl_kind 11 upload next "the .sig not uploaded"
cp "$RW/files/SHA256SUMS.sig" "$DA/"
rl_kind 11 upload "done (checked)"
rl_kind 7 verify-presign "done (implied)" "the draft moved on"
rl_kind 11 upload "done (checked)" "with a gh that reports no digests" FAKE_NO_DIGEST=1
rl_kind 12 verify-signed next
# verify-signed
printf 'sums=%s\nrebuild=1\npresign=0\ndry=0\n' "$(sha256 "$RW/files/SHA256SUMS")" > "$RW/verified"
rl_kind 12 verify-signed "done (checked)"
rl_paused 13 publish
# the state file deleted: everything is derived again
rm -f "$RSTATE"
rl_kind 12 verify-signed "done (checked)" "with no state file"
rl_kind 4 dry-verify "done (implied)" "with no state file"
rl_paused 13 publish
expect_pass "release run: ...re-derived the identifiers" sh -c 'grep -qx dry_run=101 "$1" && grep -qx tag_run=201 "$1" && grep -qx "github_commit=$2" "$1"' sh "$RSTATE" "$RG"
echo "dry_verify=ok 101 $RG 2026-11-09T12:00:00Z" >> "$RSTATE"
# 13: publish
echo '{"tag": "v0.9.0", "draft": false}' > "$rs/world/releases/1/meta.json"
rl_kind 13 publish "done (checked)"
rl_kind 14 verify-published next
# a run without a terminal does verify-published (no prompt) and pauses at the crates confirmation
rl_paused 15 crates
expect_pass "release run: ...verify-published ran first and recorded the published SHA256SUMS" grep -q "^verify_published=ok $(sha256 "$DA/SHA256SUMS") " "$RSTATE"
rl_kind 14 verify-published "done (recorded)"
# 15: crates
rl_kind 15 crates next "nothing on crates.io"
rl_kind 15 crates next "one of two crates" FAKE_CRATES_PUBLISHED=fil-shell
expect_pass "release status: ...names the missing one" grep -q 'not on crates.io at 0.9.0: fil-proto' "$rs/status.out"
rl_kind 15 crates "next (unknown)" "crates.io down" FAKE_CRATES_CODE=503
rl_kind 15 crates "done (checked)" "" FAKE_CRATES_PUBLISHED="fil-shell fil-proto"
rl_paused 16 brew FAKE_CRATES_PUBLISHED="fil-shell fil-proto"
# 16: the tap
mkdir -p "$rs/tap/Formula"
"$REPO_ROOT/packaging/render.sh" brew 0.9.0 "$RW/files/SHA256SUMS" "$rs/brew" > /dev/null 2>&1
cp "$rs/brew/fil.rb" "$rs/tap/Formula/fil.rb"
git -C "$rs/tap" add -A && git -C "$rs/tap" commit -q --no-gpg-sign -m 'fil 0.9.0'
rl_kind 16 brew next "committed, not pushed" FAKE_CRATES_PUBLISHED="fil-shell fil-proto"
git -C "$rs/tap" push -q origin main
rl_kind 16 brew "done (checked)" "" FAKE_CRATES_PUBLISHED="fil-shell fil-proto"
rl FAKE_CRATES_PUBLISHED="fil-shell fil-proto" > "$rs/run.out" 2>&1 && st=0 || st=$?
if [ $st = 0 ] && grep -q 'release: fil 0.9.0 is released' "$rs/run.out"; then pass "release run: everything through brew done: finish, exit 0"; else flunk "release run: everything through brew done: finish, exit 0 (exit $st)"; tail -8 "$rs/run.out" | sed 's/^/       | /'; fi
# the whole status, against its golden (the notes after the label are not compared)
rls FAKE_CRATES_PUBLISHED="fil-shell fil-proto" 2>&1 | awk '$1 ~ /^[0-9]+\/17$/ { k = $3; if (k == "done") k = k " " $4; print $1, $2, k }' > "$rs/golden.out"
cat > "$rs/golden" << 'EOF2'
1/17 candidate done (checked)
2/17 mirror done (checked)
3/17 dry-run done (checked)
4/17 dry-verify done (recorded)
5/17 tag done (checked)
6/17 tag-run done (checked)
7/17 verify-presign done (checked)
8/17 sign-macos done (checked)
9/17 sign-packages done (checked)
10/17 sign-sums done (checked)
11/17 upload done (checked)
12/17 verify-signed done (checked)
13/17 publish done (checked)
14/17 verify-published done (recorded)
15/17 crates done (checked)
16/17 brew done (checked)
17/17 finish next
EOF2
expect_pass "release status: the whole listing matches its golden" diff "$rs/golden" "$rs/golden.out"
expect_pass "release status: ...and it shows the keys" grep -q '^keys (tests/release/just.sh release-keys status):' "$rs/status.out"
rl_kind 3 dry-run "done (implied)" "GitHub down: the tag still verifies" FAKE_RL_GH_DOWN=1
rl_kind 6 tag-run "next (unknown)" "GitHub down" FAKE_RL_GH_DOWN=1
# a good tag, the state gone, HEAD on a later main commit: not burned; the run names the release
# commit and records nothing, and --status shows the release the tag names
rm -f "$RSTATE"
echo later > "$pf/clone/later2"
pg add later2
pg commit -q --no-gpg-sign -m later2
pg push -q origin HEAD:main
expect_fail "release run: a good tag on another tree than HEAD, nothing bound, fails" rl FAKE_CRATES_PUBLISHED="fil-shell fil-proto"
last
expect_pass "release run: ...not as burned" sh -c '! grep -qi "burned" "$1"' sh "$rs/last.log"
expect_pass "release run: ...naming the release commit to check out" grep -q "git checkout $RR" "$rs/last.log"
expect_pass "release run: ...and recording nothing" test ! -e "$RSTATE"
rl_heads() { pg rev-parse HEAD; pg symbolic-ref -q HEAD || true; pg for-each-ref --format='%(refname) %(objectname)' | LC_ALL=C sort; pg status --porcelain; }
rl_heads > "$rs/heads.before"
rl_kind 5 tag "done (checked)" "status from a later commit finds the tag's release commit" FAKE_CRATES_PUBLISHED="fil-shell fil-proto"
rl_kind 16 brew "done (checked)" "...and shows the whole release" FAKE_CRATES_PUBLISHED="fil-shell fil-proto"
rl_heads > "$rs/heads.after"
expect_pass "release status: ...showing another commit leaves HEAD and every ref unchanged" cmp -s "$rs/heads.before" "$rs/heads.after"
pg reset -q --hard "$RR"
pg push -q -f origin "$RR:main"
rl FAKE_CRATES_PUBLISHED="fil-shell fil-proto" > /dev/null 2>&1 || true
echo "dry_verify=ok 101 $RG 2026-11-09T12:00:00Z" >> "$RSTATE"
# ---- the actions of steps 5-12 (PR 5): the tag, the tag's run and its draft, the verification
# before signing, the three signing steps, the upload and the verification after it, each on the
# gh world (the github hook starts the tag's run, which the fake advances one stage per look and whose
# publish job makes the draft from $rs/files0), the step-script stubs (fake.py) and the keys stub.
# The release key is the .pub whose private half is only in the test agent (as the owner's is).
ra5_reset() {
  ra_reset
  rm -rf "$rs/world/releases" "$rs/world/next-release-id" "$RW/files" "$RW"/files.signed-* "$RW/verified" "$rs/reject-tag"
  mkdir "$rs/world/releases"
  for t in v0.9.0 v0.9.0-gh; do pg tag -d "$t" > /dev/null 2>&1 || true; done
  rm -f "$rs/keys-locked"
  touch "$rs/tag-run-on-push"
}
r5() {
  local name=$1 want=$2 pat=$3
  shift 3
  ra_expect "$name" "$want" "$pat" FIL_RELEASE_KEY="$rs/agentpub/key" "$@"
}
n5() { grep -c "$1" "$2" || true; }
# calls5_are TAG-PUSHES DRAFTS-MADE DRAFTS-DELETED UPLOADS VERIFIES MAC-SIGNINGS PACKAGE-SIGNINGS
calls5_are() {
  local got
  got="$(n5 '^push refs/tags/v0.9.0 ' "$rs/calls.log") $(n5 '^draft create ' "$rs/calls.log") $(n5 '^draft delete ' "$rs/calls.log") $(n5 '^release upload ' "$pf/gh.log") $(n_verify) $(n5 '^sign-macos ' "$rs/calls.log") $(n5 '^sign-packages ' "$rs/calls.log")"
  if [ "$got" = "$*" ]; then pass "release draft: ...calls: $1 tag push, $2 drafts made, $3 deleted, $4 uploads, $5 verifies, $6 macOS and $7 package signings"; else
    flunk "release draft: ...calls: wanted $*; got $got (tag pushes, drafts made, deleted, uploads, verifies, macOS, package signings)"
  fi
}
RK=$rs/agentpub/key.pub
st5() { rls FIL_RELEASE_KEY="$rs/agentpub/key" 2>&1 | awk '$1 ~ /^[0-9]+\/17$/ { k = $3; if (k == "done") k = k " " $4; print $1, $2, k }'; }

# the happy path, in one invocation from the start (keys locked: unlocked once, up front), with
# Gitea's marker tag owning v0.9.0 in the clone (the local name is then v0.9.0-gh)
ra5_reset
: > "$rs/keys-locked"
pg tag -a -m marker v0.9.0 "$RR"
r5 "release draft: the happy path, one invocation from the start through verify-signed, paused at 13 publish" 2 'PAUSED at step 13/17 publish' <<< "0.9.0"
calls_are 1 3 0 3
calls5_are 1 1 0 1 3 1 1
expect_pass "release draft: ...one tag confirmation (it covers the mirror and the tag), then the publish one, unanswered" sh -c 'test "$(grep -c "release: CONFIRM " "$1")" = 2 && test "$(grep -c "release: CONFIRM tag" "$1")" = 1 && grep -q "release: CONFIRM publish" "$1"' sh "$rs/ra.out"
expect_pass "release draft: ...the keys unlocked once, up front, all three" sh -c 'test "$(grep -c "^keys unlock " "$1")" = 1 && grep -qx "keys unlock --need ssh,gpg,apple --ttl 8h" "$1" && test "$(grep -n "^keys unlock" "$1" | cut -d: -f1)" -lt "$(grep -n "^push refs/heads/main" "$1" | cut -d: -f1)"' sh "$rs/calls.log"
RG5=$(git --git-dir="$pf/github.git" rev-parse refs/heads/main)
git init -q --bare "$rs/tagchk"
git -C "$rs/tagchk" fetch -q "$pf/github.git" "+refs/tags/v0.9.0:refs/tags/v0.9.0" "+refs/heads/main:refs/remotes/origin/main"
expect_pass "release draft: ...the tag on GitHub verifies against the signers on main" "$RELEASE_DIR/verify-tag.sh" "$rs/tagchk" v0.9.0
expect_pass "release draft: ...names the release commit, and is the local v0.9.0-gh (Gitea's marker kept v0.9.0)" sh -c 'test "$(git -C "$1" rev-parse "refs/tags/v0.9.0^{commit}")" = "$2" && test "$(git -C "$3" rev-parse refs/tags/v0.9.0-gh)" = "$(git -C "$1" rev-parse refs/tags/v0.9.0)" && test "$(git -C "$3" rev-parse "refs/tags/v0.9.0^{commit}")" = "$4" && test "$2" = "$4"' sh "$rs/tagchk" "$RG5" "$pf/clone" "$RR"
rm -rf "$rs/tagchk"
expect_pass "release draft: ...recorded the tag object and the tag's run" sh -c 'grep -qx "tag_object=$(git -C "$2" rev-parse refs/tags/v0.9.0-gh)" "$1" && grep -qx tag_run=104 "$1"' sh "$RSTATE" "$pf/clone"
expect_pass "release draft: ...verified before signing and after, the second reusing the rebuild" sh -c 'grep -qx "verify 0.9.0 --pre-sign --repo o/r" "$1" && grep -qx "verify 0.9.0 --repo o/r --reuse-rebuild" "$1" && grep -qx "sign-macos 0.9.0 --repo o/r" "$1" && grep -qx "sign-packages 0.9.0 $2" "$1"' sh "$rs/calls.log" "$RW/files"
expect_pass "release draft: ...uploaded the five signed files, once" grep -qx 'release upload v0.9.0 --repo o/r --clobber fil-0.9.0-1.x86_64.rpm fil-0.9.0-1.aarch64.rpm SHA256SUMS SHA256SUMS.asc SHA256SUMS.sig' "$pf/gh.log"
expect_pass "release draft: ...SHA256SUMS.sig verifies with the release key" sh -c 'printf "release namespaces=\"fil-release\" %s\n" "$(cut -d" " -f1,2 "$1")" > "$3.signers"; ssh-keygen -Y verify -f "$3.signers" -I release -n fil-release -s "$2/SHA256SUMS.sig" < "$2/SHA256SUMS" > /dev/null 2>&1' sh "$RK" "$rs/world/releases/1/assets" "$rs/tmp5"
expect_pass "release draft: ...one log per step that acted" sh -c 'for n in 05-tag 06-tag-run 07-verify-presign 08-sign-macos 09-sign-packages 10-sign-sums 11-upload 12-verify-signed; do test -s "$1/$n.log" || exit 1; done' sh "$RW/logs"
st5 > "$rs/golden5.out"
cat > "$rs/golden5" << 'EOF2'
1/17 candidate done (checked)
2/17 mirror done (checked)
3/17 dry-run done (checked)
4/17 dry-verify done (recorded)
5/17 tag done (checked)
6/17 tag-run done (checked)
7/17 verify-presign done (checked)
8/17 sign-macos done (checked)
9/17 sign-packages done (checked)
10/17 sign-sums done (checked)
11/17 upload done (checked)
12/17 verify-signed done (checked)
13/17 publish next
14/17 verify-published pending
15/17 crates pending
16/17 brew pending
17/17 finish pending
EOF2
expect_pass "release status: the listing after verify-signed matches its golden" diff "$rs/golden5" "$rs/golden5.out"
r5 "release draft: the same command again does nothing new" 2 'PAUSED at step 13/17 publish' < /dev/null
calls5_are 1 1 0 1 3 1 1
rl -- --lock-keys > "$rs/lk.out" 2>&1 && st=0 || st=$?
expect_pass "release draft: --lock-keys forgets the keys (release-keys.sh forget), exit 0, and acts on nothing" sh -c 'test "$3" = 0 && grep -qx "keys forget --need ssh,gpg" "$1" && test "$(grep -c "^push " "$1")" = 2 && ! grep -q "PAUSED\|FAILED" "$2"' sh "$rs/calls.log" "$rs/lk.out" "$st"
rm -f "$rs/keys-locked"

# a failure injected at each of steps 5-12, then the same command again: each resumes at its step,
# and the counts show no action done twice (F7: a run that did not confirm `tag` itself asks again)
ra5_reset
touch "$rs/reject-tag"
r5 "release draft: a refused tag push fails at 5" 1 'FAILED at step 5/17 tag' <<< "0.9.0"
expect_pass "release draft: ...with the known fix for the tag ruleset" grep -q 'the v\* tag ruleset' "$rs/ra.out"
calls5_are 0 0 0 0 1 0 0
LT=$(pg rev-parse refs/tags/v0.9.0-gh)
expect_pass "release draft: ...the local tag is v0.9.0-gh even with no marker in the clone (v0.9.0 is left to Gitea's)" sh -c '! git -C "$1" rev-parse -q --verify refs/tags/v0.9.0 > /dev/null' sh "$pf/clone"
rm -f "$rs/reject-tag"
r5 "release draft: ...the same command again asks for the tag again (the confirmation is not kept), EOF: paused" 2 'PAUSED at step 5/17 tag' < /dev/null
expect_pass "release draft: ...it showed the tag's confirmation" sh -c 'grep -q "CONFIRM tag, for 0.9.0" "$1" && grep -q "step 5 tag: the tag v0.9.0 on the release commit" "$1"' sh "$rs/ra.out"
expect_pass "release draft: ...and nothing in the state says it was confirmed" sh -c '! grep -qi "confirm" "$1"' sh "$RSTATE"
calls5_are 0 0 0 0 1 0 0
r5 "release draft: ...confirmed: the tag is pushed; its publish fails, the rerun declined: fails at 6" 1 'FAILED at step 6/17 tag-run' FAKE_TAG_RUN_FAIL=publish <<< "$(printf '0.9.0\nn')"
expect_pass "release draft: ...the tag pushed is the one the refused run made (reused, not made again)" sh -c 'test "$(git --git-dir="$1" rev-parse refs/tags/v0.9.0)" = "$2" && grep -q "reusing the local tag v0.9.0-gh" "$3"' sh "$pf/github.git" "$LT" "$RW/logs/05-tag.log"
expect_pass "release draft: ...the rerun offer named the stale draft" grep -q 'deleting first the stale draft(s) 1 that the failed attempt left' "$rs/ra.out"
calls5_are 1 1 0 0 1 0 0
r5 "release draft: ...again, the rerun accepted: the stale draft deleted first; then the VM is down at 7" 1 'FAILED at step 7/17 verify-presign' FAKE_TAG_RUN_FAIL=publish FAKE_VERIFY_FAIL=lima <<< "y"
expect_pass "release draft: ...one release of the tag left, the second draft" sh -c 'test "$(ls "$1")" = 2' sh "$rs/world/releases"
calls_are 1 3 1 2
calls5_are 1 2 1 0 2 0 0
r5 "release draft: ...again: Apple rejects the notarization at 8" 1 'FAILED at step 8/17 sign-macos' FAKE_SIGN_FAIL=macos < /dev/null
expect_pass "release draft: ...with the known fix" grep -q 'Apple rejected the notarization' "$rs/ra.out"
calls5_are 1 2 1 0 3 1 0
r5 "release draft: ...again: the package key has no passphrase cached at 9" 1 'FAILED at step 9/17 sign-packages' FAKE_SIGN_FAIL=packages < /dev/null
expect_pass "release draft: ...with the known fix" grep -q "the package key's passphrase is not cached" "$rs/ra.out"
calls5_are 1 2 1 0 3 2 1
SSH_AUTH_SOCK=$rs/agent.sock ssh-add -q -d "$RK" 2> /dev/null
r5 "release draft: ...again: the release key left the agent, sign-sums fails at 10" 1 'FAILED at step 10/17 sign-sums' < /dev/null
expect_pass "release draft: ...with the known fix" grep -q 'the release key is not in the agent' "$rs/ra.out"
expect_pass "release draft: ...and no .sig is left behind" test ! -e "$RW/files/SHA256SUMS.sig"
SSH_AUTH_SOCK=$rs/agent.sock ssh-add -q "$rs/agentkey/key" 2> /dev/null
calls5_are 1 2 1 0 3 2 2
r5 "release draft: ...again: the upload fails part way at 11" 1 'FAILED at step 11/17 upload' FAKE_UPLOAD_FAIL=1 < /dev/null
calls5_are 1 2 1 1 3 2 2
r5 "release draft: ...again: only what still differs is uploaded; the VM is down at 12" 1 'FAILED at step 12/17 verify-signed' FAKE_VERIFY_FAIL=lima < /dev/null
expect_pass "release draft: ...the second upload left out the file the first got up" sh -c 'grep "^release upload " "$1" | tail -n 1 | grep -qx "release upload v0.9.0 --repo o/r --clobber fil-0.9.0-1.aarch64.rpm SHA256SUMS SHA256SUMS.asc SHA256SUMS.sig"' sh "$pf/gh.log"
calls5_are 1 2 1 2 4 2 2
r5 "release draft: ...again: verify-signed passes, paused at 13" 2 'PAUSED at step 13/17 publish' < /dev/null
calls5_are 1 2 1 2 5 2 2
r5 "release draft: ...and once more: nothing is repeated" 2 'PAUSED at step 13/17 publish' < /dev/null
calls_are 1 3 1 5
calls5_are 1 2 1 2 5 2 2
st5 > "$rs/golden5.out"
expect_pass "release status: ...the same listing as the happy path's" diff "$rs/golden5" "$rs/golden5.out"

# --redo verify-presign once the draft is signed: refused (a pre-sign check of signed rpms can only
# fail, and its fix would burn a good version)
r5 "release redo: --redo verify-presign after the upload is refused" 1 '--redo verify-presign: the draft is signed already' -- --redo verify-presign < /dev/null
expect_pass "release redo: ...pointing to --redo verify-signed" grep -q 'redo verify-signed re-verifies the signed draft' "$rs/ra.out"
calls5_are 1 2 1 2 5 2 2
# --redo: the step and the ones that consume its output, each once more; nothing else
r5 "release redo: --redo sign-packages: sign-packages, sign-sums, upload and verify-signed again" 2 'PAUSED at step 13/17 publish' -- --redo sign-packages < /dev/null
calls5_are 1 2 1 3 6 2 3
expect_pass "release redo: ...the upload sent all five" sh -c 'grep "^release upload " "$1" | tail -n 1 | grep -qx "release upload v0.9.0 --repo o/r --clobber fil-0.9.0-1.x86_64.rpm fil-0.9.0-1.aarch64.rpm SHA256SUMS SHA256SUMS.asc SHA256SUMS.sig"' sh "$pf/gh.log"
r5 "release redo: --redo sign-macos signs again with --force, and everything after it" 2 'PAUSED at step 13/17 publish' -- --redo sign-macos < /dev/null
calls5_are 1 2 1 4 7 3 4
expect_pass "release redo: ...sign-macos got --force" sh -c 'grep "^sign-macos " "$1" | tail -n 1 | grep -qx "sign-macos 0.9.0 --repo o/r --force"' sh "$rs/calls.log"
r5 "release redo: --redo verify-signed verifies again, and only that" 2 'PAUSED at step 13/17 publish' -- --redo verify-signed < /dev/null
calls5_are 1 2 1 4 8 3 4
r5 "release redo: --redo dry-run after the tag is refused" 1 '--redo dry-run: v0.9.0 is on GitHub' -- --redo dry-run < /dev/null
echo '{"tag": "v0.9.0", "draft": false}' > "$rs/world/releases/2/meta.json"
r5 "release redo: --redo upload of a published release is refused" 1 '--redo upload: v0.9.0 is not a draft' -- --redo upload < /dev/null
echo '{"tag": "v0.9.0", "draft": true, "created": "2099-01-01T01:02:00Z"}' > "$rs/world/releases/2/meta.json"
calls5_are 1 2 1 4 8 3 4
st5 > "$rs/golden5.out"
expect_pass "release status: ...after the redos, the same listing" diff "$rs/golden5" "$rs/golden5.out"

# the double-draft trap after a successful publish: the newest by id (GitHub's created_at is the
# tagged commit's date) is kept if it has every release file, an older draft is deleted after a
# [y/N]; a published release is never deleted
mkdir -p "$rs/world/releases/1/assets"
echo '{"tag": "v0.9.0", "draft": true, "created": "2099-12-31T00:00:00Z"}' > "$rs/world/releases/1/meta.json"
cp "$rs/files0/SHA256SUMS" "$rs/world/releases/1/assets/"
rl_kind 6 tag-run next "two drafts" FIL_RELEASE_KEY="$rs/agentpub/key"
r5 "release stale draft: no terminal: paused at 6, nothing deleted" 2 'PAUSED at step 6/17 tag-run' FIL_RELEASE_TTY= < /dev/null
r5 "release stale draft: declined: paused, nothing deleted" 2 'stale draft\(s\) 1 not deleted' <<< "n"
expect_pass "release stale draft: ...both drafts still there" test -d "$rs/world/releases/1"
r5 "release stale draft: accepted: the older id deleted (whatever its date), the newest kept; on to 13" 2 'PAUSED at step 13/17 publish' <<< "y"
expect_pass "release stale draft: ...only the newest is left" sh -c 'test "$(ls "$1")" = 2' sh "$rs/world/releases"
calls5_are 1 2 2 4 8 3 4
mkdir -p "$rs/world/releases/9/assets"
echo '{"tag": "v0.9.0", "draft": true}' > "$rs/world/releases/9/meta.json"
cp "$rs/files0/SHA256SUMS" "$rs/world/releases/9/assets/"
r5 "release stale draft: the newest draft is incomplete: fails, deleting nothing" 1 'the newest \(9\) lacks:' <<< "y"
expect_pass "release stale draft: ...both still there" sh -c 'test -d "$1/9" && test -d "$1/2"' sh "$rs/world/releases"
rm -rf "$rs/world/releases/9"
mkdir -p "$rs/world/releases/1/assets"
echo '{"tag": "v0.9.0", "draft": false}' > "$rs/world/releases/1/meta.json"
r5 "release stale draft: a published release beside the draft: fails, deleting nothing" 1 'one of them published' <<< "y"
expect_pass "release stale draft: ...both still there" sh -c 'test -d "$1/1" && test -d "$1/2"' sh "$rs/world/releases"
rm -rf "$rs/world/releases/1"
calls5_are 1 2 2 4 8 3 4

cp "$pf/gh.log" "$rs/gh5.log"
# the waiver of red suite=all checks (F5): offered only at a terminal, only with the dry run green,
# recorded with the run ids and attempts, and honored for exactly those
ra5_reset
r5 "release waiver: a red linux-check, the rerun declined, waived: on to the tag" 2 'PAUSED at step 5/17 tag' FAKE_RUN_FAIL=linux-check.yml -- --confirm-each <<< "$(printf '0.9.0\nn\ny')"
expect_pass "release waiver: ...it asked whether they block the release" grep -q 'waive them, i.e. they do NOT block the release' "$rs/ra.out"
expect_pass "release waiver: ...recorded the failed run and its attempt, nothing for macos-check" grep -Eqx 'checks_waived=102\.1 - [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z' "$RSTATE"
calls_are 1 3 0 1
rl_kind 3 dry-run "done (waived)"
expect_pass "release waiver: ...--status names the waived run" grep -q 'WAIVED by you .*linux-check run 102' "$rs/status.out"
rl_kind 4 dry-verify "done (recorded)" "with the waiver"
r5 "release waiver: ...the tag's confirmation names it" 2 'the red suite=all checks you waived: 102\.1 -' < /dev/null
cp "$RSTATE" "$rs/state.w"
sed 's/^checks_waived=102\.1 /checks_waived=999.1 /' "$rs/state.w" > "$RSTATE"
rl_kind 3 dry-run next "a waiver of another run does not count"
rl_kind 4 dry-verify pending "...nor does the dry-verify record without it"
sed 's/^checks_waived=102\.1 - /checks_waived=- 102.1 /' "$rs/state.w" > "$RSTATE"
rl_kind 3 dry-run next "a waiver naming the run for the other workflow does not count"
r5 "release waiver: ...and a run without a terminal pauses at 3 for the decision, not at the tag" 2 'PAUSED at step 3/17 dry-run' FIL_RELEASE_TTY= < /dev/null
cp "$rs/state.w" "$RSTATE"
python3 -I -c 'import json, sys; r = json.load(open(sys.argv[1])); r["attempt"] = 2; json.dump(r, open(sys.argv[1], "w"))' "$rs/world/runs/102.json"
rl_kind 3 dry-run next "the waived run failed again in a later attempt: not waived"
python3 -I -c 'import json, sys; r = json.load(open(sys.argv[1])); r["attempt"] = 1; json.dump(r, open(sys.argv[1], "w"))' "$rs/world/runs/102.json"
rl_kind 3 dry-run "done (waived)" "...back at the waived attempt"
st5 | sed -n '3,5p' > "$rs/goldenw.out"
printf '3/17 dry-run done (waived)\n4/17 dry-verify done (recorded)\n5/17 tag next\n' > "$rs/goldenw"
expect_pass "release status: the waiver's listing matches its golden" diff "$rs/goldenw" "$rs/goldenw.out"
# --redo dry-run before the tag: new runs, the waiver and dry-verify forgotten
r5 "release redo: --redo dry-run before the tag: three new runs and a new dry-verify" 2 'PAUSED at step 5/17 tag' -- --redo dry-run < /dev/null
calls_are 1 6 0 2
expect_pass "release redo: ...the waiver is gone and the new runs are recorded" sh -c '! grep -q "^checks_waived=" "$1" && grep -qx dry_run=104 "$1" && grep -qx checks_linux=105 "$1" && grep -q "^dry_verify=ok 104 " "$1"' sh "$RSTATE"
rl_kind 3 dry-run "done (checked)" "after --redo"
r5 "release redo: --redo upload before the tag: nothing to redo" 1 '--redo upload: the step is not done' -- --redo upload < /dev/null
# the waiver in the default mode: the tag confirmation given before the mirror did not know of it,
# so step 5 asks again, listing it
ra5_reset
r5 "release waiver: default mode: waived after the tag confirmation, which step 5 then asks again" 2 'PAUSED at step 5/17 tag' FAKE_RUN_FAIL=linux-check.yml <<< "$(printf '0.9.0\nn\ny')"
expect_pass "release waiver: ...two tag confirmations, the second naming the waiver, nothing pushed" sh -c 'test "$(grep -c "CONFIRM tag, for 0.9.0" "$1")" = 2 && grep -q "the red suite=all checks you waived: 102\.1 -" "$1" && ! grep -q "^push refs/tags/" "$2"' sh "$rs/ra.out" "$rs/calls.log"
# a matrix leg of the tag's run fails while the run goes on: waited for, the rerun offered once it
# has completed
r5 "release tag-run: a leg fails mid-run: waited for, then the rerun declined: fails at 6" 1 'FAILED at step 6/17 tag-run' FAKE_TAG_RUN_FAIL=leg <<< "$(printf '0.9.0\nn')"
expect_pass "release tag-run: ...it said it waits for the run to finish, before offering the rerun" sh -c 'a=$(grep -n "waiting for the run to finish before offering a rerun" "$1" | cut -d: -f1 | head -n 1); b=$(grep -n "re-run its failed jobs" "$1" | cut -d: -f1 | head -n 1); test -n "$a" && test -n "$b" && test "$a" -lt "$b"' sh "$rs/ra.out"
r5 "release tag-run: ...the rerun accepted (of a completed run): on to 13" 2 'PAUSED at step 13/17 publish' <<< "y"
calls_are 1 3 1 3
# no terminal at the red checks: paused (exit 2), nothing waived
ra5_reset
r5 "release waiver: a red check, no answer at the prompts: failed at 3, nothing waived" 1 'FAILED at step 3/17 dry-run' FAKE_RUN_FAIL=linux-check.yml -- --confirm-each <<< "0.9.0"
expect_pass "release waiver: ...said not waived" grep -q 'not re-run, not waived' "$rs/ra.out"
r5 "release waiver: ...again without a terminal: paused, exit 2" 2 'PAUSED at step 3/17 dry-run' FIL_RELEASE_TTY= < /dev/null
expect_pass "release waiver: ...no prompt shown, nothing recorded" sh -c '! grep -q "waive them" "$1" && ! grep -q "^checks_waived=" "$2"' sh "$rs/ra.out" "$RSTATE"
# a red dry run (release.yml) is never offered the waiver
ra5_reset
r5 "release waiver: a red release.yml dry run: failed at 3, no waiver offered" 1 'FAILED at step 3/17 dry-run' FAKE_RUN_FAIL=release.yml -- --confirm-each <<< "$(printf '0.9.0\nn\ny')"
expect_pass "release waiver: ...no prompt shown, nothing recorded" sh -c '! grep -q "waive them" "$1" && ! grep -q "^checks_waived=" "$2"' sh "$rs/ra.out" "$RSTATE"
rm -f "$rs/tag-run-on-push"

# ---- the actions of steps 13-17 (PR 6): publish (the second confirmation, asked again on a resume),
# verify-published, the crates-io deployment approved through pending_deployments and its job (the
# fake job uploads the crates crates.io lacks, $rs/world/crates), the tap (its bare remote logs each
# push and refuses it while $rs/reject-tap exists; the clone commits with the test agent's key),
# Homebrew's install test (the brew stub) and finish. Each case starts from nothing done.
git -C "$rs/tap" config gpg.format ssh
git -C "$rs/tap" config user.signingkey "$rs/agentpub/key.pub"
: > "$rs/gh6.log"
r6_reset() {
  cat "$pf/gh.log" >> "$rs/gh6.log"
  ra5_reset
  rm -rf "$rs/world/crates" "$rs/brewprefix" "$rs/shared" "$rs/reject-tap" "$pf/clone/target/packaging"
  mkdir -p "$rs/shared"
  git --git-dir="$rs/tap.git" update-ref refs/heads/main "$TAP0"
  git -C "$rs/tap" checkout -q main
  git -C "$rs/tap" reset -q --hard "$TAP0"
  git -C "$rs/tap" fetch -q origin
}
# calls6_are PUBLISHES APPROVALS CRATE-UPLOADS TAP-PUSHES
calls6_are() {
  local got
  got="$(n5 '^release publish ' "$rs/calls.log") $(n5 '^approve ' "$rs/calls.log") $(n5 '^crate upload ' "$rs/calls.log") $(n5 '^tap push ' "$rs/calls.log")"
  if [ "$got" = "$*" ]; then pass "release publish: ...calls: $1 publish, $2 approvals, $3 crate uploads, $4 tap pushes"; else
    flunk "release publish: ...calls: wanted $*; got $got (publishes, approvals, crate uploads, tap pushes)"
  fi
}
n_confirm() { grep -c "release: CONFIRM $1, for 0.9.0" "$rs/ra.out" || true; }
TR6() { sed -n 's/^tag_run=//p' "$RSTATE"; }
set_run() { # ID KEY=JSON...: change the fake run ID
  python3 -I -c 'import json, sys
p = sys.argv[1]
r = json.load(open(p))
for kv in sys.argv[2:]:
    k, v = kv.split("=", 1)
    r[k] = json.loads(v)
json.dump(r, open(p, "w"))' "$rs/world/runs/$1.json" "${@:2}"
}

# the happy path, all seventeen steps in one invocation from the start: two confirmations
r6_reset
: > "$rs/keys-locked"
r5 "release publish: the happy path, one invocation from the start through finish: exit 0" 0 '^release: fil 0.9.0 is released$' <<< "$(printf '0.9.0\n0.9.0')"
calls_are 1 3 0 4
calls5_are 1 1 0 1 4 1 1
calls6_are 1 1 2 1
cp "$rs/ra.out" "$rs/happy6.out"
expect_pass "release publish: ...two confirmations: tag, then publish, which covers crates and brew" sh -c 'test "$(grep -c "release: CONFIRM " "$1")" = 2 && grep -q "release: CONFIRM tag, for 0.9.0" "$1" && grep -q "release: CONFIRM publish, for 0.9.0" "$1" && grep -q "step 15 crates: .*(this answer covers it too)" "$1" && grep -q "step 16 brew: .*(this answer covers it too)" "$1"' sh "$rs/ra.out"
expect_pass "release publish: ...the release is published" grep -q '"draft": false' "$rs/world/releases/1/meta.json"
expect_pass "release publish: ...verify-published ran on what is published and is recorded with its SHA256SUMS" sh -c 'grep -qx "verify 0.9.0 --repo o/r --no-rebuild" "$1" && grep -q "^verify_published=ok $3 " "$2"' sh "$rs/calls.log" "$RSTATE" "$(sha256 "$rs/world/releases/1/assets/SHA256SUMS")"
expect_pass "release publish: ...approved the crates-io environment of the tag's run, once" sh -c 'test "$(grep -c "^api -X POST " "$1")" = 1 && grep -qx "api -X POST repos/o/r/actions/runs/$2/pending_deployments -F environment_ids\[\]=7 -f state=approved -f comment=approved by just release 0.9.0, after its typed confirmation" "$1"' sh "$pf/gh.log" "$(TR6)"
expect_pass "release publish: ...the tap's main has the formula of the published SHA256SUMS, in a commit signed by you" sh -c 'packaging=$1/packaging; "$packaging/render.sh" brew 0.9.0 "$2/SHA256SUMS" "$3/brew6" > /dev/null 2>&1 && git --git-dir="$4" show main:Formula/fil.rb | cmp -s - "$3/brew6/fil.rb" && git --git-dir="$4" -c gpg.ssh.allowedSignersFile="$3/allowed" verify-commit main 2> /dev/null && test "$(git --git-dir="$4" log -1 --format=%s main)" = "fil 0.9.0"' sh "$REPO_ROOT" "$rs/world/releases/1/assets" "$rs" "$rs/tap.git"
expect_pass "release publish: ...the install test: brew update, then install (not installed yet), and fil --version" sh -c 'grep -qx "brew update" "$1" && grep -qx "brew install o/tap/fil" "$1" && grep -q "fil 0.9.0 (wire" "$2"' sh "$rs/calls.log" "$RW/logs/16-brew.log"
expect_pass "release publish: ...finish: the AUR PKGBUILD, and the coordinator's copy (directory 755, files 644)" sh -c 'test -s "$1/target/packaging/aur/PKGBUILD" && cmp -s "$2/SHA256SUMS" "$3/SHA256SUMS" && cmp -s "$2/SHA256SUMS.sig" "$3/SHA256SUMS.sig" && test "$(python3 -I -c "import os, sys; print(\" \".join(oct(os.stat(p).st_mode & 0o777) for p in sys.argv[1:]))" "$2" "$2/SHA256SUMS" "$2/SHA256SUMS.sig")" = "0o755 0o644 0o644"' sh "$pf/clone" "$rs/shared/fil-0.9.0" "$rs/world/releases/1/assets"
expect_pass "release publish: ...the keys forgotten before the publish, and again by finish, last" sh -c 'f=$(grep -n "^keys forget " "$1" | head -n 1 | cut -d: -f1); p=$(grep -n "^release publish " "$1" | cut -d: -f1); test -n "$f" && test -n "$p" && test "$f" -lt "$p" && test "$(grep "^keys " "$1" | tail -n 1)" = "keys forget --need ssh,gpg" && test "$(grep -c "^keys forget " "$1")" = 2' sh "$rs/calls.log"
expect_pass "release publish: ...the install test recorded with the tested formula, and in the summary" sh -c 'grep -q "^brew_tested=ok $(git --git-dir="$2" show main:Formula/fil.rb | (sha256sum 2> /dev/null || shasum -a 256) | cut -d" " -f1) " "$1" && grep -q "install test *passed" "$3"' sh "$RSTATE" "$rs/tap.git" "$rs/ra.out"
expect_pass "release publish: ...the summary names the release" grep -q 'release *https://github.com/o/r/releases/tag/v0.9.0' "$rs/ra.out"
expect_pass "release publish: ...one log per step that acted" sh -c 'for n in 13-publish 14-verify-published 15-crates 16-brew 17-finish; do test -s "$1/$n.log" || exit 1; done' sh "$RW/logs"
st5 > "$rs/golden6.out"
sed 's/^13\/17 publish next$/13\/17 publish done (checked)/; s/^14\/17 verify-published pending$/14\/17 verify-published done (recorded)/; s/^15\/17 crates pending$/15\/17 crates done (checked)/; s/^16\/17 brew pending$/16\/17 brew done (checked)/; s/^17\/17 finish pending$/17\/17 finish next/' "$rs/golden5" > "$rs/golden6"
expect_pass "release status: the listing after finish matches its golden" diff "$rs/golden6" "$rs/golden6.out"
r5 "release publish: the same command again: only finish again (nothing public), exit 0" 0 'fil 0.9.0 is released' < /dev/null
expect_pass "release publish: ...with no confirmation" sh -c '! grep -q "release: CONFIRM" "$1"' sh "$rs/ra.out"
calls5_are 1 1 0 1 4 1 1
calls6_are 1 1 2 1
# --redo: verify-published and brew again, each only itself; publish, crates and finish refused
r5 "release redo: --redo verify-published verifies the published release again" 0 'is released' -- --redo verify-published < /dev/null
calls5_are 1 1 0 1 5 1 1
r5 "release redo: --redo brew: the tap has the formula, so no confirmation and no push: the install test again" 0 'is released' -- --redo brew < /dev/null
expect_pass "release redo: ...no confirmation, and brew upgraded (installed already)" sh -c '! grep -q "release: CONFIRM" "$1" && grep -qx "brew upgrade o/tap/fil" "$2"' sh "$rs/ra.out" "$rs/calls.log"
calls6_are 1 1 2 1
# F1: the tap's formula is judged by its version and checksums, and never overwritten by a release
# of the same or an older version. tap_put PATH FILE: a commit on the tap's main (not a push: the hook
# does not log it) that sets PATH to FILE's content
tap_put() {
  local b t c
  b=$(git --git-dir="$rs/tap.git" hash-object -w "$2")
  GIT_INDEX_FILE=$rs/tap.idx git --git-dir="$rs/tap.git" read-tree main
  GIT_INDEX_FILE=$rs/tap.idx git --git-dir="$rs/tap.git" update-index --add --cacheinfo "100644,$b,$1"
  t=$(GIT_INDEX_FILE=$rs/tap.idx git --git-dir="$rs/tap.git" write-tree)
  rm -f "$rs/tap.idx"
  c=$(git --git-dir="$rs/tap.git" commit-tree "$t" -p main -m "edit $1")
  git --git-dir="$rs/tap.git" update-ref refs/heads/main "$c"
  git -C "$rs/tap" fetch -q origin
  echo "$c"
}
git --git-dir="$rs/tap.git" show main:Formula/fil.rb > "$rs/tap-v.rb"
sed 's/^  desc ".*"$/  desc "fixed in the tap"/' "$rs/tap-v.rb" > "$rs/tap-fixed.rb"
tap_put Formula/fil.rb "$rs/tap-fixed.rb" > /dev/null
rl_kind 16 brew "done (checked)" "the tap's formula fixed by hand: still 0.9.0 with the published checksums" FIL_RELEASE_KEY="$rs/agentpub/key"
r5 "release brew: ...a plain run: nothing pushed over the fix" 0 'fil 0.9.0 is released' < /dev/null
r5 "release brew: ...--redo brew: the install test only, nothing pushed over the fix" 0 'fil 0.9.0 is released' -- --redo brew < /dev/null
expect_pass "release brew: ...no confirmation, and the fix is still the tap's formula" sh -c '! grep -q "release: CONFIRM" "$1" && git --git-dir="$2" show main:Formula/fil.rb | cmp -s - "$3"' sh "$rs/ra.out" "$rs/tap.git" "$rs/tap-fixed.rb"
calls6_are 1 1 2 1
expect_pass "release brew: ...the summary says the fixed formula's install test passed" grep -q 'install test *passed' "$rs/ra.out"
sed 's/0\.9\.0/0.9.1/g' "$rs/world/releases/1/assets/SHA256SUMS" > "$rs/sums091"
"$REPO_ROOT/packaging/render.sh" brew 0.9.1 "$rs/sums091" "$rs/brew091" > /dev/null 2>&1
tap_put Formula/fil.rb "$rs/brew091/fil.rb" > /dev/null
r5 "release brew: the tap at a newer version (0.9.1): fails at 16, no downgrade" 1 'version 0\.9\.1, newer than 0\.9\.0: it is not changed' < /dev/null
expect_pass "release brew: ...saying the tap has moved past it, not to --redo brew (refused there)" sh -c 'grep -q "it has moved past 0.9.0" "$1" && ! grep -q -- "--redo brew" "$1"' sh "$rs/ra.out"
python3 -I -c 'import re, sys
t = open(sys.argv[1]).read()
open(sys.argv[2], "w").write(re.sub(r"sha256 \"[0-9a-f]{64}\"", "sha256 \"" + "0" * 64 + "\"", t, count=1))' "$rs/tap-v.rb" "$rs/tap-badsum.rb"
tap_put Formula/fil.rb "$rs/tap-badsum.rb" > /dev/null
r5 "release brew: the tap at 0.9.0 with other checksums: fails at 16, not overwritten" 1 'names 0\.9\.0, but not with the published SHA256SUMS.s checksums: it is not changed' < /dev/null
calls6_are 1 1 2 1
tap_put Formula/fil.rb "$rs/tap-v.rb" > /dev/null
# F2: the coordinator's directory is a symlink: refused at 17, its target untouched; a symlink planted
# in a directory of ours is removed, not followed
rm -rf "$rs/shared/fil-0.9.0" "$rs/elsewhere"
mkdir -p "$rs/elsewhere"
ln -s "$rs/elsewhere" "$rs/shared/fil-0.9.0"
r5 "release finish: the coordinator's directory is a symlink: fails at 17" 1 'fil-0\.9\.0 exists and is not a directory of yours' < /dev/null
expect_pass "release finish: ...nothing written through it" sh -c 'test -z "$(ls -A "$1")"' sh "$rs/elsewhere"
rm -f "$rs/shared/fil-0.9.0"
mkdir -p "$rs/shared/fil-0.9.0"
echo "the owner's file" > "$rs/victim"
ln -s "$rs/victim" "$rs/shared/fil-0.9.0/SHA256SUMS"
r5 "release finish: a symlink planted in the coordinator's directory: replaced, exit 0" 0 'fil 0.9.0 is released' < /dev/null
expect_pass "release finish: ...its target untouched, the copy a regular file" sh -c 'test "$(cat "$1")" = "the owner'"'"'s file" && test ! -L "$2/SHA256SUMS" && cmp -s "$2/SHA256SUMS" "$3/SHA256SUMS"' sh "$rs/victim" "$rs/shared/fil-0.9.0" "$rs/world/releases/1/assets"

# a failure injected at each of steps 13-17, then the same command again: each resumes at its step,
# asks for the confirmation again (never kept), and the counts show no action done twice
r6_reset
r5 "release publish: GitHub fails the publish: fails at 13" 1 'FAILED at step 13/17 publish' FAKE_PUBLISH_FAIL=1 <<< "$(printf '0.9.0\n0.9.0')"
expect_pass "release publish: ...with the known fix" grep -q 'GitHub or the network failed for a moment' "$rs/ra.out"
calls6_are 0 0 0 0
r5 "release publish: ...the same command again asks for publish again (the answer is not kept), EOF: paused at 13" 2 'PAUSED at step 13/17 publish' < /dev/null
expect_pass "release publish: ...it showed the publish confirmation, covering crates and brew" sh -c 'grep -q "release: CONFIRM publish, for 0.9.0" "$1" && grep -q "step 13 publish: the draft v0.9.0" "$1" && grep -q "step 15 crates: .*covers it too" "$1" && ! grep -q "CONFIRM tag" "$1"' sh "$rs/ra.out"
expect_pass "release publish: ...and nothing in the state says it was confirmed" sh -c '! grep -qi "confirm" "$1"' sh "$RSTATE"
calls6_are 0 0 0 0
r5 "release publish: ...confirmed: published; then the published release does not verify: a loud stop at 14" 1 'FAILED at step 14/17 verify-published' FAKE_VERIFY_FAIL=mismatch <<< "0.9.0"
expect_pass "release publish: ...saying not to go on to crates.io or the tap" grep -q 'STOP: the PUBLISHED release did not verify' "$rs/ra.out"
expect_pass "release publish: ...recording nothing for it" sh -c '! grep -q "^verify_published=" "$1"' sh "$RSTATE"
calls6_are 1 0 0 0
r5 "release crates: ...again: verified; the crates-io job fails after one crate, the rerun declined: fails at 15" 1 'FAILED at step 15/17 crates' FAKE_CRATES_FAIL=1 <<< "$(printf '0.9.0\nn')"
expect_pass "release crates: ...a resume asks for crates (covering brew), not publish" sh -c 'grep -q "release: CONFIRM crates, for 0.9.0" "$1" && ! grep -q "CONFIRM publish" "$1" && grep -q "step 16 brew: .*covers it too" "$1"' sh "$rs/ra.out"
expect_pass "release crates: ...verify-published was recorded first" grep -q '^verify_published=ok ' "$RSTATE"
calls6_are 1 1 1 0
touch "$rs/reject-tap"
r5 "release crates: ...again: the rerun accepted and approved again, only the crate crates.io lacks is uploaded; the tap push refused: fails at 16" 1 'FAILED at step 16/17 brew' FAKE_CRATES_FAIL=1 <<< "$(printf '0.9.0\ny')"
expect_pass "release crates: ...each crate uploaded once (fil-proto was on crates.io already)" sh -c 'test "$(grep -c "^crate upload fil-proto$" "$1")" = 1 && test "$(grep -c "^crate upload fil-shell$" "$1")" = 1' sh "$rs/calls.log"
expect_pass "release crates: ...one approval per attempt of the run" sh -c 'test "$(grep "^approve " "$1" | cut -d" " -f4 | sort -u | wc -l | tr -d " ")" = 2' sh "$rs/calls.log"
calls6_are 1 2 2 0
rm -f "$rs/reject-tap"
r5 "release brew: ...again: the formula commit the refused push left is pushed (not made again); the install test fails at 16" 1 'FAILED at step 16/17 brew' FAKE_BREW_FAIL=1 <<< "0.9.0"
expect_pass "release brew: ...it asked for brew on its own" sh -c 'grep -q "release: CONFIRM brew, for 0.9.0" "$1" && ! grep -q "CONFIRM crates" "$1"' sh "$rs/ra.out"
expect_pass "release brew: ...one formula commit, reused" sh -c 'test "$(git --git-dir="$1" log --format=%s main | grep -c "^fil 0.9.0$")" = 1 && grep -q "made by an earlier run and not pushed: it is pushed" "$2"' sh "$rs/tap.git" "$RW/logs/16-brew.log"
expect_pass "release brew: ...saying the release is out, and how to run the install test again" sh -c 'grep -q "the release is out" "$1" && grep -q -- "--redo brew" "$1"' sh "$rs/ra.out"
calls6_are 1 2 2 1
r5 "release finish: ...again: brew detects as done (the tap has it); forgetting the keys fails at 17" 1 'FAILED at step 17/17 finish' FAKE_KEYS_FORGET_FAIL=1 < /dev/null
expect_pass "release finish: ...with the known fix" grep -q 'tests/release/just.sh release-keys forget --need ssh,gpg' "$rs/ra.out"
r5 "release finish: ...again: exit 0" 0 'fil 0.9.0 is released' < /dev/null
r5 "release brew: --redo brew after the failed install test: installs, pushes nothing, asks nothing" 0 'is released' -- --redo brew < /dev/null
expect_pass "release brew: ...installed this time" sh -c 'test "$(grep -c "^brew install o/tap/fil$" "$1")" = 2 && ! grep -q "release: CONFIRM" "$2"' sh "$rs/calls.log" "$rs/ra.out"
calls_are 1 3 1 5
calls6_are 1 2 2 1
st5 > "$rs/golden6.out"
expect_pass "release status: ...the same listing as the happy path's" diff "$rs/golden6" "$rs/golden6.out"

# F3: verify-published fails, then the crates reach crates.io by another route (approved in the
# browser): the next run verifies again instead of taking the step as implied, and touches no tap
r6_reset
r5 "release publish: to 13" 2 'PAUSED at step 13/17 publish' <<< "0.9.0"
r5 "release publish: the published release does not verify: fails at 14" 1 'FAILED at step 14/17 verify-published' FAKE_VERIFY_FAIL=mismatch <<< "0.9.0"
printf 'fil-proto\nfil-shell\n' > "$rs/world/crates"
set_run "$(TR6)" status='"completed"' conclusion='"success"' pending='[]' jobs='[["prepare", "success"], ["publish", "success"], ["crates-io", "success"]]'
rl_kind 14 verify-published next "not implied by the crates" FIL_RELEASE_KEY="$rs/agentpub/key"
r5 "release publish: ...the crates out meanwhile: the next run verifies again (and fails again), the tap untouched" 1 'FAILED at step 14/17 verify-published' FAKE_VERIFY_FAIL=mismatch < /dev/null
calls6_are 1 0 0 0
r5 "release publish: ...verified: on to the end" 0 'fil 0.9.0 is released' <<< "0.9.0"
calls6_are 1 0 0 1

# F7: the tap clone's own commits: a foreign commit stops the run; an unsigned commit is never pushed;
# this release's commit left by a refused push on a main that moved since is made again on the new main
r6_reset
r5 "release publish: to 13" 2 'PAUSED at step 13/17 publish' <<< "0.9.0"
git -C "$rs/tap" commit -q --allow-empty --no-gpg-sign -m 'local work'
r5 "release brew: a commit in the tap clone its origin lacks: fails at 16, nothing pushed" 1 'has commits that are not on its origin.s main' <<< "0.9.0"
calls6_are 1 1 2 0
git -C "$rs/tap" reset -q --hard origin/main
# an unsigned "fil 0.9.0" commit of this release's formula, left in the clone: not taken for an
# earlier run's (tap_ours wants a signature), so it stops the run like any foreign commit
mkdir -p "$rs/tap/Formula"
cp "$rs/tap-v.rb" "$rs/tap/Formula/fil.rb"
git -C "$rs/tap" add Formula/fil.rb
git -C "$rs/tap" commit -q --no-gpg-sign -m 'fil 0.9.0' -- Formula/fil.rb
r5 "release brew: an unsigned 'fil 0.9.0' commit left in the tap clone: not reused, fails at 16, nothing pushed" 1 'has commits that are not on its origin.s main' <<< "0.9.0"
expect_pass "release brew: ...the log never says it is pushed" sh -c '! grep -q "made by an earlier run and not pushed" "$1"' sh "$RW/logs/16-brew.log"
calls6_are 1 1 2 0
git -C "$rs/tap" reset -q --hard origin/main
git -C "$rs/tap" config gpg.ssh.program "$rs/emptysig"
r5 "release brew: a tap commit that is not signed: fails at 16, nothing pushed" 1 'FAILED at step 16/17 brew' <<< "0.9.0"
expect_pass "release brew: ...saying it could not sign (or made an unsigned commit)" grep -Eq 'made an unsigned commit in the tap|could not make or sign the formula commit' "$rs/ra.out"
git -C "$rs/tap" config --unset gpg.ssh.program
git -C "$rs/tap" reset -q --hard origin/main
calls6_are 1 1 2 0
touch "$rs/reject-tap"
r5 "release brew: the tap push refused: fails at 16" 1 'FAILED at step 16/17 brew' <<< "0.9.0"
rm -f "$rs/reject-tap"
echo moved > "$rs/readme2"
TMOVED=$(tap_put README2 "$rs/readme2")
r5 "release brew: ...the tap's main moved meanwhile: the commit made again on it, pushed once" 0 'fil 0.9.0 is released' <<< "0.9.0"
expect_pass "release brew: ...on the moved main, one formula commit there, and the log says so" sh -c 'test "$(git --git-dir="$1" rev-parse main^)" = "$2" && test "$(git --git-dir="$1" log --format=%s main | grep -c "^fil 0.9.0$")" = 1 && grep -q "made again on" "$3"' sh "$rs/tap.git" "$TMOVED" "$RW/logs/16-brew.log"
calls6_are 1 1 2 1

# --confirm-each: one confirmation each for publish, crates and brew; the crates-io deployment not
# created yet for two looks (polled for, then approved once)
r6_reset
r5 "release publish: to 13" 2 'PAUSED at step 13/17 publish' <<< "0.9.0"
r5 "release crates: --confirm-each and a deployment not created yet: three confirmations, one approval, exit 0" 0 'fil 0.9.0 is released' FAKE_PENDING_LAG=2 -- --confirm-each <<< "$(printf '0.9.0\n0.9.0\n0.9.0')"
expect_pass "release crates: ...publish, crates and brew asked once each" sh -c 'for p in publish crates brew; do test "$(grep -c "release: CONFIRM $p, for 0.9.0" "$1")" = 1 || exit 1; done; ! grep -q "covers it too" "$1"' sh "$rs/ra.out"
expect_pass "release crates: ...the pending deployments asked for until one appeared" sh -c 'test "$(grep -c "pending_deployments --jq" "$1")" -ge 3' sh "$pf/gh.log"
calls6_are 1 1 2 1

# a deployment the gh user may not approve: paused (exit 2) with the run's URL; then approved in the
# browser meanwhile: the job is waited for, and the command approves nothing
r6_reset
r5 "release publish: to 13" 2 'PAUSED at step 13/17 publish' <<< "0.9.0"
r5 "release crates: a deployment the gh user may not approve: paused at 15, nothing approved" 2 'PAUSED at step 15/17 crates' FAKE_CANNOT_APPROVE=1 <<< "0.9.0"
expect_pass "release crates: ...saying why, with the run's URL" sh -c 'grep -q "current_user_can_approve is false" "$1" && grep -q "https://github.com/o/r/actions/runs/$2" "$1"' sh "$rs/ra.out" "$(TR6)"
calls6_are 1 0 0 0
set_run "$(TR6)" status='"in_progress"' pending='[]' crates_lag=2
r5 "release crates: ...approved in the browser meanwhile: the job waited for, no approval from the command; exit 0" 0 'fil 0.9.0 is released' <<< "0.9.0"
expect_pass "release crates: ...it asked for crates (a new run)" grep -q 'release: CONFIRM crates, for 0.9.0' "$rs/ra.out"
calls6_are 1 0 2 1

# the draft changed while the publish confirmation waited: an asset replaced, or one added: nothing
# is published; restored: published
r6_reset
r5 "release publish: to 13" 2 'PAUSED at step 13/17 publish' <<< "0.9.0"
DA6=$(ls -d "$rs/world/releases"/* | tail -n 1)/assets
cp "$DA6/fil_0.9.0_amd64.deb" "$rs/deb.saved"
ra_midprompt 'echo tampered > "$DA6/fil_0.9.0_amd64.deb"' FIL_RELEASE_KEY="$rs/agentpub/key"
ra_check "release publish: a draft asset replaced during the confirmation: fails at 13" 1 "the draft's fil_0.9.0_amd64.deb is not the file SHA256SUMS lists"
calls6_are 0 0 0 0
cp "$rs/deb.saved" "$DA6/fil_0.9.0_amd64.deb"
ra_midprompt 'echo extra > "$DA6/notes.txt"' FIL_RELEASE_KEY="$rs/agentpub/key"
ra_check "release publish: an asset added during the confirmation: fails at 13" 1 "not exactly the release's \(extra or missing: notes.txt"
calls6_are 0 0 0 0
rm -f "$DA6/notes.txt"
r5 "release publish: ...restored: published, and on to the end" 0 'fil 0.9.0 is released' <<< "0.9.0"
calls6_are 1 1 2 1
cat "$pf/gh.log" >> "$rs/gh6.log"
rm -f "$rs/tag-run-on-push"

# the state: identifiers and results only
expect_pass "release: the state holds only its keys (no reaper pid, no confirmation, nothing else)" sh -c '! grep -Ev "^(version|release_commit|github_commit|dry_run|checks_linux|checks_macos|checks_waived|dry_verify|tag_object|tag_run|verify_published|brew_tested)=" "$1"' sh "$RSTATE"
# gh's writes are the dispatches of the three workflows on main and reruns of failed jobs (the
# actions); everything else is a read
expect_pass "release: gh wrote only dispatches (the three workflows, on main), reruns --failed, deletions of a release by id, uploads of the five signed files to v0.9.0, its publish and approvals of a run's crates-io deployment" sh -c 'cat "$2" "$3" "$4" > "$1"; test -s "$1" && ! grep -Ev "^workflow run (release\.yml --repo o/r --ref main|(linux|macos)-check\.yml --repo o/r --ref main -f suite=all -f ref=[0-9a-f]{40})$|^run rerun [0-9]+ --repo o/r --failed$|^api -X DELETE repos/o/r/releases/[0-9]+$|^release upload v0\.9\.0 --repo o/r --clobber( (fil-0\.9\.0-1\.(x86_64|aarch64)\.rpm|SHA256SUMS(\.asc|\.sig)?))+$|^release edit v0\.9\.0 --repo o/r --draft=false$|^api -X POST repos/o/r/actions/runs/[0-9]+/pending_deployments -F environment_ids\[\]=[0-9]+ -f state=approved -f comment=approved by just release 0\.9\.0, after its typed confirmation$" "$1" | grep -Eq -- "(^| )(-X|--method|-f|-F|--field|--raw-field|--input)( |=|$)|^release (create|edit|delete|upload)|^workflow |^run (rerun|cancel)"' sh "$rs/gh-all.log" "$rs/gh5.log" "$rs/gh6.log" "$pf/gh.log"
expect_pass "release: ...and it did upload, delete, publish and approve (the log covers the later actions)" sh -c 'grep -q "^release upload " "$1" && grep -q "^api -X DELETE " "$1" && grep -q "^release edit " "$1" && grep -q "^api -X POST .*/pending_deployments " "$1"' sh "$rs/gh-all.log"
expect_pass "release: ...and it did dispatch (the log covers the actions)" grep -q '^workflow run release.yml' "$pf/gh.log"

echo
echo "release-test: $passed passed, $failed failed"
[ "$failed" = 0 ]
exit
}
