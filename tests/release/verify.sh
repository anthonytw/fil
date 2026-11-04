#!/usr/bin/env bash
# `just release-verify`: the owner's check before signing a release, and anyone's afterward
# (docs/m4/plan.md, REL; docs/m4/release.md section 4).
#
#   verify.sh VERSION [--dry-run] [--from-run RUN | --from-dir DIR] [--repo OWNER/REPO]
#                     [--ref REF] [--vm NAME] [--no-rebuild] [--work DIR]
#
# Release (default): download the draft or published release v<VERSION> (`gh release download`),
# then
#   1. the tag is annotated, SSH-signed by a key in .github/release-signers as it is on MAIN (never
#      the tag's own copy), and on main;
#   2. SHA256SUMS.sig verifies (ssh-keygen -Y verify -n fil-release) against the same signers, and
#      the checksums match;
#   3. each archive's attestation verifies, pinned to this repository, the release workflow and the
#      tag (`gh attestation verify --signer-workflow --source-ref`);
#   4. `release-check` passes on the files (--tag v<VERSION> --complete);
#   5. the Linux VM rebuilds the binaries and all four archives from the tag in the digest-pinned
#      images, and the hashes are compared with the release's.
# The macOS binary is the one input that is not rebuilt (it comes from the release): the macOS
# archive is reassembled around it.
#
# --dry-run: for the output of a release dry run (--from-run RUN, or --from-dir): no tag, signature
# or attestation exists, so steps 1-3 are skipped; the rebuild uses --ref (default: the run's
# commit). The VM needs Docker and amd64 emulation that can run rustc (Rosetta; qemu-user does not):
# docs/m4/release.md section 9.3.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

version='' dry=0 from_run='' from_dir='' repo=anthonytw/fil ref='' vm=${FIL_VM:-fil} rebuild=1 work=''
while [ $# -gt 0 ]; do
  case $1 in
    --dry-run) dry=1; shift ;;
    --from-run) from_run=$2; shift 2 ;;
    --from-dir) from_dir=$2; shift 2 ;;
    --repo) repo=$2; shift 2 ;;
    --ref) ref=$2; shift 2 ;;
    --vm) vm=$2; shift 2 ;;
    --no-rebuild) rebuild=0; shift ;;
    --work) work=$2; shift 2 ;;
    -*) die "unknown option: $1" ;;
    *) version=$1; shift ;;
  esac
