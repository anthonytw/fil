#!/usr/bin/env bash
# `tests/release/just.sh release-preflight X.Y.Z`: everything a release needs, checked before anything is done
# (docs/m4/release.md section 9.4). Read-only: it changes nothing in the clone, on GitHub or on
# Gitea, and pulls nothing. Its traces: the crates the lockfiles name, fetched into the cargo cache
# (outside the clone) when it lacks them; a dangling signed commit object (the commit-signing check:
# `git commit-tree -S` of the empty tree), which `git gc` removes; in the VM, a removed container,
# run only from the cross image if it is already there; and the keychain's "Always Allow" for
# codesign if the test signature asks for it (meant: so it is not asked mid-release). It never
# starts Docker or the VM; it says how.
#
#   release-preflight.sh VERSION [--from STEP] [--only CHECK[,CHECK...]] [--commit SHA]
#                                [--repo OWNER/REPO] [--vm NAME] [--origin REMOTE] [--github REMOTE]
#   release-preflight.sh --list
#
#   --from STEP    only the checks that STEP and the steps after it need (default: the first step,
#                  so every check). The steps are those of `tests/release/just.sh release` (PR 3; `--list` shows the
#                  map): a resume at `crates` does not need the VM.
#   --only CHECKS  exactly these checks (comma-separated), plus the override check, which always runs.
#   --commit SHA   the release commit R the run is bound to: HEAD must be R, and R on --origin's main
#                  (main may have moved on). Without it, HEAD must BE --origin's main, as before the
#                  tag; a --from step after `tag` without --commit binds to HEAD.
#
# Every failure names the 0.1.0 class of trouble it catches and the exact fix. All checks run (a
# failure does not stop the rest), except the override check, which is fatal. Exit 0 when nothing
# failed (warnings are printed), 1 otherwise.
#
# Not here: the Gitea CI status of R (owner 2026-11-09: skipped, main is handed over green) and the
# key UNLOCKS (`release-keys.sh`, PR 2). This only checks that the keys exist and are listed.
#
# Sourcing (for release.sh): `source release-preflight.sh` defines the functions and runs nothing.
# Then `pf_init VERSION`, optionally set PF_REPO, PF_VM, PF_ORIGIN, PF_GITHUB, PF_COMMIT (the bound
# release commit: set it once the tag exists, from the state's release_commit), PF_AGAIN
# (the "then" line of each failure), PF_LOCK_OWNER (the pid holding the release lock: the caller's,
# default $$), and `pf_run $(pf_checks_from STEP)` or `pf_run CHECK...`; it
# returns the number of failed checks (0 = all passed).
#
# The tools it calls, each replaceable for the self-test: FIL_GH, FIL_GATE_GH (passed on to
# crates-io-gate.sh), FIL_DOCKER, FIL_LIMACTL, FIL_GPG, FIL_SSH, FIL_SECURITY, FIL_NOTARYTOOL,
# FIL_CODESIGN, FIL_CARGO, FIL_CRATES_CURL, FIL_DF, FIL_RPMSIGN, FIL_RPMKEYS; FIL_RELEASE_ROOT names
# the clone. Each is a test-only override: set without FIL_RELEASE_SELFTEST=1 it is fatal, because a
# preflight that asked a fake is no preflight. Configuration that is not an override:
# FIL_RELEASE_KEY (default ~/.ssh/fil-release; its .pub is checked), FIL_CODESIGN_IDENTITY,
# FIL_NOTARY_PROFILE (default fil-notary), FIL_VM (default fil).
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

# The one GitHub account that releases (as in crates-io-gate.sh).
PF_OWNER=anthonytw
# Free space: the clone (target/, the downloads, the rebuild's copies) and the VM (the rebuild).
PF_MIN_DISK_GIB=10
PF_MIN_VM_DISK_GIB=20
# The package key: warn when it expires sooner than this.
PF_KEY_WARN_DAYS=60

# The test-only overrides: common.sh's one list, without the self-test's own switch.
PF_OVERRIDES=$(printf '%s\n' $TEST_OVERRIDES | grep -vx FIL_RELEASE_SELFTEST | tr '\n' ' ')

# The checks, in the order they run.
PF_CHECKS="overrides tools lock disk tree recipes version lockfiles github-main gh github-ssh commit-signing crates-env tag-ruleset tag-free docker vm release-key package-key apple"
# The steps of `tests/release/just.sh release` (docs: the design's section 5), in order.
PF_STEPS="candidate mirror dry-run dry-verify tag tag-run verify-presign sign-macos sign-packages sign-sums upload verify-signed publish verify-published crates brew finish"

# pf_step_checks STEP: the checks STEP needs (besides the ones every invocation runs).
pf_step_checks() {
  case $1 in
    candidate) echo "version lockfiles" ;;
    mirror) echo "github-main gh github-ssh release-key" ;;
    dry-run) echo "gh" ;;
    dry-verify) echo "docker vm" ;;
    tag) echo "tag-free tag-ruleset crates-env release-key github-ssh gh" ;;
    tag-run) echo "gh" ;;
    verify-presign) echo "gh docker vm release-key" ;;
    sign-macos) echo "gh docker apple release-key" ;;
    sign-packages) echo "package-key" ;;
    sign-sums) echo "release-key" ;;
    upload | publish | verify-published) echo "gh" ;;
    verify-signed) echo "gh docker vm" ;;
    crates) echo "gh crates-env" ;;
    brew) echo "github-ssh commit-signing" ;;
    finish) echo "" ;;
    *) return 1 ;;
  esac
}
# pf_step_after_tag STEP: STEP comes after `tag` (the tag exists, so the run is bound to its commit).
pf_step_after_tag() {
  local s seen=0
  for s in $PF_STEPS; do
    [ "$s" = "$1" ] && { [ "$seen" = 1 ]; return; }
    [ "$s" = tag ] && seen=1
  done
  return 1
}
# Checked on every invocation, whatever the remaining steps are.
PF_ALWAYS="overrides tools lock disk tree recipes"

