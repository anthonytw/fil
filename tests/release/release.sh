#!/usr/bin/env bash
# `just release X.Y.Z`: the one-command, resumable release (docs/m4/release.md section 9.5). This
# version is the state machine, DETECT-ONLY: it binds a run to the version and the release commit,
# takes the lock, runs the preflight, and works out from the world (git refs, GitHub's runs and
# releases, crates.io, the Homebrew tap) which of the seventeen steps are done. It acts on nothing:
# where a step's action would run, it stops and says what to do by hand. The actions arrive in later
# PRs (4: mirror, dry-run, dry-verify; 5: tag to verify-signed; 6: publish to finish).
#
#   release.sh VERSION [--status] [--reset-state] [--repo OWNER/REPO] [--origin REMOTE]
#                      [--github REMOTE] [--tap DIR] [--vm NAME]
#
#   (default)      fetch `origin` and `github` (no tags), bind the state to VERSION and HEAD, take
#                  the lock, detect every step, record the identifiers found (the mirror commit,
#                  run ids, the tag object) in the state, run the preflight for the next step and
#                  the ones after it, then stop at that step. Exit 0: every step through `brew` is
#                  done; 1: failed (a preflight check, a burned tag, a step that could not be
#                  detected); 2: paused, the next step needs a human (in this version: always).
#   --status       per step: done (checked), done (recorded), done (implied), next or pending, with
#                  the run URLs, and whether the release keys are unlocked (release-keys.sh status).
#                  Read-only: no fetch of the clone (the tap is fetched), no lock, no state written,
#                  no preflight. With no state bound and HEAD not the release commit of a tag on
#                  GitHub, it shows the commit on origin's main with the tag's tree. Exit 1 only for
#                  a burned tag.
#   --reset-state  forget the state (another release commit before the tag); refused once the tag
#                  is on GitHub, which binds the commit forever.
#   --tap DIR      the Homebrew tap clone (default ~/dev/homebrew-tap; its `origin` is the tap).
#
# The state: target/release-verify/VERSION/release.state, `key=value` lines written atomically
# (temp file and mv), only identifiers the world cannot be asked for cheaply and the results of
# purely local checks. Never a secret, and not the gpg reaper (release-keys.sh keeps it in
# ~/.cache/fil/release-keys/). Real state beats the state file: every recorded identifier is
# checked against the world before it counts.
#
# A step that does not detect as done is "done (implied)" when a later step does: the steps are a
# pipeline, each detection includes the evidence of the earlier steps it consumes (a recorded
# dry-verify counts only with the dry run and both checks green), and the steps after `tag` are only
# looked at once the tag is there (after `publish`, once it is published).
#
# A tag on GitHub is BURNED (a hard stop: bump the version) when it does not verify, when the state
# binds a release commit of another tree, or when it is not the tag object the state recorded. A
# good tag of another tree with nothing bound means this checkout is not the release commit: the
# run says which commit to check out and records nothing.
#
# gh is only READ here (run list/view, release view/download, api GETs). The tools, each a test-only
# override refused outside the self-test (FIL_RELEASE_SELFTEST=1): FIL_GH, FIL_GPG, FIL_CARGO,
# FIL_CRATES_CURL, FIL_RELEASE_ROOT, and the preflight's (release-preflight.sh). Written for bash 3.2.
set -euo pipefail
# shellcheck source=tests/release/release-preflight.sh
source "$(dirname "${BASH_SOURCE[0]}")/release-preflight.sh"

usage="usage: release.sh VERSION [--status] [--reset-state] [--repo OWNER/REPO] [--origin REMOTE] [--github REMOTE] [--tap DIR] [--vm NAME]"
version='' mode=run reset=0 repo=anthonytw/fil origin=origin github=github tap=$HOME/dev/homebrew-tap vm=''
while [ $# -gt 0 ]; do
  case $1 in
    --status) mode=status; shift ;;
    --reset-state) reset=1; shift ;;
    --repo | --origin | --github | --tap | --vm)
      [ $# -ge 2 ] || die "$usage"
      case $1 in --repo) repo=$2 ;; --origin) origin=$2 ;; --github) github=$2 ;; --tap) tap=$2 ;; --vm) vm=$2 ;; esac
      shift 2
      ;;
    --redo | --from | --confirm-each | --lock-keys | --ttl) die "$1: not in this version of the command (it detects, and acts on nothing yet: the actions and their options arrive in PRs 4-6 of the one-command release)" ;;
    -*) die "unknown option: $1 ($usage)" ;;
    *) [ -z "$version" ] || die "$usage"; version=$1; shift ;;
  esac
done
[ -n "$version" ] || die "$usage"
V=${version#v}
package_version_ok "$V" || die "'$version' is not X.Y.Z or X.Y.Z-PRE"
[ "$reset" = 0 ] || [ "$mode" = run ] || die "--reset-state and --status do not go together"
TAG=v$V
W=$ROOT/target/release-verify/$V
STATE=$W/release.state
LOCK=$W/.lock
AGAIN="just release $V"

# Every test-only override: a NOTE each, and refused outside the self-test.
test_override_notes
if [ "${FIL_RELEASE_SELFTEST:-}" != 1 ]; then
  for v in $TEST_OVERRIDES; do
    [ "$v" != FIL_RELEASE_SELFTEST ] || continue
    [ -z "${!v:-}" ] || die "STOP: $v is set (a test-only override): a release that asks a fake proves nothing. Unset $v."
  done
fi

GH=${FIL_GH:-gh}
GPG=${FIL_GPG:-gpg}
CARGO=${FIL_CARGO:-cargo}
CURL=${FIL_CRATES_CURL:-curl}

scratch=$(mktemp -d "${TMPDIR:-/tmp}/fil-release.XXXXXX")
LOCK_OWNED=0
cleanup() {
  rm -rf "$scratch"
  if [ "$LOCK_OWNED" = 1 ] && [ "$(cat "$LOCK/pid" 2> /dev/null || true)" = "$$" ]; then rm -rf "$LOCK"; fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

rg() { git -C "$ROOT" "$@"; }
is_hex40() { case $1 in [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]*) [ ${#1} = 40 ] && case $1 in *[!0-9a-f]*) false ;; esac ;; *) false ;; esac; }
is_num() { case $1 in "" | *[!0-9]* | ????????????????????*) false ;; *) true ;; esac; }
short() { printf '%s' "$1" | cut -c1-12; }
tree_of() { rg rev-parse -q --verify "$1^{tree}" 2> /dev/null || true; }
has_commit() { [ -n "$1" ] && rg cat-file -e "$1^{commit}" 2> /dev/null; }

# ---- the state file
STATE_KEYS="version release_commit github_commit dry_run checks_linux checks_macos dry_verify tag_object tag_run verify_published"
# st_get KEY: the value of the last KEY= line ("" if none).
st_get() {
  [ -f "$STATE" ] || return 0
  awk -v k="$1" 'index($0, k "=") == 1 { v = substr($0, length(k) + 2) } END { if (v != "") print v }' "$STATE"
}
# st_set KEY VALUE: rewrite the file with KEY=VALUE (atomically). Only the keys above, and values of
# plain characters: the state holds identifiers and results, never anything else.
st_set() {
  case " $STATE_KEYS " in *" $1 "*) ;; *) die "internal: no state key '$1'" ;; esac
  case $2 in *[!A-Za-z0-9._:\ -]*) die "internal: refusing to write '$2' to the state" ;; esac
  [ "$(st_get "$1")" != "$2" ] || return 0
  mkdir -p "$W"
  {
    if [ -f "$STATE" ]; then awk -v k="$1" 'index($0, k "=") != 1' "$STATE"; fi
    printf '%s=%s\n' "$1" "$2"
  } > "$STATE.tmp.$$"
  mv "$STATE.tmp.$$" "$STATE"
}
st_hex() { local v; v=$(st_get "$1"); if is_hex40 "$v"; then echo "$v"; fi; }
st_num() { local v; v=$(st_get "$1"); if is_num "$v"; then echo "$v"; fi; }