done
[ -n "$version" ] || die "usage: verify.sh VERSION [--dry-run] [--from-run RUN | --from-dir DIR] ..."
version=${version#v}
tag=v$version
repo_url=${FIL_RELEASE_REPO_URL:-https://github.com/$repo.git}
[ -n "$work" ] || work=$REPO_ROOT/target/release-verify/$version
mkdir -p "$work"
work=$(cd "$work" && pwd)

sha256_stdin() { if command -v sha256sum > /dev/null; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi; }

fails=0
fail() { echo "release-verify: FAIL: $*" >&2; fails=$((fails + 1)); }
ok() { echo "release-verify: ok: $*" >&2; }

# ---- the files
dl=$work/files
if [ -n "$from_dir" ]; then
  dl=$(cd "$from_dir" && pwd)
else
  rm -rf "$dl"
  mkdir -p "$dl"
  if [ -n "$from_run" ]; then
    gh run download "$from_run" --repo "$repo" -n release-files -D "$dl"
  else
    gh release download "$tag" --repo "$repo" -D "$dl"
  fi
fi
log() { echo "release-verify: $*" >&2; }
log "files in $dl: $(ls "$dl" | tr '\n' ' ')"

if [ "$dry" = 0 ]; then
  # ---- 1. the tag, against the signers on main
  tagrepo=$work/tag-repo
  rm -rf "$tagrepo"
  git init -q --bare "$tagrepo"
  git -C "$tagrepo" fetch -q "$repo_url" "+refs/heads/main:refs/remotes/origin/main" "+refs/tags/$tag:refs/tags/$tag" \
    || die "could not fetch main and $tag from $repo_url"
  if "$RELEASE_DIR/verify-tag.sh" "$tagrepo" "$tag" refs/remotes/origin/main; then ok "tag $tag"; else fail "tag $tag"; fi

  # ---- 2. SHA256SUMS.sig, with the same signers (main's, or the verifier's own copy)
  signers=$work/signers
  if [ -n "${FIL_RELEASE_SIGNERS:-}" ]; then
    cp "$FIL_RELEASE_SIGNERS" "$signers"
  else
    git -C "$tagrepo" show "refs/remotes/origin/main:.github/release-signers" > "$signers"
  fi
  if [ ! -f "$dl/SHA256SUMS.sig" ]; then
    fail "no SHA256SUMS.sig in the release (the owner signs SHA256SUMS before publishing)"
  elif ! grep -Evq '^[[:space:]]*(#|$)' "$signers"; then
    fail "release-signers has no key: SHA256SUMS.sig cannot be verified"
  else
    principal=$(ssh-keygen -Y find-principals -f "$signers" -s "$dl/SHA256SUMS.sig" 2> /dev/null | head -n 1) || true
    if [ -n "$principal" ] && ssh-keygen -Y verify -f "$signers" -I "$principal" -n fil-release -s "$dl/SHA256SUMS.sig" < "$dl/SHA256SUMS" > /dev/null 2>&1; then
      ok "SHA256SUMS.sig is a valid fil-release signature by $principal"
    else
      fail "SHA256SUMS.sig does not verify (ssh-keygen -Y verify -n fil-release) against the signers on main"
    fi
  fi

  # ---- 3. attestations, pinned to the workflow and the tag
  for a in "$dl"/fil-"$version"-*.tar.gz; do
    if gh attestation verify "$a" --repo "$repo" \
      --signer-workflow "$repo/.github/workflows/release.yml" \
      --source-ref "refs/tags/$tag" > "$work/attest.log" 2>&1; then
      ok "attestation of $(basename "$a")"
    else
      cat "$work/attest.log" >&2
      fail "attestation of $(basename "$a")"
    fi
  done
fi

# ---- sha256sum -c and release-check
if sums_check "$dl" SHA256SUMS; then ok "sha256sum -c SHA256SUMS"; else fail "sha256sum -c SHA256SUMS"; fi
if [ "$dry" = 1 ]; then
  "$RELEASE_DIR/check.sh" --dry-run --no-tree "$dl" || fail "release-check"
else
  "$RELEASE_DIR/check.sh" --tag "$tag" --complete --no-tree "$dl" || fail "release-check"
fi

# ---- 5. the independent rebuild in the Linux VM
if [ "$rebuild" = 1 ]; then
  command -v limactl > /dev/null || die "limactl not found; the rebuild needs the Linux VM (docs/dev/vm.md, docs/m4/release.md section 9.3)"
  [ "$(limactl list --format '{{.Status}}' "$vm" 2> /dev/null)" = Running ] || die "Lima instance '$vm' is not running (limactl start $vm)"
  if [ -z "$ref" ]; then
    if [ "$dry" = 0 ]; then ref=refs/tags/$tag; else die "--dry-run needs --ref (the commit the run built)"; fi
  fi
  vwork=release-verify/$version
  mac=$work/mac-binary
  rm -rf "$mac"
  mkdir -p "$mac"
  tar -xzf "$dl/fil-$version-universal-apple-darwin.tar.gz" -C "$mac" "fil-$version-universal-apple-darwin/bin/fil"
  limactl shell --workdir / "$vm" -- bash -c 'rm -rf "$HOME/$1/in" && mkdir -p "$HOME/$1/in/universal-apple-darwin" && cat > "$HOME/$1/in/universal-apple-darwin/fil"' vm-sh "$vwork" \
    < "$mac/fil-$version-universal-apple-darwin/bin/fil"
  log "rebuilding $ref of $repo_url in the VM '$vm' (about 15 to 30 minutes)"
  limactl shell --workdir / "$vm" -- bash -s -- "$repo_url" "$ref" "$vwork" < "$RELEASE_DIR/rebuild.sh" \
    || { fail "the rebuild in the VM failed"; }
  rb=$work/rebuilt
  rm -rf "$rb"
  mkdir -p "$rb"
  limactl shell --workdir / "$vm" -- bash -c 'cd "$HOME/$1/out" && tar -cf - .' vm-sh "$vwork" | tar -xf - -C "$rb" || fail "could not copy the rebuilt files out of the VM"
  if [ -f "$rb/SHA256SUMS" ]; then
    echo "release-verify: rebuilt vs. released SHA256SUMS:" >&2
    if diff "$dl/SHA256SUMS" "$rb/SHA256SUMS" > "$work/sums.diff"; then
      ok "all four archives reproduce byte for byte (the macOS binary inside the macOS archive is the release's own)"
    else
      cat "$work/sums.diff" >&2
      fail "the rebuilt archives differ from the release's (diff above: < released, > rebuilt)"
    fi
    for t in x86_64-linux-musl aarch64-linux-musl; do
      a=fil-$version-$t
      r1=$(tar -xzOf "$dl/$a.tar.gz" "$a/bin/fil" | sha256_stdin)
      r2=$(tar -xzOf "$rb/$a.tar.gz" "$a/bin/fil" | sha256_stdin)
      if [ "$r1" = "$r2" ]; then ok "$t binary $r1 reproduces"; else fail "$t binary: released $r1, rebuilt $r2"; fi
    done
  else
    fail "the VM left no SHA256SUMS"
  fi
fi

if [ "$fails" -gt 0 ]; then
  echo "release-verify: $fails check(s) FAILED: do not sign" >&2
  exit 1
fi
echo "release-verify: all checks passed" >&2