# pf_checks_from STEP: the checks for STEP and every step after it, in PF_CHECKS order.
pf_checks_from() {
  local s on=0 want=" $PF_ALWAYS " c out=''
  for s in $PF_STEPS; do
    [ "$s" = "$1" ] && on=1
    [ "$on" = 1 ] && want="$want $(pf_step_checks "$s") "
  done
  [ "$on" = 1 ] || return 1
  for c in $PF_CHECKS; do
    case $want in *" $c "*) out="$out $c" ;; esac
  done
  echo "${out# }"
}

# pf_check_tools CHECK: the tools CHECK calls (names; pf_tool resolves the overrides).
pf_check_tools() {
  case $1 in
    tree | github-main | commit-signing | version) echo git ;;
    lockfiles) echo cargo ;;
    gh | crates-env | tag-ruleset) echo gh ;;
    tag-free) echo "git gh curl" ;;
    github-ssh) echo ssh ;;
    docker) echo docker ;;
    vm) echo limactl ;;
    disk) echo df ;;
    release-key) echo "git ssh-keygen ssh-add" ;;
    package-key) echo "git gpg rpmsign rpmkeys" ;;
    apple) echo "security xcrun codesign" ;;
    *) echo "" ;;
  esac
}
# pf_tool NAME: the command for tool NAME (the override if one is set).
pf_tool() {
  case $1 in
    gh) echo "${FIL_GH:-gh}" ;;
    docker) echo "${FIL_DOCKER:-docker}" ;;
    limactl) echo "${FIL_LIMACTL:-limactl}" ;;
    gpg) echo "${FIL_GPG:-gpg}" ;;
    ssh) echo "${FIL_SSH:-ssh}" ;;
    security) echo "${FIL_SECURITY:-security}" ;;
    codesign) echo "${FIL_CODESIGN:-codesign}" ;;
    cargo) echo "${FIL_CARGO:-cargo}" ;;
    curl) echo "${FIL_CRATES_CURL:-curl}" ;;
    df) echo "${FIL_DF:-df}" ;;
    rpmsign) echo "${FIL_RPMSIGN:-rpmsign}" ;;
    rpmkeys) echo "${FIL_RPMKEYS:-rpmkeys}" ;;
    *) echo "$1" ;;
  esac
}

# ---- reporting (the settings default to empty until pf_init: the script runs under set -u)
PF_FAILS=0 PF_CUR='' PF_CARGO_FETCH=${PF_CARGO_FETCH:-1} PF_LOCK_OWNER=${PF_LOCK_OWNER:-$$} PF_AGAIN=${PF_AGAIN:-} PF_COMMIT=${PF_COMMIT:-} PF_REPO=${PF_REPO:-} PF_VM=${PF_VM:-} PF_ORIGIN=${PF_ORIGIN:-} PF_GITHUB=${PF_GITHUB:-} PF_R='' PF_G=''
pf_ok() { echo "preflight: ok    $PF_CUR: $*" >&2; }
pf_note() { echo "preflight: note  $PF_CUR: $*" >&2; }
pf_warn() { echo "preflight: WARN  $PF_CUR: $*" >&2; }
# pf_fail WHAT CLASS FIX: one failure of the current check. CLASS is the 0.1.0 trouble it catches
# ("-" for none); FIX is the exact thing to do.
pf_fail() {
  PF_FAILS=$((PF_FAILS + 1))
  {
    echo "preflight: FAIL  $PF_CUR: $1"
    [ "$2" = - ] || echo "  catches: $2"
    echo "  do:      $3"
    echo "  then:    $PF_AGAIN"
  } >&2
}

# pf_init VERSION: the settings every check reads. The PF_* variables may be set before or after.
pf_init() {
  PF_VERSION=${1#v}
  PF_TAG=v$PF_VERSION
  PF_REPO=${PF_REPO:-anthonytw/fil}
  PF_VM=${PF_VM:-${FIL_VM:-fil}}
  PF_ORIGIN=${PF_ORIGIN:-origin}
  PF_GITHUB=${PF_GITHUB:-github}
  PF_COMMIT=${PF_COMMIT:-}
  PF_AGAIN=${PF_AGAIN:-just release-preflight $PF_VERSION}
  PF_KEY=${FIL_RELEASE_KEY:-$HOME/.ssh/fil-release}
  case $PF_KEY in *.pub) PF_KEY_PUB=$PF_KEY ;; *) PF_KEY_PUB=$PF_KEY.pub ;; esac
  PF_WORK=$ROOT/target/release-verify/$PF_VERSION
  PF_R='' PF_G=''
}

pf_git() { git -C "$ROOT" "$@"; }

# ---- the checks. Each prints its findings and returns 1 if it failed (pf_fail was called).

pf_check_overrides() {
  local v set=''
  test_override_notes
  for v in $PF_OVERRIDES; do
    [ -z "${!v:-}" ] || set="$set $v"
  done
  if [ -n "$set" ] && [ "${FIL_RELEASE_SELFTEST:-}" != 1 ]; then
    pf_fail "test-only override(s) set:$set: a preflight that asks a fake proves nothing" - "unset$set (only the self-test sets them, with FIL_RELEASE_SELFTEST=1)"
    return 1
  fi
  pf_ok "no test-only override${set:+ outside the self-test}"
}

PF_SELECTED=''
pf_check_tools_run() {
  local c t cmd seen=' ' bad=0 ver
  for c in $PF_SELECTED; do
    for t in just python3 $(pf_check_tools "$c"); do
      case $seen in *" $t "*) continue ;; esac
      seen="$seen$t "
      cmd=$(pf_tool "$t")
      if ! command -v "$cmd" > /dev/null 2>&1; then
        pf_fail "$t is not installed (looked for '$cmd')" - "$(pf_install_hint "$t")"
        bad=1
        continue
      fi
      case $t in
        codesign | security | df | ssh-add | ssh-keygen) ver=$(command -v "$cmd") ;;
        ssh) ver=$("$cmd" -V 2>&1 | head -n 1) ;;
        *) ver=$("$cmd" --version 2>&1 | head -n 1) ;;
      esac
      pf_ok "$t: $ver"
    done
  done
  return "$bad"
}
pf_install_hint() {
  case $1 in
    gh) echo "brew install gh, then gh auth login" ;;
    gpg) echo "brew install gnupg" ;;
    rpmsign | rpmkeys) echo "brew install rpm" ;;
    limactl) echo "brew install lima, then create the VM (docs/dev/vm.md)" ;;
    docker) echo "install Docker Desktop" ;;
    just) echo "brew install just" ;;
    xcrun | codesign) echo "xcode-select --install" ;;
    *) echo "install $1" ;;
  esac
}