# ---- output
step_no() { local s i=0; for s in $PF_STEPS; do i=$((i + 1)); [ "$s" = "$1" ] && { echo $i; return; }; done; }
step_name() { local s i=0; for s in $PF_STEPS; do i=$((i + 1)); [ $i = "$1" ] && { echo "$s"; return; }; done; }
NSTEPS=$(set -- $PF_STEPS; echo $#)
LAST_DONE=0
then_line() {
  if [ "$LAST_DONE" -gt 0 ]; then
    printf '  then:  %s          (steps 1-%s are done and will be skipped)\n' "$AGAIN" "$LAST_DONE"
  else
    printf '  then:  %s\n' "$AGAIN"
  fi
}
# report KIND N WHAT DO: the one failure (or pause) format; N=0 for no step. Exits 1 (FAILED) or 2.
report() {
  local where=''
  [ "$2" = 0 ] || where=" at step $2/$NSTEPS $(step_name "$2")"
  {
    echo "release: $1$where"
    echo "  what:  $3"
    echo "  do:    $4"
    then_line
  } >&2
  if [ "$1" = PAUSED ]; then exit 2; fi
  exit 1
}

# ---- the lock (run mode): a directory holding the pid; a dead pid's lock is removed with a note.
lock_take() {
  local pid n=0
  mkdir -p "$W"
  until mkdir "$LOCK" 2> /dev/null; do
    pid=$(cat "$LOCK/pid" 2> /dev/null || true)
    if [ -z "$pid" ] && [ $n -lt 3 ]; then
      n=$((n + 1))
      sleep 1
      continue
    fi
    if [ -n "$pid" ] && kill -0 "$pid" 2> /dev/null; then
      report FAILED 0 "another release run (pid $pid) holds $LOCK" "wait for it, or stop it (kill $pid)"
    fi
    # Move it aside (one mover wins), then check it is still the dead run's: a run that took the lock
    # meanwhile gets it back.
    if mv "$LOCK" "$LOCK.stale.$$" 2> /dev/null; then
      if [ "$(cat "$LOCK.stale.$$/pid" 2> /dev/null || true)" = "$pid" ]; then
        log "removed a stale lock (pid ${pid:-unknown} is not running): $LOCK"
        rm -rf "$LOCK.stale.$$"
      elif ! mv "$LOCK.stale.$$" "$LOCK" 2> /dev/null; then
        rm -rf "$LOCK.stale.$$"
        report FAILED 0 "two release runs raced for $LOCK" "run it again"
      fi
    fi
    n=0
  done
  echo $$ > "$LOCK/pid"
  LOCK_OWNED=1
}

# ---- GitHub reads. Each prints its result; a failure returns 1 with the error in $scratch/gh.err.
gh_read() { "$GH" "$@" 2> "$scratch/gh.err"; }
gh_err() { head -n 1 "$scratch/gh.err" 2> /dev/null | cut -c1-200; }
run_url() { printf 'https://github.com/%s/actions/runs/%s' "$repo" "$1"; }
# run_ids WORKFLOW EVENT SHA [BRANCH]: "id status conclusion" per run, newest first.
run_ids() {
  local out
  out=$(gh_read run list --repo "$repo" --workflow "$1" --event "$2" --commit "$3" ${4:+--branch "$4"} --json databaseId,status,conclusion,createdAt --limit 20) || return 1
  printf '%s' "$out" | python3 -I -c 'import json, sys
runs = json.load(sys.stdin)
runs.sort(key=lambda r: (r.get("createdAt") or "", r.get("databaseId") or 0), reverse=True)
for r in runs:
    print(r["databaseId"], r.get("status") or "-", r.get("conclusion") or "-")'
}
# run_view ID: $scratch/run-ID holds "headSha X", "event X", "workflowName X", "status X",
# "conclusion X" and "job CONCLUSION NAME" lines (cached).
run_view() {
  local f=$scratch/run-$1 out
  [ -f "$f" ] && return 0
  out=$(gh_read run view "$1" --repo "$repo" --json headSha,event,workflowName,status,conclusion,jobs) || return 1
  printf '%s' "$out" | python3 -I -c 'import json, sys
r = json.load(sys.stdin)
for k in ("headSha", "event", "workflowName", "status", "conclusion"):
    print(k, r.get(k) or "-")
for j in r.get("jobs") or []:
    print("job", j.get("conclusion") or "-", j.get("name") or "")' > "$f.tmp" || return 1
  mv "$f.tmp" "$f"
}
rv_has() { grep -qx -- "$2" "$scratch/run-$1"; }
rv_job_prefix() { grep -q "^job success $2" "$scratch/run-$1"; }
rv_no_failed_job() { ! grep -Eq '^job (failure|cancelled|timed_out|action_required|startup_failure) ' "$scratch/run-$1"; }
# The runs each step needs, as predicates on a viewed run.
pred_dry() { rv_has "$1" "workflowName release" && rv_has "$1" "event workflow_dispatch" && rv_has "$1" "headSha $G" && rv_has "$1" "conclusion success"; }
# suite=all is not in the run's metadata (inputs are not reported): its jobs are. linux-check runs
# `check` with aarch64 and terminals only for suite=all; macos-check runs check and remote together
# only for suite=all. The workflows' `ref` input is not reported either: a run dispatched on main
# (head G) with `-f ref=OTHER` tested OTHER, and only the run ids the command dispatches itself and
# records (PR 4) close that gap; a discovered run is trusted to have tested its head.
pred_linux_all() {
  rv_has "$1" "workflowName linux-check" && rv_has "$1" "event workflow_dispatch" && rv_has "$1" "headSha $G" && rv_has "$1" "conclusion success" \
    && rv_has "$1" "job success check" && rv_has "$1" "job success aarch64" && rv_has "$1" "job success terminals" && rv_no_failed_job "$1"
}
pred_macos_all() {
  rv_has "$1" "workflowName macos-check" && rv_has "$1" "event workflow_dispatch" && rv_has "$1" "headSha $G" && rv_has "$1" "conclusion success" \
    && rv_job_prefix "$1" "check (" && rv_job_prefix "$1" "remote (" && rv_no_failed_job "$1"
}
pred_tag_run() { rv_has "$1" "workflowName release" && rv_has "$1" "event push" && rv_has "$1" "headSha $G" && rv_has "$1" "job success publish"; }
# find_run PRED RECORDED WORKFLOW EVENT SHA [BRANCH]: the recorded run if it satisfies PRED, else the
# newest of the listed runs that does (at most five are looked at). Prints the id; returns 1 if
# none, 2 if GitHub could not be asked.
find_run() {
  local pred=$1 rec=$2 id st concl n=0 ids
  if [ -n "$rec" ] && run_view "$rec" && "$pred" "$rec"; then
    echo "$rec"
    return 0
  fi
  ids=$(run_ids "$3" "$4" "$5" ${6:+"$6"}) || return 2
  while read -r id st concl; do
    [ -n "$id" ] || continue
    is_num "$id" || continue
    [ "$pred" = pred_tag_run ] || [ "$concl" = success ] || continue
    n=$((n + 1))
    [ $n -le 5 ] || break
    run_view "$id" || return 2
    if "$pred" "$id"; then
      echo "$id"
      return 0
    fi
  done <<< "$ids"
  return 1
}

# ---- the release on GitHub: REL is none, unknown, draft or published; $scratch/assets holds
# "name digest" lines (digest "-" when this gh does not report digests).
REL=unknown
read_release() {
  local out
  if ! out=$(gh_read release view "$TAG" --repo "$repo" --json isDraft,assets); then
    case $(cat "$scratch/gh.err") in *"release not found"*) REL=none ;; *) REL=unknown ;; esac
    return 0
  fi
  printf '%s' "$out" | python3 -I -c 'import json, sys
