#!/usr/bin/env bash
# `just release-verify`: the owner's check before signing a release, and anyone's afterward
# (docs/m4/plan.md, REL; docs/m4/release.md section 4).
#
#   verify.sh VERSION [--dry-run] [--from-run RUN | --from-dir DIR] [--repo OWNER/REPO]
#                     [--ref REF] [--vm NAME] [--no-rebuild] [--pre-sign] [--work DIR]
#
# Release (default): download the draft or published release v<VERSION> (`gh release download`),
# then, STOPPING at the first failure of steps 1-3 (nothing from the download is looked at, let alone
# run, until they pass):
#   1. the tag is annotated, SSH-signed by a key in .github/release-signers as it is on MAIN (never
#      the tag's own copy), and on main. Its commit is resolved ONCE here; everything below is bound
#      to that commit, not to a name that can move;
#   2. SHA256SUMS.sig verifies (ssh-keygen -Y verify -n fil-release) against the same signers, and
#      the files match SHA256SUMS. `--pre-sign` skips only the signature, for the run the owner makes
#      BEFORE signing (the signature does not exist yet); run again without it afterwards;
#   3. each archive's attestation verifies, pinned to the repository, the exact workflow identity at
#      the tag, the commit (source and signer digest) and a GitHub-hosted runner;
# then, in the rest, failures are counted:
#   4. `release-check --no-exec` passes on the files (--tag v<VERSION> --complete): the files are
#      unpacked, hashed, grepped and inspected, and NOTHING that came from the download is run
#      (no `cargo` in the source archive, no `--version` of a binary);
#   5. the Linux VM rebuilds the Linux binaries, the four archives and the four .deb and .rpm
#      packages from that commit in the digest-pinned images, and the hashes are compared with the
#      release's. The rebuild judges the source archive byte for byte. Three archives (x86_64,
#      aarch64, source) and the four packages are reproduced from the source; the macOS archive is
#      only REASSEMBLED around the released macOS binary, which is not rebuilt and is ad-hoc signed.
#
# --dry-run: for the output of a release dry run (--from-run RUN, or --from-dir): no tag, signature
# or attestation exists, so steps 1-3 are skipped; the rebuild uses --ref (a commit SHA: the run's
# commit). Nothing from the download is run in this mode either. The VM needs Docker and amd64
# emulation that can run rustc (Rosetta; qemu-user does not): docs/m4/release.md section 9.3.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