pf_check_lock() {
  local l=$PF_WORK/.lock pid
  if [ ! -d "$l" ]; then
    pf_ok "no other release run holds $l"
    return 0
  fi
  pid=$(cat "$l/pid" 2> /dev/null || true)
  if [ "$pid" = "$PF_LOCK_OWNER" ]; then
    pf_ok "the release lock is this run's (pid $pid)"
    return 0
  fi
  if [ -n "$pid" ] && kill -0 "$pid" 2> /dev/null; then
    pf_fail "another release run (pid $pid) holds $l" "two runs at once" "wait for it, or stop it (kill $pid)"
    return 1
  fi
  pf_warn "a stale lock (pid ${pid:-unknown} is not running): $l; tests/release/just.sh release removes it (or: rm -r '$l')"
}

# pf_avail_kib DF ARGS...: the free KiB that `DF ARGS` (POSIX df -Pk) reports ("" if unknown).
pf_avail_kib() { "$@" 2> /dev/null | awk 'NR == 2 { print $4 }'; }
pf_check_disk() {
  local df kib bad=0 lima
  df=$(pf_tool df)
  kib=$(pf_avail_kib "$df" -Pk "$ROOT")
  if [ -z "$kib" ]; then
    pf_fail "cannot read the free space of $ROOT" "disk full (97% on 2026-11-07)" "df -h '$ROOT'"
    bad=1
  elif [ "$kib" -lt $((PF_MIN_DISK_GIB * 1048576)) ]; then
    pf_fail "$((kib / 1048576)) GiB free for $ROOT; a release needs $PF_MIN_DISK_GIB GiB" "disk full (97% on 2026-11-07)" "free space: cargo clean in old worktrees, remove old target/release-verify/*"
    bad=1
  else
    pf_ok "$((kib / 1048576)) GiB free for the clone"
  fi
  # The VM's disk, only when the VM is needed and running (the vm check reports a stopped VM).
  case " $PF_SELECTED " in *" vm "*) ;; *) return "$bad" ;; esac
  lima=$(pf_tool limactl)
  [ "$("$lima" list --format '{{.Status}}' "$PF_VM" 2> /dev/null)" = Running ] || { pf_note "VM $PF_VM is not running: its disk is not checked"; return "$bad"; }
  kib=$(pf_avail_kib "$lima" shell "$PF_VM" df -Pk /)
  if [ -z "$kib" ]; then
    pf_fail "cannot read the free space of VM $PF_VM" "disk full (97% on 2026-11-07)" "limactl shell $PF_VM df -h /"
    bad=1
  elif [ "$kib" -lt $((PF_MIN_VM_DISK_GIB * 1048576)) ]; then
    pf_fail "$((kib / 1048576)) GiB free in VM $PF_VM; the rebuild needs $PF_MIN_VM_DISK_GIB GiB" "disk full (97% on 2026-11-07)" "limactl shell $PF_VM docker system prune -a (and remove old rebuild trees there)"
    bad=1
  else
    pf_ok "$((kib / 1048576)) GiB free in VM $PF_VM"
  fi
  return "$bad"
}

pf_check_tree() {
  local head main bad=0
  head=$(pf_git rev-parse -q --verify 'HEAD^{commit}' 2> /dev/null) || { pf_fail "$ROOT is not a git clone with a commit" - "run it in your fil clone"; return 1; }
  if [ -n "$(pf_git status --porcelain --untracked-files=no)" ]; then
    pf_fail "the clone has uncommitted changes to tracked files" - "git status; commit them on a branch or discard them (git restore .), then check out main"
    bad=1
  fi
  main=$(pf_git ls-remote --exit-code "$PF_ORIGIN" refs/heads/main 2> /dev/null | awk '{ print $1 }') || main=''
  if [ -z "$main" ]; then
    pf_fail "cannot read main on remote '$PF_ORIGIN' (Gitea)" - "check the remote: git remote -v; git ls-remote $PF_ORIGIN main"
    bad=1
  elif [ -n "$PF_COMMIT" ]; then
    # Bound (the tag exists): HEAD is the release commit, which is on main; main may have moved on.
    if [ "$head" != "$PF_COMMIT" ]; then
      pf_fail "HEAD ($head) is not the release commit this run is bound to ($PF_COMMIT)" - "git checkout $PF_COMMIT (a new commit after the tag is a new version)"
      bad=1
    elif ! pf_git cat-file -e "$main^{commit}" 2> /dev/null; then
      pf_fail "$PF_ORIGIN's main ($main) is not in the clone" - "git fetch $PF_ORIGIN"
      bad=1
    elif ! pf_git merge-base --is-ancestor "$PF_COMMIT" "$main"; then
      pf_fail "the release commit $PF_COMMIT is not on $PF_ORIGIN's main ($main)" - "check out the commit the tag was made from; a commit that is not on main is not a release"
      bad=1
    fi
  elif [ "$head" != "$main" ]; then
    pf_fail "HEAD ($head) is not $PF_ORIGIN's main ($main)" - "git fetch $PF_ORIGIN && git checkout main && git merge --ff-only $PF_ORIGIN/main"
    bad=1
  fi
  PF_R=$head
  if [ "$bad" = 0 ]; then
    if [ -n "$PF_COMMIT" ]; then pf_ok "clean, HEAD is the bound release commit $head, on $PF_ORIGIN's main"; else pf_ok "clean, HEAD is $PF_ORIGIN's main: release commit $head"; fi
  fi
  return "$bad"
}
# The release path reads tests/release/release.just (owned) alone, never the justfile (unowned):
# tests/release/recipes.sh, on the clone's tree.
pf_check_recipes() {
  local out
  if out=$("$RELEASE_DIR/recipes.sh" "$ROOT" 2>&1); then
    pf_ok "${out#recipes: }"
  else
    pf_fail "$(printf '%s' "$out" | sed 's/^recipes: //' | tr '\n' ' ')" "a release path that reads a file with no code owner (the justfile, or one release.just imports)" \
      "a PR on Gitea that keeps tests/release/release.just self-contained and the release scripts on tests/release/just.sh (tests/release/recipes.sh says what it requires), merged to main"
    return 1
  fi
}
# The release commit: what the tree check found, or HEAD.
pf_r() { [ -n "$PF_R" ] || PF_R=$(pf_git rev-parse -q --verify 'HEAD^{commit}' 2> /dev/null || true); echo "$PF_R"; }