d = json.load(sys.stdin)
print("draft" if d["isDraft"] else "published")
for a in d["assets"]:
    print(a["name"], (a.get("digest") or "-").replace("sha256:", ""))' > "$scratch/release" || { REL=unknown; return 0; }
  REL=$(head -n 1 "$scratch/release")
  tail -n +2 "$scratch/release" > "$scratch/assets"
}
# asset_digest NAME: the sha256 of the release's asset NAME (downloaded and hashed when the listing
# has no digest); nothing if there is no such asset or it cannot be read.
asset_digest() {
  local d
  d=$(awk -v n="$1" '$1 == n { print $2; exit }' "$scratch/assets" 2> /dev/null || true)
  [ -n "$d" ] || return 0
  if [ "$d" = - ]; then
    mkdir -p "$scratch/dl"
    [ -f "$scratch/dl/$1" ] || gh_read release download "$TAG" --repo "$repo" -p "$1" -D "$scratch/dl" > /dev/null || return 0
    [ -f "$scratch/dl/$1" ] && sha256 "$scratch/dl/$1"
    return 0
  fi
  echo "$d"
}
# draft_sums: DRAFT_SUMS names a copy of the release's SHA256SUMS (the local one if it is the same).
DRAFT_SUMS=''
draft_sums() {
  local d
  d=$(asset_digest SHA256SUMS)
  [ -n "$d" ] || return 1
  if [ "$(local_sha SHA256SUMS)" = "$d" ]; then DRAFT_SUMS=$FILES/SHA256SUMS; return 0; fi
  mkdir -p "$scratch/sums"
  [ -f "$scratch/sums/SHA256SUMS" ] || gh_read release download "$TAG" --repo "$repo" -p SHA256SUMS -D "$scratch/sums" > /dev/null || return 1
  [ "$(sha256 "$scratch/sums/SHA256SUMS")" = "$d" ] || return 1
  DRAFT_SUMS=$scratch/sums/SHA256SUMS
}
has_asset() { awk -v n="$1" '$1 == n { f = 1 } END { exit !f }' "$scratch/assets" 2> /dev/null; }
# The release files of the version (section 4.1): the five archives and four packages SHA256SUMS lists.
release_files() {
  { printf '%s\n' "fil-$V-aarch64-linux-musl.tar.gz" "fil-$V-source.tar.gz" "fil-$V-universal-apple-darwin.tar.gz" "$(ci_archive_name "$V")" "fil-$V-x86_64-linux-musl.tar.gz"; package_names "$V"; } | LC_ALL=C sort
}
MAC=fil-$V-universal-apple-darwin.tar.gz
MACCI=$(ci_archive_name "$V")
RPMS="$(package_file rpm x86_64 "$V") $(package_file rpm aarch64 "$V")"
FILES=$W/files
verified_has() { grep -qx -- "$1" "$W/verified" 2> /dev/null; }
local_sha() { if [ -f "$FILES/$1" ]; then sha256 "$FILES/$1"; fi; }

# ---- the detections. Each returns 0 (done) or 1 (not done) and sets NOTE; UNKNOWN=1 when the world
# could not be asked; HARD (a message) when the world is in a state no re-run can fix.
NOTE='' UNKNOWN=0 HARD='' HARD_STEP='' NEXT=0
R='' G='' GMAIN='' TAG_OBJ='' TAG_COMMIT='' TAG_OK=0
DRY_RUN='' CHECKS_LINUX='' CHECKS_MACOS='' TAG_RUN=''

det_candidate() {
  local log=$scratch/candidate.log
  pf_init "$V"
  PF_REPO=$repo PF_ORIGIN=$origin PF_GITHUB=$github PF_AGAIN=$AGAIN PF_LOCK_OWNER=$$ PF_R='' PF_G=''
  [ -z "$vm" ] || PF_VM=$vm
  PF_COMMIT=''
  [ "$TAG_OK" = 0 ] || PF_COMMIT=$R
  if (PF_CUR=tree && pf_check_tree && PF_CUR=version && pf_check_version && PF_CUR=lockfiles && pf_check_lockfiles) 2> "$log"; then
    NOTE="R $(short "$R"): clean, at the version, lockfiles current"
    return 0
  fi
  NOTE=$(grep -m1 '^preflight: FAIL' "$log" | sed 's/^preflight: FAIL  //' | cut -c1-200)
  return 1
}