version='' dry=0 from_run='' from_dir='' repo=anthonytw/fil ref='' vm=${FIL_VM:-fil} rebuild=1 presign=0 work=''
while [ $# -gt 0 ]; do
  case $1 in
    --dry-run) dry=1; shift ;;
    --from-run) from_run=$2; shift 2 ;;
    --from-dir) from_dir=$2; shift 2 ;;
    --repo) repo=$2; shift 2 ;;
    --ref) ref=$2; shift 2 ;;
    --vm) vm=$2; shift 2 ;;
    --no-rebuild) rebuild=0; shift ;;
    --pre-sign) presign=1; shift ;;
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
  "$RELEASE_DIR/verify-tag.sh" "$tagrepo" "$tag" refs/remotes/origin/main || die "STOP: tag $tag does not verify; nothing from the download was looked at"
  ok "tag $tag"
  # The one commit everything below is bound to.
  commit=$(git -C "$tagrepo" rev-parse "refs/tags/$tag^{commit}")
  log "$tag is commit $commit"

  # ---- 2. SHA256SUMS.sig, with the same signers (main's, or the verifier's own copy), and the sums
  signers=$work/signers
  if [ -n "${FIL_RELEASE_SIGNERS:-}" ]; then
    cp "$FIL_RELEASE_SIGNERS" "$signers"
  else
    git -C "$tagrepo" show "refs/remotes/origin/main:.github/release-signers" > "$signers"
  fi
  if [ "$presign" = 1 ]; then
    echo "release-verify: NOTE: --pre-sign: SHA256SUMS.sig was NOT checked; run again without it after signing" >&2
  elif [ ! -f "$dl/SHA256SUMS.sig" ]; then
    die "STOP: no SHA256SUMS.sig in the release (use --pre-sign for the run before signing)"
  elif ! grep -Evq '^[[:space:]]*(#|$)' "$signers"; then
    die "STOP: release-signers has no key: SHA256SUMS.sig cannot be verified"
  else
    principal=$(ssh-keygen -Y find-principals -f "$signers" -s "$dl/SHA256SUMS.sig" 2> /dev/null | head -n 1) || true
    if [ -n "$principal" ] && ssh-keygen -Y verify -f "$signers" -I "$principal" -n fil-release -s "$dl/SHA256SUMS.sig" < "$dl/SHA256SUMS" > /dev/null 2>&1; then
      ok "SHA256SUMS.sig is a valid fil-release signature by $principal"
    else
      die "STOP: SHA256SUMS.sig does not verify (ssh-keygen -Y verify -n fil-release) against the signers on main"
    fi
  fi
  sums_check "$dl" SHA256SUMS || die "STOP: the files do not match SHA256SUMS"
  ok "sha256sum -c SHA256SUMS"

  # ---- 3. attestations: this repository, this exact workflow identity at this tag, this commit
  # (--source-digest is the commit that was built, --signer-digest the commit of the workflow file),
  # and no self-hosted runner.
  for a in "$dl"/fil-"$version"-*.tar.gz $(package_names "$version" | sed "s|^|$dl/|"); do
    if gh attestation verify "$a" --repo "$repo" \
      --cert-identity "https://github.com/$repo/.github/workflows/release.yml@refs/tags/$tag" \
      --source-ref "refs/tags/$tag" --source-digest "$commit" --signer-digest "$commit" \
      --deny-self-hosted-runners > "$work/attest.log" 2>&1; then
      ok "attestation of $(basename "$a")"
    else
      cat "$work/attest.log" >&2
      die "STOP: attestation of $(basename "$a") does not verify"
    fi
  done
fi

# ---- sha256sum -c and release-check
if [ "$dry" = 1 ]; then
  if sums_check "$dl" SHA256SUMS; then ok "sha256sum -c SHA256SUMS"; else fail "sha256sum -c SHA256SUMS"; fi
  "$RELEASE_DIR/check.sh" --dry-run --no-tree --no-exec "$dl" || fail "release-check"
else
  "$RELEASE_DIR/check.sh" --tag "$tag" --complete --no-tree --no-exec "$dl" || fail "release-check"
fi

# ---- 5. the independent rebuild in the Linux VM
if [ "$rebuild" = 1 ]; then
  command -v limactl > /dev/null || die "limactl not found; the rebuild needs the Linux VM (docs/dev/vm.md, docs/m4/release.md section 9.3)"
  [ "$(limactl list --format '{{.Status}}' "$vm" 2> /dev/null)" = Running ] || die "Lima instance '$vm' is not running (limactl start $vm)"
  if [ -z "$ref" ]; then
    if [ "$dry" = 0 ]; then ref=$commit; else die "--dry-run needs --ref (the commit SHA the run built)"; fi
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
    for n in fil-"$version"-x86_64-linux-musl.tar.gz fil-"$version"-aarch64-linux-musl.tar.gz fil-"$version"-source.tar.gz fil-"$version"-universal-apple-darwin.tar.gz $(package_names "$version"); do
      h1=$(grep -F "  $n" "$dl/SHA256SUMS" | cut -d' ' -f1)
      h2=$(grep -F "  $n" "$rb/SHA256SUMS" | cut -d' ' -f1)
      if [ -z "$h1" ] || [ "$h1" != "$h2" ]; then
        fail "$n: released $h1, rebuilt $h2"
      elif [ "$n" = "fil-$version-universal-apple-darwin.tar.gz" ]; then
        ok "$n is REASSEMBLED identically around the released macOS binary (that binary was not rebuilt)"
      else
        ok "$n is REPRODUCED from source byte for byte ($h1)"
      fi
    done
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
if [ "$presign" = 1 ]; then
  echo "release-verify: all checks passed EXCEPT the signature, which --pre-sign did not check" >&2
else
  echo "release-verify: all checks passed" >&2
fi