# pf_cargo_version REV: the [workspace.package] version of Cargo.toml at REV ("" if none).
pf_cargo_version() {
  pf_git show "$1:Cargo.toml" 2> /dev/null | awk '
    /^\[workspace\.package\]/ { p = 1; next }
    /^\[/ { p = 0 }
    p && /^version[ ]*=/ { v = $0; sub(/^version[ ]*=[ ]*"/, "", v); sub(/".*$/, "", v); print v; exit }'
}
pf_check_version() {
  local r v pins bad=0 vre
  r=$(pf_r)
  v=$(pf_cargo_version "$r")
  if [ "$v" != "$PF_VERSION" ]; then
    pf_fail "Cargo.toml's version is '${v:-none}', not $PF_VERSION" - "a PR on Gitea: just set-version $PF_VERSION (and the CHANGELOG.md section), merged to main"
    bad=1
  fi
  pins=$(pf_git show "$r:Cargo.toml" 2> /dev/null | sed -n 's/.*version = "=\([^"]*\)".*/\1/p' | sort -u | tr '\n' ' ')
  if [ -n "$pins" ] && [ "$pins" != "$PF_VERSION " ]; then
    pf_fail "the =X.Y.Z pins in Cargo.toml are '${pins% }', not =$PF_VERSION" - "a PR on Gitea: just set-version $PF_VERSION, merged to main"
    bad=1
  fi
  vre=$(printf '%s' "$PF_VERSION" | sed 's/[.]/[.]/g')
  if ! pf_git show "$r:CHANGELOG.md" 2> /dev/null | grep -Eq "^##[[:space:]]+\[?v?${vre}\]?([[:space:]]|\$)"; then
    pf_fail "CHANGELOG.md has no section for $PF_VERSION" - "a PR on Gitea: rename [Unreleased] to [$PF_VERSION] - DATE in CHANGELOG.md, merged to main"
    bad=1
  fi
  [ "$bad" = 1 ] || pf_ok "Cargo.toml, its pins and CHANGELOG.md are at $PF_VERSION"
  return "$bad"
}

# The lockfiles: the crates each lock names are fetched into the cargo cache first (cargo fetch
# --locked: offline when the cache has them, so a warm cache needs no network), then each lock must
# resolve offline as it is. A --locked fetch that fails naming the lock file is a stale lock (cargo
# 1.99: "cannot update the lock file ... because --locked was passed"; older: "the lock file needs
# to be updated"); any other failure is the network or the registry, and says so. PF_CARGO_FETCH=0 (release.sh --status:
# read-only) checks the cache as it is, with no fetch.
pf_check_lockfiles() {
  local cargo m bad=0 err
  cargo=$(pf_tool cargo)
  for m in Cargo.toml tests/terminals/exporter/Cargo.toml; do
    if [ "$PF_CARGO_FETCH" != 0 ] && ! (cd "$ROOT" && "$cargo" fetch --locked --offline --manifest-path "$m" > /dev/null 2>&1); then
      if ! err=$(cd "$ROOT" && "$cargo" fetch --locked --manifest-path "$m" 2>&1 > /dev/null); then
        if printf '%s' "$err" | grep -qiE 'lock ?file|--locked'; then
          pf_fail "the lockfile of $m is not current: $(printf '%s' "$err" | grep -m1 -i 'error' | head -c 200)" "stale exporter lock" \
            "a PR on Gitea with just set-version $PF_VERSION (it updates both locks; just exporter-lock checks the exporter's), merged to main"
        else
          pf_fail "cargo fetch --locked --manifest-path $m failed, so the crates its lockfile names are not in the cargo cache; that is the network or the registry, not the lockfile: $(printf '%s' "$err" | grep -m1 -i 'error' | head -c 200)" - \
            "check the network (cargo fetch --locked --manifest-path $m shows the whole error), then run it again"
        fi
        bad=1
        continue
      fi
      pf_note "fetched the crates of $m's lockfile into the cargo cache"
    fi
    if err=$(cd "$ROOT" && "$cargo" metadata --locked --offline --format-version 1 --manifest-path "$m" 2>&1 > /dev/null); then
      pf_ok "the lockfile of $m is current"
    else
      pf_fail "the lockfile of $m is not current, or its crates are not fetched: $(printf '%s' "$err" | grep -m1 -i 'error' | head -c 200)" "stale exporter lock" \
        "if it says the lock needs updating: a PR on Gitea with just set-version $PF_VERSION (it updates both locks; just exporter-lock checks the exporter's); if it names a missing package: cargo fetch --locked --manifest-path $m"
      bad=1
    fi
  done
  return "$bad"
}

pf_check_github_main() {
  local r g
  r=$(pf_r)
  g=$(pf_git ls-remote --exit-code "$PF_GITHUB" refs/heads/main 2> /dev/null | awk '{ print $1 }') || g=''
  if [ -z "$g" ]; then
    pf_fail "cannot read main on remote '$PF_GITHUB' (the GitHub mirror)" - "git remote add $PF_GITHUB git@github.com:$PF_REPO.git (or fix it: git remote -v)"
    return 1
  fi
  if ! pf_git cat-file -e "$g^{commit}" 2> /dev/null; then
    pf_fail "$PF_GITHUB's main ($g) is not in the clone" - "git fetch $PF_GITHUB"
    return 1
  fi
  PF_G=$g
  if [ "$g" = "$r" ]; then
    pf_ok "$PF_GITHUB/main is the release commit ($r): step mirror is done"
    return 0
  fi
  if pf_git merge-base --is-ancestor "$r" "$g"; then
    pf_main_trusted "$r" "$g" || return 1
    pf_warn "$PF_GITHUB/main ($g) has moved on past the release commit $r: step mirror is done, but the dry run (step dry-run) dispatches on main and needs main at the release commit; hold the pushes of GitHub's main until it is dispatched, or release main's head"
    return 0
  fi
  if pf_git merge-base --is-ancestor "$g" "$r"; then
    pf_note "$PF_GITHUB/main ($g) is behind the release commit (new commits: $(pf_git rev-list --count "$g..$r")): step mirror fast-forwards it"
    return 0
  fi
  pf_fail "$PF_GITHUB/main ($g) is not an ancestor of the release commit $r: it has commits Gitea's main lacks, and step mirror only fast-forwards (it never forces)" "GitHub's main pushed from outside Gitea" \
    "see what they are (git log $r..$PF_GITHUB/main); they must reach Gitea's main first (a PR there), then release from the new main"
  return 1
}
# pf_main_trusted R G: G (GitHub's main, past R) is on Gitea's main and has R's release-signers, byte for
# byte: the tag and every verification read the signers from GitHub's main, which any account that
# may push it can fast-forward. Fails (pf_fail) if not.
pf_main_trusted() {
  if ! pf_git merge-base --is-ancestor "$2" "refs/remotes/$PF_ORIGIN/main" 2> /dev/null; then
    pf_fail "$PF_GITHUB/main ($2) is past the release commit with commits $PF_ORIGIN/main lacks: the signers on it are not Gitea's" "GitHub's main pushed from outside Gitea" \
      "see what they are (git fetch $PF_ORIGIN $PF_GITHUB; git log $PF_ORIGIN/main..$PF_GITHUB/main); they must reach Gitea's main first (a PR there)"
    return 1
  fi
  if [ "$(pf_git rev-parse -q --verify "$2:.github/release-signers" 2> /dev/null || true)" != "$(pf_git rev-parse -q --verify "$1:.github/release-signers" 2> /dev/null || true)" ]; then
    pf_fail "$PF_GITHUB/main ($2) has a .github/release-signers that differs from the release commit's: the tag and the verifications read it from main" "rotated or lost release key" \
      "a change of the release signers needs a new release commit (a PR on Gitea, then release that commit)"
    return 1
  fi
}
# github/main: what the github-main check read, else the clone's remote-tracking ref (no network).
pf_g() { [ -n "$PF_G" ] || PF_G=$(pf_git rev-parse -q --verify "refs/remotes/$PF_GITHUB/main^{commit}" 2> /dev/null || true); echo "$PF_G"; }

pf_check_gh() {
  local gh user admin bad=0
  gh=$(pf_tool gh)
  user=$("$gh" api user --jq .login 2> /dev/null) || user=''
  if [ "$user" != "$PF_OWNER" ]; then
    pf_fail "gh is logged in as '${user:-nobody}', not $PF_OWNER" - "gh auth login (or gh auth switch -u $PF_OWNER)"
    return 1
  fi
  admin=$("$gh" api "repos/$PF_REPO" --jq .permissions.admin 2> /dev/null) || admin=''
  if [ "$admin" != true ]; then
    pf_fail "gh's token has no admin rights on $PF_REPO (it said '${admin:-nothing}')" - "gh auth refresh -h github.com -s repo,workflow (as $PF_OWNER)"
    bad=1
  fi
  [ "$bad" = 1 ] || pf_ok "gh is $PF_OWNER, admin on $PF_REPO"
  return "$bad"
}

pf_check_github_ssh() {
  local ssh out url host
  ssh=$(pf_tool ssh)
  # The host the github remote pushes to (an ~/.ssh/config alias, perhaps), else github.com.
  url=$(pf_git remote get-url "$PF_GITHUB" 2> /dev/null || true)
  case $url in
    ssh://*) host=${url#ssh://}; host=${host%%/*}; host=${host#*@}; host=${host%%:*} ;;
    *@*:*) host=${url#*@}; host=${host%%:*} ;;
    *) host=github.com ;;
  esac
  out=$("$ssh" -T -o BatchMode=yes "git@$host" 2>&1 || true)
  case $out in
    *"Hi $PF_OWNER!"* | *"Hi $PF_OWNER "*) pf_ok "ssh to GitHub authenticates as $PF_OWNER" ;;
    *)
      pf_fail "ssh -T git@$host did not say 'Hi $PF_OWNER': $(printf '%s' "$out" | head -n 1 | head -c 200)" - "ssh-add your GitHub key (ssh-add -l lists the agent), and check ~/.ssh/config for $host"
      return 1
      ;;
  esac
}