# The tag on GitHub, read into a scratch repository that borrows the clone's objects (nothing in the
# clone changes; Gitea's marker tag may own vV there), and verified with verify-tag.sh against the
# signers on GitHub's main.
TAG_STATE=unknown TAG_MSG='' TAG_TREE='' TAG_FOUND='' TAG_FIX=''
read_tag() {
  local lines url d objs out
  if ! lines=$(rg ls-remote "$github" "refs/tags/$TAG" 2> "$scratch/git.err"); then
    TAG_STATE=unknown TAG_MSG="cannot list the tags of '$github': $(head -n 1 "$scratch/git.err" | cut -c1-200)"
    return 0
  fi
  if [ -z "$lines" ]; then
    TAG_STATE=none TAG_MSG="no $TAG on GitHub"
    return 0
  fi
  TAG_OBJ=$(printf '%s\n' "$lines" | awk -v r="refs/tags/$TAG" '$2 == r { print $1; exit }')
  url=$(rg remote get-url "$github")
  d=$scratch/tagcheck
  objs=$(cd "$ROOT" && cd "$(git rev-parse --git-common-dir)" && pwd)/objects
  git init -q --bare "$d"
  echo "$objs" > "$d/objects/info/alternates"
  if ! git -C "$d" fetch -q --no-tags "$url" "+refs/tags/$TAG:refs/tags/$TAG" "+refs/heads/main:refs/remotes/origin/main" 2> "$scratch/git.err"; then
    TAG_STATE=unknown TAG_MSG="cannot fetch $TAG from '$github': $(head -n 1 "$scratch/git.err" | cut -c1-200)"
    return 0
  fi
  TAG_COMMIT=$(git -C "$d" rev-parse "refs/tags/$TAG^{commit}")
  if ! out=$("$RELEASE_DIR/verify-tag.sh" "$d" "$TAG" refs/remotes/origin/main 2>&1); then
    TAG_STATE=burned TAG_MSG="$TAG on GitHub does not verify: $(printf '%s' "$out" | tail -n 1 | cut -c1-240)"
  elif [ -n "$(st_hex tag_object)" ] && [ "$(st_hex tag_object)" != "$TAG_OBJ" ]; then
    TAG_STATE=burned TAG_MSG="$TAG on GitHub is tag object $(short "$TAG_OBJ"), not the $(short "$(st_hex tag_object)") this release pushed"
  elif TAG_TREE=$(git -C "$d" rev-parse "$TAG_COMMIT^{tree}") && [ "$TAG_TREE" != "$(tree_of "$R")" ]; then
    if [ "$(st_hex release_commit)" = "$R" ]; then
      # The state binds R: the tag was made from something else.
      TAG_STATE=burned TAG_MSG="$TAG on GitHub names $(short "$TAG_COMMIT"), whose tree is not that of the release commit $(short "$R") the state is bound to"
    else
      # Nothing is bound: the tag is fine, this checkout is not its release commit.
      TAG_FOUND=$(rg log --format='%H %T' "refs/remotes/$origin/main" 2> /dev/null | awk -v t="$TAG_TREE" '$2 == t { print $1; exit }' || true)
      TAG_STATE=mismatch TAG_MSG="$TAG on GitHub names $(short "$TAG_COMMIT") (tree $(short "$TAG_TREE")), and HEAD $(short "$R") is not its release commit"
      if [ -n "$TAG_FOUND" ]; then
        TAG_FIX="git checkout $TAG_FOUND (the commit on $origin/main with the tag's tree)"
      else
        TAG_FIX="check out the release commit: git log --format='%H %T' $origin/main | awk '\$2 == \"$TAG_TREE\"'"
      fi
    fi
  else
    TAG_STATE=ok TAG_MSG="$TAG (tag object $(short "$TAG_OBJ")) is signed by a release signer on GitHub's main and names $(short "$TAG_COMMIT")"
    TAG_OK=1
  fi
}

det_mirror() {
  local sg
  sg=$(st_hex github_commit)
  if [ "$TAG_OK" = 1 ]; then
    G=$TAG_COMMIT
    NOTE="G $(short "$G"): the tag names it, and its tree is the release commit's"
    return 0
  fi
  if [ -z "$GMAIN" ]; then
    UNKNOWN=1 NOTE="cannot read main on '$github'"
    return 1
  fi
  if ! has_commit "$GMAIN"; then
    UNKNOWN=1 NOTE="$github's main ($(short "$GMAIN")) is not in the clone: git fetch $github"
    return 1
  fi
  if [ -n "$sg" ] && has_commit "$sg" && [ "$(tree_of "$sg")" = "$(tree_of "$R")" ] && rg merge-base --is-ancestor "$sg" "$GMAIN"; then
    G=$sg
    NOTE="G $(short "$G") (recorded): on GitHub's main, with the release commit's tree"
    return 0
  fi
  if ! has_commit "$GMAIN"; then
    UNKNOWN=1 NOTE="$github's main ($(short "$GMAIN")) is not in the clone: git fetch $github"
    return 1
  fi
  if [ "$(tree_of "$GMAIN")" = "$(tree_of "$R")" ]; then
    G=$GMAIN
    NOTE="G $(short "$G"): $github/main has the release commit's tree"
    return 0
  fi
  NOTE="$github/main ($(short "$GMAIN")) does not have the release commit's tree"
  return 1
}

det_dry_run() {
  local rc miss=''
  [ -n "$G" ] || { NOTE="needs the mirror commit (step mirror)"; return 1; }
  rc=0; DRY_RUN=$(find_run pred_dry "$(st_num dry_run)" release.yml workflow_dispatch "$G") || rc=$?
  [ $rc != 2 ] || { UNKNOWN=1 NOTE="cannot ask GitHub for the runs: $(gh_err)"; return 1; }
  rc=0; CHECKS_LINUX=$(find_run pred_linux_all "$(st_num checks_linux)" linux-check.yml workflow_dispatch "$G") || rc=$?
  [ $rc != 2 ] || { UNKNOWN=1 NOTE="cannot ask GitHub for the runs: $(gh_err)"; return 1; }
  rc=0; CHECKS_MACOS=$(find_run pred_macos_all "$(st_num checks_macos)" macos-check.yml workflow_dispatch "$G") || rc=$?
  [ $rc != 2 ] || { UNKNOWN=1 NOTE="cannot ask GitHub for the runs: $(gh_err)"; return 1; }
  [ -n "$DRY_RUN" ] || miss="$miss, a successful release.yml dry run"
  [ -n "$CHECKS_LINUX" ] || miss="$miss, a successful linux-check suite=all"
  [ -n "$CHECKS_MACOS" ] || miss="$miss, a successful macos-check suite=all"
  if [ -n "$miss" ]; then
    NOTE="on $(short "$G") there is no${miss#,}"
    return 1
  fi
  NOTE="dry run $(run_url "$DRY_RUN"), linux-check $(run_url "$CHECKS_LINUX"), macos-check $(run_url "$CHECKS_MACOS")"
}

det_dry_verify() {
  local rec
  rec=$(st_get dry_verify)
  # The record alone does not prove the checks: they are required too, so that a recorded
  # dry-verify never lets a red or missing check read as done (implied).
  if [ -n "$DRY_RUN" ] && [ -n "$CHECKS_LINUX" ] && [ -n "$CHECKS_MACOS" ] && [ -n "$G" ]; then
    case $rec in
      "ok $DRY_RUN $G "*) NOTE="recorded: release-verify --dry-run of run $DRY_RUN passed (${rec##* })"; return 0 ;;
    esac
  fi
  if [ -z "$DRY_RUN" ] || [ -z "$CHECKS_LINUX" ] || [ -z "$CHECKS_MACOS" ]; then
    NOTE="needs the dry run and both suite=all checks green (step dry-run)"
    return 1
  fi
  NOTE="no recorded release-verify --dry-run of run $DRY_RUN at the mirror commit"
  [ -z "$rec" ] || NOTE="$NOTE (the state records '$rec', for another run or commit)"
  return 1
}

det_tag() {
  case $TAG_STATE in
    ok) NOTE=$TAG_MSG; return 0 ;;
    burned) HARD=$TAG_MSG NOTE=$TAG_MSG; return 1 ;;
    mismatch) UNKNOWN=1 NOTE="$TAG_MSG: $TAG_FIX"; return 1 ;;
    unknown) UNKNOWN=1 NOTE=$TAG_MSG; return 1 ;;
    *) NOTE=$TAG_MSG; return 1 ;;
  esac
}

need_tag() { [ "$TAG_OK" = 1 ] || { NOTE="needs the tag (step tag)"; return 1; }; }
need_release() {
  need_tag || return 1
  case $REL in
    none) NOTE="no release (draft) for $TAG on GitHub"; return 1 ;;
    unknown) UNKNOWN=1 NOTE="cannot ask GitHub for the release $TAG: $(gh_err)"; return 1 ;;
  esac
}

det_tag_run() {
  local rc ids n f missing=''
  need_tag || return 1
  rc=0; TAG_RUN=$(find_run pred_tag_run "$(st_num tag_run)" release.yml push "$G" "$TAG") || rc=$?
  [ $rc != 2 ] || { UNKNOWN=1 NOTE="cannot ask GitHub for the runs: $(gh_err)"; return 1; }
  [ -n "$TAG_RUN" ] || { NOTE="no release.yml run of the tag push on $(short "$G") whose publish job succeeded (yet)"; return 1; }
  ids=$(gh_read api "repos/$repo/releases" --paginate --jq ".[] | select(.tag_name == \"$TAG\") | .id") || { UNKNOWN=1 NOTE="cannot list the releases: $(gh_err)"; return 1; }
  n=$(printf '%s\n' "$ids" | grep -c '[0-9]' || true)
  if [ "$n" -gt 1 ]; then
    NOTE="$n releases for $TAG (ids $(printf '%s' "$ids" | tr '\n' ' ')): the double-draft trap; delete the stale draft (gh api -X DELETE repos/$repo/releases/ID)"
    return 1
  fi
  need_release || return 1
  for f in $(release_files) SHA256SUMS; do has_asset "$f" || missing="$missing $f"; done
  [ -z "$missing" ] || { NOTE="the release $TAG lacks:$missing"; return 1; }
  NOTE="run $(run_url "$TAG_RUN"): one release for $TAG, with the release files and SHA256SUMS"
}

det_verify_presign() {
  local d
  need_release || return 1
  d=$(asset_digest SHA256SUMS)
  if verified_has rebuild=1 && verified_has dry=0 && [ -n "$d" ] && verified_has "sums=$d"; then
    NOTE="$W/verified: the full verification (rebuild included) of the draft's SHA256SUMS"
    return 0
  fi
  NOTE="$W/verified does not record a full verification of the draft's SHA256SUMS ${d:+($(short "$d"))}"
  return 1
}

det_sign_macos() {
  local m c l
  need_release || return 1
  m=$(asset_digest "$MAC")
  c=$(asset_digest "$MACCI")
  l=$(local_sha "$MAC")
  if [ -z "$m" ] || [ -z "$c" ]; then NOTE="the release lacks $MAC or $MACCI"; return 1; fi
  if [ "$m" = "$c" ]; then NOTE="the draft's $MAC is still CI's (the same as $MACCI)"; return 1; fi
  if [ -n "$l" ] && [ "$l" != "$m" ]; then NOTE="the draft's $MAC differs from $FILES/$MAC"; return 1; fi
  # sign-macos uploads the archive, SHA256SUMS and SHA256SUMS.sig: a run that stopped after the
  # archive leaves a SHA256SUMS that does not list it.
  has_asset SHA256SUMS.sig || { NOTE="the draft has the new $MAC but no SHA256SUMS.sig (an upload that stopped part way)"; return 1; }
  draft_sums || { UNKNOWN=1 NOTE="cannot read the draft's SHA256SUMS: $(gh_err)"; return 1; }
  grep -qx "$m  $MAC" "$DRAFT_SUMS" || { NOTE="the draft's SHA256SUMS does not list its $MAC (an upload that stopped part way)"; return 1; }
  NOTE="the draft's $MAC is not CI's, its SHA256SUMS lists it${l:+, and it is the one in $FILES}"
}