pf_check_commit_signing() {
  local empty c e
  empty=$(pf_git hash-object -t tree /dev/null)
  e=$(mktemp "${TMPDIR:-/tmp}/fil-pf-sign.XXXXXX")
  if ! c=$(pf_git commit-tree -S "$empty" -m 'fil release preflight (dangling, harmless)' 2> "$e"); then
    pf_fail "git cannot sign a commit: $(head -n 1 "$e" | head -c 200)" - "set up commit signing (git config --global gpg.format ssh; git config --global user.signingkey ~/.ssh/KEY.pub) and ssh-add that key"
    rm -f "$e"
    return 1
  fi
  rm -f "$e"
  if ! pf_git cat-file commit "$c" | grep -q '^gpgsig'; then
    pf_fail "git commit-tree -S made an unsigned commit" - "check git config user.signingkey and gpg.format"
    return 1
  fi
  pf_ok "git signs commits (the tap's formula commit, step brew)"
}

pf_check_crates_env() {
  local gh self bad=0 out
  gh=$(pf_tool gh)
  if out=$(GITHUB_REPOSITORY=$PF_REPO FIL_GATE_GH=$gh "$RELEASE_DIR/crates-io-gate.sh" --environment-only 2>&1); then
    pf_ok "the crates-io environment requires $PF_OWNER and deploys from v* only"
  else
    pf_fail "$(printf '%s' "$out" | tail -n 1 | sed 's/^release: //')" "crates-io reviewers missing" \
      "GitHub, $PF_REPO, Settings, Environments, crates-io: add $PF_OWNER as a required reviewer and restrict deployments to tags v* (docs/m4/release.md section 10.1)"
    bad=1
  fi
  self=$("$gh" api "repos/$PF_REPO/environments/crates-io" --jq '[.protection_rules[]? | select(.type == "required_reviewers") | .prevent_self_review] | any' 2> /dev/null) || self=''
  case $self in
    false) pf_ok "self-review is allowed: you can approve your own tag's run" ;;
    *)
      pf_fail "the crates-io environment prevents self-review (or it cannot be read: '${self:-nothing}'): you could not approve your own tag's run" "crates-io reviewers missing" \
        "GitHub, $PF_REPO, Settings, Environments, crates-io: untick \"Prevent self-review\""
      bad=1
      ;;
  esac
  return "$bad"
}

pf_check_tag_ruleset() {
  local gh ids id inc found='' imm
  gh=$(pf_tool gh)
  ids=$("$gh" api "repos/$PF_REPO/rulesets" --jq '.[] | select(.target == "tag" and .enforcement == "active") | .id' 2> /dev/null) || {
    pf_fail "cannot list the rulesets of $PF_REPO" - "gh api repos/$PF_REPO/rulesets (as $PF_OWNER, admin)"
    return 1
  }
  for id in $ids; do
    inc=$("$gh" api "repos/$PF_REPO/rulesets/$id" --jq '.conditions.ref_name.include[]?' 2> /dev/null || true)
    case " $(printf '%s' "$inc" | tr '\n' ' ') " in *" refs/tags/v* "*) found=$id ;; esac
  done
  if [ -z "$found" ]; then
    pf_fail "no active tag ruleset on refs/tags/v* in $PF_REPO" - "GitHub, $PF_REPO, Settings, Rules, Rulesets: an active tag ruleset for v* that blocks updates and deletions, the owner bypassing (docs/m4/plan.md section 5)"
    return 1
  fi
  pf_ok "tag ruleset $found protects v*"
  imm=$("$gh" api "repos/$PF_REPO/immutable-releases" --jq .enabled 2>&1) || case $imm in *404* | *"Not Found"*) imm=false ;; *) imm='' ;; esac
  case $imm in
    true) pf_ok "releases are immutable" ;;
    false) pf_warn "immutable releases are off in $PF_REPO (Settings, General, Releases)" ;;
    *) pf_warn "could not read whether releases are immutable in $PF_REPO" ;;
  esac
}

pf_check_tag_free() {
  local gh curl t code out bad=0
  gh=$(pf_tool gh)
  curl=$(pf_tool curl)
  t=$(pf_git ls-remote "$PF_GITHUB" "refs/tags/$PF_TAG" 2> /dev/null) || {
    pf_fail "cannot list the tags of '$PF_GITHUB'" - "git ls-remote $PF_GITHUB"
    return 1
  }
  if [ -n "$t" ]; then
    pf_fail "$PF_TAG already exists on GitHub" - "this version is taken: bump to the next patch version (a PR on Gitea: just set-version X.Y.Z+1)"
    bad=1
  fi
  if out=$("$gh" release view "$PF_TAG" --repo "$PF_REPO" --json isDraft 2>&1); then
    pf_fail "GitHub already has a release (or draft) for $PF_TAG" - "a stale draft of a failed attempt: gh release delete $PF_TAG --repo $PF_REPO; a published one: bump to the next patch version"
    bad=1
  else
    case $out in
      *"release not found"*) ;;
      *) pf_warn "could not ask GitHub whether a release $PF_TAG exists ($(printf '%s' "$out" | head -n 1 | head -c 200)); the tag itself was checked" ;;
    esac
  fi
  code=$("$curl" -s -o /dev/null -w '%{http_code}' -A "fil-release-preflight (https://github.com/$PF_REPO)" "https://crates.io/api/v1/crates/fil-shell/$PF_VERSION" 2> /dev/null || true)
  case $code in
    404) ;;
    200)
      pf_fail "fil-shell $PF_VERSION is already on crates.io" - "this version is taken: bump to the next patch version"
      bad=1
      ;;
    *) pf_warn "could not ask crates.io whether fil-shell $PF_VERSION exists (HTTP '${code:-none}')" ;;
  esac
  [ "$bad" = 1 ] || pf_ok "$PF_TAG is free (no tag, no release, not on crates.io)"
  return "$bad"
}

pf_check_docker() {
  local docker
  docker=$(pf_tool docker)
  if "$docker" info > /dev/null 2>&1; then
    pf_ok "Docker is running on this Mac"
  else
    pf_fail "Docker is not running on this Mac" "Docker not started" "open -a Docker, wait until docker info answers"
    return 1
  fi
}

pf_check_vm() {
  local lima st arch binfmt image
  lima=$(pf_tool limactl)
  st=$("$lima" list --format '{{.Status}}' "$PF_VM" 2> /dev/null || true)
  if [ "$st" != Running ]; then
    pf_fail "Lima instance '$PF_VM' is ${st:-missing}, not Running" "VM not running" "limactl start $PF_VM (no VM yet: docs/dev/vm.md)"
    return 1
  fi
  if ! "$lima" shell "$PF_VM" docker info > /dev/null 2>&1; then
    pf_fail "Docker cannot be reached in VM $PF_VM as your user" "VM docker group" "limactl stop $PF_VM && limactl start $PF_VM (a new VM puts you in the docker group only after one restart; docs/dev/vm.md)"
    return 1
  fi
  # Rosetta, not qemu: an amd64 container says x86_64 under both, but the qemu-user of Ubuntu 24.04
  # cannot run rustc (section 9.3). So the binfmt registrations are read, not `uname -m`.
  binfmt=$("$lima" shell "$PF_VM" sh -c 'printf "rosetta=%s qemu=%s\n" "$(head -n 1 /proc/sys/fs/binfmt_misc/rosetta 2> /dev/null)" "$(head -n 1 /proc/sys/fs/binfmt_misc/qemu-x86_64 2> /dev/null)"' 2> /dev/null | tr -d '\r' | tail -n 1 || true)
  case $binfmt in
    "rosetta=enabled qemu=enabled")
      pf_fail "VM $PF_VM registers both Rosetta and qemu-x86_64 for amd64 binaries: qemu may run them, and it cannot run rustc" "VM without Rosetta" "in the VM: sudo sh -c 'echo -1 > /proc/sys/fs/binfmt_misc/qemu-x86_64' (and sudo apt remove qemu-user-static qemu-user-binfmt so it stays gone)"
      return 1
      ;;
    "rosetta=enabled qemu="*) ;;
    *)
      pf_fail "VM $PF_VM has no enabled Rosetta binfmt registration ($binfmt): amd64 runs under qemu or not at all" "VM without Rosetta" "enable Rosetta in the VM: limactl stop $PF_VM; limactl edit $PF_VM (rosetta enabled and binfmt, docs/m4/release.md section 9.3); limactl start $PF_VM"
      return 1
      ;;
  esac
  # A container run, only from the x86_64 cross image if the VM already has it (nothing is pulled).
  image=$(tools_image 2> /dev/null || true)
  if [ -n "$image" ] && "$lima" shell "$PF_VM" docker image inspect "$image" > /dev/null 2>&1; then
    arch=$("$lima" shell "$PF_VM" docker run --rm --pull=never --network none --platform linux/amd64 "$image" uname -m 2> /dev/null | tr -d '\r' | tail -n 1 || true)
    if [ "$arch" != x86_64 ]; then
      pf_fail "the cross image does not run as amd64 in VM $PF_VM (it said '${arch:-nothing}')" "VM without Rosetta" "limactl stop $PF_VM; limactl start $PF_VM (re-registers Rosetta); docs/m4/release.md section 9.3"
      return 1
    fi
  else
    pf_note "the cross image is not in VM $PF_VM yet (the rebuild pulls it): no test container run"
  fi
  pf_ok "VM $PF_VM runs, its Docker answers, amd64 runs under Rosetta"
}