# The package key at the release commit, and a scratch gpg home that knows only it.
pkg_fpr=''
asc_verifies() {
  local h=$scratch/gpghome k=$scratch/package-key.asc
  if [ -z "$pkg_fpr" ]; then
    rg show "$R:.github/package-signing-key.asc" > "$k" 2> /dev/null || return 1
    pkg_fpr=$(FIL_GPG=$GPG package_key_fingerprint "$k") || return 1
    mkdir -m 700 "$h"
    "$GPG" --batch --no-autostart --homedir "$h" --import "$k" > /dev/null 2>&1 || return 1
  fi
  LC_ALL=C "$GPG" --batch --no-autostart --homedir "$h" --status-fd 1 --verify "$FILES/SHA256SUMS.asc" "$FILES/SHA256SUMS" 2> /dev/null \
    | awk -v f="$pkg_fpr" '$1 == "[GNUPG:]" && $2 == "VALIDSIG" && ($3 == f || $NF == f) { ok = 1 } END { exit !ok }'
}
# local_signed_packages: the evidence of sign-packages in $FILES (also required by the steps after it).
local_signed_packages() {
  local p
  [ -d "$FILES" ] || { NOTE="no $FILES"; return 1; }
  [ -f "$FILES/.signed-locally" ] || { NOTE="$FILES has no .signed-locally marker (sign-packages writes it)"; return 1; }
  for p in $RPMS; do
    [ -f "$FILES/$p" ] || { NOTE="no $FILES/$p"; return 1; }
    [ "$(python3 -I "$RELEASE_DIR/pkg_inspect.py" "$FILES/$p" "$scratch/pi-$p" 2> /dev/null | sed -n 's/^signed //p')" = yes ] \
      || { NOTE="$p in $FILES is not signed"; return 1; }
  done
  [ -f "$FILES/$MAC" ] && [ "$(local_sha "$MAC")" != "$(local_sha "$MACCI")" ] || { NOTE="$FILES/$MAC is not the signed macOS archive (sign-macos comes first)"; return 1; }
  sums_check "$FILES" SHA256SUMS || { NOTE="the files in $FILES do not match their SHA256SUMS"; return 1; }
  [ -f "$FILES/SHA256SUMS.asc" ] && asc_verifies || { NOTE="$FILES/SHA256SUMS.asc does not verify with the package key at the release commit"; return 1; }
}
det_sign_packages() {
  need_release || return 1
  local_signed_packages || return 1
  NOTE="both rpms in $FILES are signed and SHA256SUMS.asc verifies with key $pkg_fpr"
}

sig_verifies() {
  local s=$scratch/signers p
  rg show "$G:.github/release-signers" > "$s" 2> /dev/null || return 1
  [ -f "$FILES/SHA256SUMS.sig" ] || return 1
  p=$(ssh-keygen -Y find-principals -f "$s" -s "$FILES/SHA256SUMS.sig" 2> /dev/null | head -n 1) || return 1
  [ -n "$p" ] && ssh-keygen -Y verify -f "$s" -I "$p" -n fil-release -s "$FILES/SHA256SUMS.sig" < "$FILES/SHA256SUMS" > /dev/null 2>&1
}
det_sign_sums() {
  need_release || return 1
  local_signed_packages || return 1
  sig_verifies || { NOTE="$FILES/SHA256SUMS.sig is not a fil-release signature of the current SHA256SUMS by a signer on GitHub's main"; return 1; }
  NOTE="$FILES/SHA256SUMS.sig verifies over the signed SHA256SUMS"
}

det_upload() {
  local f differ=''
  need_release || return 1
  local_signed_packages || return 1
  sig_verifies || { NOTE="$FILES/SHA256SUMS.sig does not verify (step sign-sums)"; return 1; }
  for f in $RPMS SHA256SUMS SHA256SUMS.asc SHA256SUMS.sig; do
    [ "$(asset_digest "$f")" = "$(local_sha "$f")" ] || differ="$differ $f"
  done
  [ -z "$differ" ] || { NOTE="the draft's copies differ from $FILES:$differ"; return 1; }
  NOTE="the draft holds the signed rpms, SHA256SUMS, .asc and .sig of $FILES"
}

det_verify_signed() {
  local d
  need_release || return 1
  d=$(asset_digest SHA256SUMS)
  if verified_has presign=0 && verified_has dry=0 && [ -n "$d" ] && verified_has "sums=$d"; then
    NOTE="$W/verified: a verification with the signature of the draft's SHA256SUMS"
    return 0
  fi
  NOTE="$W/verified does not record a signed verification (presign=0) of the draft's SHA256SUMS"
  return 1
}

det_publish() {
  need_release || return 1
  [ "$REL" = published ] || { NOTE="$TAG is still a draft"; return 1; }
  NOTE="$TAG is published"
}

need_published() {
  need_release || return 1
  [ "$REL" = published ] || { NOTE="needs the published release (step publish)"; return 1; }
}

det_verify_published() {
  local rec d
  need_published || return 1
  rec=$(st_get verify_published)
  d=$(asset_digest SHA256SUMS)
  case $rec in
    "ok $d "*) [ -n "$d" ] && { NOTE="recorded: release-verify of the published SHA256SUMS passed (${rec##* })"; return 0; } ;;
  esac
  NOTE="no recorded release-verify --no-rebuild of the published SHA256SUMS"
  return 1
}

# The published crates: the workspace members whose publish is ["crates-io"] (as publish-crates.sh).
det_crates() {
  local names n code missing='' odd=''
  need_published || return 1
  names=$(cd "$ROOT" && "$CARGO" metadata --locked --offline --no-deps --format-version 1 2> /dev/null | python3 -I -c 'import json, sys
m = json.load(sys.stdin)
for p in sorted(m["packages"], key=lambda p: p["name"]):
    if p["publish"] == ["crates-io"]:
        print(p["name"])') || names=''
  [ -n "$names" ] || { UNKNOWN=1 NOTE="cannot list the published crates (cargo metadata --offline)"; return 1; }
  for n in $names; do
    code=$("$CURL" -s -o /dev/null -w '%{http_code}' -A "fil-release (https://github.com/$repo)" "https://crates.io/api/v1/crates/$n/$V" 2> /dev/null || true)
    case $code in
      200) ;;
      404) missing="$missing $n" ;;
      *) odd="$odd $n(HTTP ${code:-none})" ;;
    esac
  done
  [ -z "$odd" ] || { UNKNOWN=1 NOTE="crates.io did not answer for:$odd"; return 1; }
  [ -z "$missing" ] || { NOTE="not on crates.io at $V:$missing"; return 1; }
  NOTE="all $(printf '%s\n' $names | grep -c .) crates are on crates.io at $V"
}

# The formula on the tap's main must be the one render.sh makes from the published SHA256SUMS.
det_brew() {
  local sums d main
  need_published || return 1
  draft_sums || { UNKNOWN=1 NOTE="cannot read the published SHA256SUMS: $(gh_err)"; return 1; }
  sums=$DRAFT_SUMS
  "$REPO_ROOT/packaging/render.sh" brew "$V" "$sums" "$scratch/brew" > /dev/null 2>&1 || { NOTE="render.sh cannot make the formula from the published SHA256SUMS"; return 1; }
  git -C "$tap" rev-parse --git-dir > /dev/null 2>&1 || { UNKNOWN=1 NOTE="no tap clone at $tap (--tap DIR)"; return 1; }
  main=$(git -C "$tap" ls-remote origin refs/heads/main 2> /dev/null | awk '{ print $1 }') || main=''
  [ -n "$main" ] || { UNKNOWN=1 NOTE="cannot read main of the tap's origin ($tap)"; return 1; }
  git -C "$tap" cat-file -e "$main^{commit}" 2> /dev/null || { UNKNOWN=1 NOTE="the tap's main ($(short "$main")) is not fetched: git -C $tap fetch origin"; return 1; }
  git -C "$tap" show "$main:Formula/fil.rb" > "$scratch/tap-fil.rb" 2> /dev/null || { NOTE="the tap's main has no Formula/fil.rb"; return 1; }
  cmp -s "$scratch/tap-fil.rb" "$scratch/brew/fil.rb" || { NOTE="Formula/fil.rb on the tap's main is not $V's formula"; return 1; }
  NOTE="Formula/fil.rb on the tap's main ($(short "$main")) is $V's"
}

det_finish() { NOTE="the summary, forgetting the keys, the copy for the coordinator"; return 1; }

# ---- detect every step: KIND[i] is checked, recorded, no or unknown, then implied/next/pending.
KIND=() NOTES=()
detect_all() {
  local s i=0
  TAG_OK=0 TAG_STATE=unknown HARD='' HARD_STEP='' LAST_DONE=0 G='' REL=unknown
  DRY_RUN='' CHECKS_LINUX='' CHECKS_MACOS='' TAG_RUN='' pkg_fpr=''
  rm -f "$scratch"/run-* "$scratch/assets" "$scratch/release"
  rm -rf "$scratch/dl" "$scratch/gpghome" "$scratch/sums" "$scratch/brew"
  GMAIN=$(rg ls-remote "$github" refs/heads/main 2> /dev/null | awk '{ print $1 }') || GMAIN=''
  read_tag
  [ "$TAG_OK" = 0 ] || read_release
  for s in $PF_STEPS; do
    i=$((i + 1))
    NOTE='' UNKNOWN=0
    if "det_$(printf '%s' "$s" | tr - _)"; then
      case $s in dry-verify | verify-published) KIND[i]=recorded ;; *) KIND[i]=checked ;; esac
      LAST_DONE=$i
    elif [ "$UNKNOWN" = 1 ]; then
      KIND[i]=unknown
    else
      KIND[i]=no
    fi
    NOTES[i]=$NOTE
    [ -z "$HARD" ] || [ -n "$HARD_STEP" ] || HARD_STEP=$i
  done
  NEXT=0
  for ((i = 1; i <= NSTEPS; i++)); do
    case ${KIND[i]} in
      checked | recorded) ;;
      *) if [ $i -lt "$LAST_DONE" ]; then KIND[i]=implied; elif [ "$NEXT" = 0 ]; then NEXT=$i; else KIND[i]=pending; fi ;;
    esac
  done
}

label() {
  case $1 in
    checked) echo "done (checked)" ;;
    recorded) echo "done (recorded)" ;;
    implied) echo "done (implied)" ;;
    burned) echo "BURNED" ;;
    *) echo "$1" ;;
  esac
}

print_status() {
  local i k sr held
  sr=$(st_hex release_commit)
  echo "release $V ($repo): release commit $R"
  if [ -f "$STATE" ]; then echo "  state: $STATE"; else echo "  state: none yet ($STATE)"; fi
  [ -z "$sr" ] || [ "$sr" = "$(rg rev-parse -q --verify 'HEAD^{commit}' 2> /dev/null || true)" ] \
    || echo "  WARNING: the state is bound to $sr, but HEAD is $(rg rev-parse -q --verify HEAD 2> /dev/null || echo none): the steps below are for the bound commit"
  [ -z "$G" ] || echo "  mirror commit (GitHub): $G"
  held=$(cat "$LOCK/pid" 2> /dev/null || true)
  if [ -n "$held" ] && kill -0 "$held" 2> /dev/null; then echo "  a release run (pid $held) is running now"; fi
  for ((i = 1; i <= NSTEPS; i++)); do
    k=${KIND[i]}
    [ "$i" != "$NEXT" ] || k=next
    [ "$i" != "${HARD_STEP:-0}" ] || k=burned
    case $k in
      pending) printf '  %2d/%d %-17s %s\n' "$i" "$NSTEPS" "$(step_name $i)" pending ;;
      next) printf '  %2d/%d %-17s %-16s %s\n' "$i" "$NSTEPS" "$(step_name $i)" "$([ "${KIND[i]}" = unknown ] && echo "next (unknown)" || echo next)" "${NOTES[i]}" ;;
      *) printf '  %2d/%d %-17s %-16s %s\n' "$i" "$NSTEPS" "$(step_name $i)" "$(label "$k")" "${NOTES[i]}" ;;
    esac
  done
  echo "keys (just release-keys status):"
  "$RELEASE_DIR/release-keys.sh" status --need ssh,gpg < /dev/null 2>&1 | sed 's/^/  /' || true
}

# do_hint STEP: what to do by hand at STEP, until the command does it (PRs 4-6).
do_hint() {
  case $1 in
    candidate) echo "fix what the preflight lists (a PR on Gitea), then run the same command" ;;
    mirror) echo "G=\$(git commit-tree -S $(short "$R")^{tree} -p $github/main -m 'fil $V') && git push $github \$G:main  (the mirror commit: the release commit's exact tree on GitHub's main)" ;;
    dry-run) echo "gh workflow run release.yml --repo $repo --ref main; gh workflow run linux-check.yml --repo $repo --ref main -f suite=all; gh workflow run macos-check.yml --repo $repo --ref main -f suite=all; wait for all three" ;;
    dry-verify) echo "just release-verify $V --dry-run --from-run ${DRY_RUN:-RUN} --ref ${G:-G}  (by hand it leaves no record: this step reads done once the tag is pushed; PR 4 records it)" ;;
    tag) echo "git -c gpg.format=ssh -c user.signingkey=\$HOME/.ssh/fil-release.pub tag -s -m 'fil $V' $TAG-gh ${G:-G} && git push $github $TAG-gh:refs/tags/$TAG  (irreversible)" ;;
    tag-run) echo "wait for the tag's release.yml run to publish the draft${TAG_RUN:+ ($(run_url "$TAG_RUN"))}; gh run list --repo $repo --workflow release.yml --event push" ;;
    verify-presign) echo "just release-verify $V --pre-sign" ;;
    sign-macos) echo "FIL_RELEASE_KEY=\$HOME/.ssh/fil-release.pub just release-sign-macos $V" ;;
    sign-packages) echo "just release-sign-packages $V" ;;
    sign-sums) echo "rm -f $FILES/SHA256SUMS.sig && ssh-keygen -Y sign -f \$HOME/.ssh/fil-release.pub -n fil-release $FILES/SHA256SUMS" ;;
    upload) echo "(cd $FILES && gh release upload $TAG --repo $repo --clobber $RPMS SHA256SUMS SHA256SUMS.asc SHA256SUMS.sig)" ;;
    verify-signed) echo "just release-verify $V" ;;
    publish) echo "gh release edit $TAG --repo $repo --draft=false  (irreversible)" ;;
    verify-published) echo "just release-verify $V --no-rebuild  (by hand it leaves no record: this step reads done once the crates are out; PR 6 records it)" ;;
    crates) echo "approve the crates-io deployment of the tag's run${TAG_RUN:+ ($(run_url "$TAG_RUN"))} and wait for its crates-io job" ;;
    brew) echo "just brew-formula $V; copy target/packaging/homebrew/fil.rb to $tap/Formula/fil.rb; commit (signed) and push the tap" ;;
    finish) echo "nothing: the summary and key forgetting arrive in PR 6" ;;
  esac
}