# pf_signer_namespaces FILE KEY: the namespaces FILE lists for KEY ("type base64"), one per line.
pf_signer_namespaces() {
  awk -v want="$2" '
    /^[[:space:]]*(#|$)/ { next }
    {
      for (i = 2; i <= NF - 2; i++) if ($i ~ /^namespaces="/) {
        if ($(i + 1) " " $(i + 2) == want) {
          ns = $i; sub(/^namespaces="/, "", ns); sub(/"$/, "", ns)
          n = split(ns, a, ","); for (j = 1; j <= n; j++) print a[j]
        }
      }
    }' "$1"
}
pf_check_release_key() {
  local key r g s ns bad=0 where fp at
  if [ ! -f "$PF_KEY_PUB" ]; then
    pf_fail "no release public key at $PF_KEY_PUB" "lost release key" "restore ~/.ssh/fil-release and its .pub from your backup (FIL_RELEASE_KEY names another path); a lost key is rotated by a PR to .github/release-signers"
    return 1
  fi
  key=$(cut -d' ' -f1,2 "$PF_KEY_PUB")
  fp=$(ssh-keygen -lf "$PF_KEY_PUB" 2> /dev/null | cut -d' ' -f2)
  r=$(pf_r)
  g=$(pf_g)
  s=$(mktemp "${TMPDIR:-/tmp}/fil-pf-signers.XXXXXX")
  for where in R G; do
    if [ "$where" = G ]; then
      [ -n "$g" ] || { pf_note "github/main not read: the release key is checked on the release commit only"; break; }
      # verify-tag.sh reads the signers on GitHub's main: R's own once main is R or behind it (the
      # mirror makes it R), the later commit's once main has moved on past R
      if [ "$g" = "$r" ] || ! pf_git merge-base --is-ancestor "$r" "$g" 2> /dev/null; then
        break
      fi
      # past R: main must be Gitea's and keep R's file (pf_main_trusted), not merely list the key
      pf_main_trusted "$r" "$g" || { bad=1; break; }
    fi
    pf_git show "$([ "$where" = R ] && echo "$r" || echo "$g"):.github/release-signers" > "$s" 2> /dev/null || : > "$s"
    ns=" $(pf_signer_namespaces "$s" "$key" | tr '\n' ' ')"
    at=$([ "$where" = R ] && echo "the release commit $r" || echo "github/main $g")
    if case $ns in *" git "*) false ;; esac || case $ns in *" fil-release "*) false ;; esac; then
      pf_fail "$fp ($PF_KEY_PUB) is not listed for both git and fil-release in .github/release-signers at $at (it has:${ns% })" "rotated or lost release key" \
        "the key you sign with must be the one on main: a PR to .github/release-signers (owner merges), or point FIL_RELEASE_KEY at the listed key"
      bad=1
    else
      pf_ok "$fp ($PF_KEY_PUB) signs git and fil-release in .github/release-signers at $at"
    fi
  done
  rm -f "$s"
  return "$bad"
}

pf_check_package_key() {
  local gpg r k fpr line valid exp now bad=0
  gpg=$(pf_tool gpg)
  r=$(pf_r)
  k=$(mktemp "${TMPDIR:-/tmp}/fil-pf-pkgkey.XXXXXX")
  if [ -n "${FIL_PACKAGE_KEY_FILE:-}" ]; then cp "$FIL_PACKAGE_KEY_FILE" "$k"; else pf_git show "$r:.github/package-signing-key.asc" > "$k" 2> /dev/null || : > "$k"; fi
  fpr=$(FIL_GPG=$gpg package_key_fingerprint "$k") || fpr=""
  rm -f "$k"
  if [ -z "$fpr" ]; then
    pf_fail ".github/package-signing-key.asc at the release commit is not exactly one OpenPGP key" - "a PR on Gitea with the package key's public half (docs/m4/release.md section 10.4)"
    return 1
  fi
  line=$("$gpg" --list-secret-keys --with-colons "$fpr" 2> /dev/null | awk -F: '$1 == "sec" { print; exit }') || line=''
  if [ -z "$line" ]; then
    pf_fail "the package key $fpr is not a secret key in your keyring" "lost package key" "gpg --import your backup of the secret key (gpg -K $fpr then lists it); if it is gone, rotate it (a PR to .github/package-signing-key.asc, docs/m4/release.md section 10.4)"
    return 1
  fi
  valid=$(printf '%s' "$line" | cut -d: -f2)
  exp=$(printf '%s' "$line" | cut -d: -f7)
  now=$(date +%s)
  case $valid in
    r)
      pf_fail "the package key $fpr is revoked" - "rotate it: a PR to .github/package-signing-key.asc (docs/m4/release.md section 10.4)"
      return 1
      ;;
  esac
  if [ "$valid" = e ] || { [ -n "$exp" ] && [ "$exp" -le "$now" ]; }; then
    pf_fail "the package key $fpr has expired" "expired package key" "gpg --quick-set-expire $fpr 2y, then a PR to .github/package-signing-key.asc with the new public key (gpg --armor --export $fpr)"
    return 1
  fi
  if [ -n "$exp" ] && [ "$exp" -lt $((now + PF_KEY_WARN_DAYS * 86400)) ]; then
    pf_warn "the package key $fpr expires in $(((exp - now) / 86400)) days: extend it (gpg --quick-set-expire $fpr 2y) and update .github/package-signing-key.asc"
  fi
  pf_ok "the package key $fpr is a secret key here${exp:+, expires $(date -r "$exp" +%Y-%m-%d 2> /dev/null || date -d "@$exp" +%Y-%m-%d 2> /dev/null || echo "$exp")}"
  return "$bad"
}

pf_check_apple() {
  local sec cs id profile bad=0 s src
  local -a notarytool
  sec=$(pf_tool security)
  cs=$(pf_tool codesign)
  id=${FIL_CODESIGN_IDENTITY:-$DEFAULT_CODESIGN_IDENTITY}
  profile=${FIL_NOTARY_PROFILE:-fil-notary}
  read -r -a notarytool <<< "${FIL_NOTARYTOOL:-xcrun notarytool}"
  if "$sec" find-identity -v -p codesigning 2> /dev/null | grep -qF "\"$id\""; then
    pf_ok "the keychain has \"$id\""
  else
    pf_fail "no valid codesigning identity \"$id\" in the keychain" - "import the Developer ID Application certificate and its key (Xcode, Settings, Accounts; or the .p12 backup); security find-identity -v -p codesigning lists it"
    bad=1
  fi
  if "${notarytool[@]}" history --keychain-profile "$profile" > /dev/null 2>&1; then
    pf_ok "notarytool's profile $profile works"
  else
    pf_fail "xcrun notarytool history --keychain-profile $profile failed" - "xcrun notarytool store-credentials $profile (docs/m4/release.md section 10.3)"
    bad=1
  fi
  [ "$bad" = 0 ] || return 1
  # A test signature of a scratch copy of a tiny binary: the keychain's "Always Allow" dialog and the
  # timestamp server now, not in the middle of the release.
  s=$(mktemp -d "${TMPDIR:-/tmp}/fil-pf-cs.XXXXXX")
  src=/usr/bin/true
  [ -f "$src" ] || src=/bin/true
  cp "$src" "$s/true"
  if "$cs" --force --timestamp --options runtime --sign "$id" "$s/true" > "$s/log" 2>&1; then
    pf_ok "a test codesign with a secure timestamp works"
  else
    pf_fail "a test codesign failed: $(head -n 1 "$s/log" | head -c 200)" - "security unlock-keychain login.keychain-db (over SSH the keychain is locked), then answer the keychain dialog with Always Allow"
    bad=1
  fi
  rm -rf "$s"
  return "$bad"
}

# pf_run CHECK...: runs the checks, prints a summary, returns the number that failed. The override
# check runs first whatever is asked, and a failure there stops the rest.
pf_run() {
  local c failed=0 n=0 list=''
  for c in "$@"; do [ "$c" = overrides ] || list="$list $c"; done
  PF_SELECTED="overrides$list"
  for c in $PF_SELECTED; do
    case " $PF_CHECKS " in *" $c "*) ;; *) echo "preflight: no check '$c' (one of: $PF_CHECKS)" >&2; return 1 ;; esac
  done
  for c in $PF_SELECTED; do
    PF_CUR=$c
    n=$((n + 1))
    if [ "$c" = tools ]; then
      pf_check_tools_run || failed=$((failed + 1))
    elif ! "pf_check_$(printf '%s' "$c" | tr - _)"; then
      failed=$((failed + 1))
      if [ "$c" = overrides ]; then
        echo "preflight: stopped: test-only overrides are set" >&2
        return "$failed"
      fi
    fi
  done
  if [ "$failed" = 0 ]; then
    echo "preflight: all $n checks passed for $PF_VERSION" >&2
  else
    echo "preflight: $failed of $n checks FAILED for $PF_VERSION (each says what to do above)" >&2
  fi
  return "$failed"
}

pf_list() {
  local s
  echo "checks (in order): $PF_CHECKS"
  echo "every run: $PF_ALWAYS"
  for s in $PF_STEPS; do printf '%-17s %s\n' "$s" "$(pf_step_checks "$s")"; done
}

pf_main() {
  local version='' from='' only='' checks
  while [ $# -gt 0 ]; do
    case $1 in
      --from) from=$2; shift 2 ;;
      --only) only=$2; shift 2 ;;
      --commit) PF_COMMIT=$2; shift 2 ;;
      --repo) PF_REPO=$2; shift 2 ;;
      --vm) PF_VM=$2; shift 2 ;;
      --origin) PF_ORIGIN=$2; shift 2 ;;
      --github) PF_GITHUB=$2; shift 2 ;;
      --list) pf_list; return 0 ;;
      -*) die "unknown option: $1" ;;
      *) version=$1; shift ;;
    esac
  done
  [ -n "$version" ] || die "usage: release-preflight.sh VERSION [--from STEP] [--only CHECKS] [--commit SHA] [--repo OWNER/REPO] [--vm NAME] (--list: the checks and steps)"
  package_version_ok "${version#v}" || die "'$version' is not X.Y.Z or X.Y.Z-PRE"
  if [ -n "$PF_COMMIT" ] && ! [[ $PF_COMMIT =~ ^[0-9a-f]{40}$ ]]; then die "--commit must be a full 40-hex commit SHA"; fi
  pf_init "$version"
  if [ -n "$only" ]; then
    checks=$(printf '%s' "$only" | tr ',' ' ')
  else
    checks=$(pf_checks_from "${from:-candidate}") || die "no step '$from' (one of: $PF_STEPS)"
    if [ -z "$PF_COMMIT" ] && pf_step_after_tag "$from"; then
      PF_COMMIT=$(pf_git rev-parse -q --verify 'HEAD^{commit}' 2> /dev/null || true)
      log "a resume after the tag: bound to HEAD $PF_COMMIT (pass --commit to name the release commit)"
    fi
  fi
  # shellcheck disable=SC2086 # a word list
  pf_run $checks || exit 1
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  pf_main "$@"
fi