# ---- run
if [ "$mode" = status ]; then
  if git -C "$tap" rev-parse --git-dir > /dev/null 2>&1; then
    git -C "$tap" fetch -q --no-tags origin 2> /dev/null || true
  fi
  R=$(st_hex release_commit)
  has_commit "$R" || R=$(rg rev-parse -q --verify 'HEAD^{commit}' 2> /dev/null) || die "$ROOT is not a git clone with a commit"
  detect_all
  # Nothing bound and the tag is another tree's: show the release the tag names, from its commit.
  if [ "$TAG_STATE" = mismatch ] && [ -n "$TAG_FOUND" ]; then
    log "HEAD is not the release commit of $TAG; showing $TAG_FOUND (the commit on $origin/main with the tag's tree)"
    R=$TAG_FOUND
    detect_all
  fi
  print_status
  [ -z "$HARD_STEP" ] || exit 1
  exit 0
fi

lock_take
rg fetch -q --no-tags "$origin" 2> "$scratch/git.err" || report FAILED 0 "git fetch $origin failed: $(head -n 1 "$scratch/git.err" | cut -c1-200)" "check the remote (git remote -v; git fetch $origin)"
rg fetch -q --no-tags "$github" 2> "$scratch/git.err" || report FAILED 0 "git fetch $github failed: $(head -n 1 "$scratch/git.err" | cut -c1-200)" "check the remote (git remote -v; git fetch $github)"
if git -C "$tap" rev-parse --git-dir > /dev/null 2>&1; then
  git -C "$tap" fetch -q --no-tags origin 2> /dev/null || log "note: could not fetch the tap ($tap); step brew reads it as it is"
fi
R=$(rg rev-parse -q --verify 'HEAD^{commit}' 2> /dev/null) || report FAILED 0 "$ROOT is not a git clone with a commit" "run it in your fil clone"

# Binding: the state names one version and one release commit.
if [ "$reset" = 1 ]; then
  t=$(rg ls-remote "$github" "refs/tags/$TAG" 2> "$scratch/git.err") || report FAILED 0 "cannot ask '$github' whether $TAG exists, so --reset-state is refused" "check the remote, then run it again"
  [ -z "$t" ] || report FAILED 0 "$TAG is on GitHub: --reset-state is refused (the tag binds its commit forever)" "git checkout the commit the state names; a new commit after the tag is a new version"
  rm -f "$STATE"
  log "the state of $V is reset"
fi
sv=$(st_get version)
sr=$(st_get release_commit)
[ -z "$sv" ] || [ "$sv" = "$V" ] || report FAILED 0 "$STATE is for version $sv, not $V" "rm '$STATE' (it holds nothing the world cannot tell again), then run it again"
if [ -n "$sr" ] && [ "$sr" != "$R" ]; then
  if t=$(rg ls-remote "$github" "refs/tags/$TAG" 2> /dev/null) && [ -z "$t" ]; then
    report FAILED 0 "the state is for $V at $(short "$sr"), HEAD is $(short "$R"); no $TAG on GitHub yet" \
      "git checkout $sr; or, for the new release commit: $AGAIN --reset-state (it rebinds)"
  fi
  report FAILED 0 "the state is for $V at $(short "$sr"), HEAD is $(short "$R"), and $TAG may be on GitHub: the tag binds its commit, and a new commit after it is a new version" \
    "git checkout $sr"
fi

detect_all
if [ "$TAG_STATE" = mismatch ]; then
  LAST_DONE=0
  report FAILED "$(step_no tag)" "$TAG_MSG (nothing is recorded)" "$TAG_FIX, then run it again"
fi
st_set version "$V"
st_set release_commit "$R"
# Record what was found (identifiers only).
[ -z "$G" ] || st_set github_commit "$G"
[ -z "$DRY_RUN" ] || st_set dry_run "$DRY_RUN"
[ -z "$CHECKS_LINUX" ] || st_set checks_linux "$CHECKS_LINUX"
[ -z "$CHECKS_MACOS" ] || st_set checks_macos "$CHECKS_MACOS"
[ "$TAG_OK" = 0 ] || st_set tag_object "$TAG_OBJ"
[ -z "$TAG_RUN" ] || st_set tag_run "$TAG_RUN"

if [ -n "$HARD_STEP" ]; then
  LAST_DONE=$((HARD_STEP - 1))
  report FAILED "$HARD_STEP" "$HARD" "this version is burned: bump to the next patch version (a PR on Gitea: just set-version), and release that"
fi
if [ "$NEXT" = "$NSTEPS" ]; then
  log "steps 1-$LAST_DONE of $NSTEPS are done for $V; step $NSTEPS finish is not automated yet (PR 6): nothing to do"
  exit 0
fi
name=$(step_name "$NEXT")
pf_init "$V"
PF_REPO=$repo PF_ORIGIN=$origin PF_GITHUB=$github PF_AGAIN=$AGAIN PF_LOCK_OWNER=$$
[ -z "$vm" ] || PF_VM=$vm
PF_COMMIT=''
[ "$TAG_OK" = 0 ] || PF_COMMIT=$R
# shellcheck disable=SC2046 # a word list
if ! pf_run $(pf_checks_from "$name"); then
  report FAILED "$NEXT" "the preflight for $name and the steps after it failed (each failure above says what to do)" "fix what the preflight lists"
fi
if [ "${KIND[NEXT]}" = unknown ]; then
  report FAILED "$NEXT" "cannot tell whether $name is done: ${NOTES[NEXT]}" "fix that, then run it again"
fi
report PAUSED "$NEXT" "$name is not done (${NOTES[NEXT]}); this version of the command detects the steps but does not act yet" "$(do_hint "$name")"
