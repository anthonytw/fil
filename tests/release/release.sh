#!/usr/bin/env bash
# `tests/release/just.sh release X.Y.Z`: the one-command, resumable release (docs/m4/release.md section 9.5). It binds
# a run to the version and the release commit, takes the lock, runs the preflight, and works out from
# the world (git refs, GitHub's runs and releases, crates.io, the Homebrew tap) which of the seventeen
# steps are done, then does the next step's action and the ones after it, through `finish`. Re-run the
# same command after any failure: every step is detected again first, and no action that is done is
# repeated.
#
#   release.sh VERSION [--status] [--reset-state] [--from STEP | --redo STEP] [--confirm-each]
#                      [--ttl 8h] [--lock-keys] [--repo OWNER/REPO] [--origin REMOTE]
#                      [--github REMOTE] [--tap DIR] [--vm NAME]
#
#   (default)      fetch `origin` and `github` (no tags); before vV exists (in the clone or on
#                  GitHub), fast-forward a clean main that is behind Gitea's (git merge --ff-only) and
#                  rebind a state bound to another commit to HEAD when HEAD is Gitea's main (as
#                  --reset-state); bind the state to VERSION and HEAD, take the lock, detect every
#                  step, record the identifiers found (GitHub's commit, run ids, the tag object) in
#                  the state, run the preflight for the next step and the ones after it, at a
#                  terminal unlock the keys the remaining steps need (one block, release-keys.sh
#                  unlock --ttl), then do the next step's action and the ones after it, through
#                  finish:
#                    mirror      [confirmation] GitHub's main fast-forwarded to the release commit R
#                                itself (git push github R:refs/heads/main, never forced); done when R
#                                is on GitHub's main already (agents push it); refused when GitHub's
#                                main has commits R lacks
#                    dry-run     dispatch release.yml, and linux-check and macos-check with suite=all
#                                (ref = R), on GitHub's main (which must be R); the run ids (from the URL
#                                gh prints, else the new run on the commit) are recorded at once, and
#                                every run is waited for (Ctrl-C is safe: a re-run re-attaches). A run
#                                that failed: at a terminal, the command offers `gh run rerun --failed`.
#                                Red suite=all checks with a green dry run: at a terminal the command
#                                asks whether they block the release ([y/N] to waive them; only `y`
#                                waives; a waiver also voids a `tag` confirmation given before it, so
#                                step 5 asks again, listing it); a waiver is recorded as checks_waived=<linux-check ID.ATTEMPT
#                                or -> <macos-check ID.ATTEMPT or -> <utc> and counts only for exactly
#                                those runs and attempts (--status: "done (waived)"). Without a
#                                terminal that decision pauses the run (exit 2). A running run the
#                                state lost is attached to only if it is this release's (triggered by
#                                the gh user, not older than the release commit)
#                    dry-verify  release-verify --dry-run --from-run RUN --ref R (the VM rebuild); its
#                                success is recorded in the state (a local result)
#                    tag         [confirmation, again in every run that did not confirm `tag` itself:
#                                the answer is never stored] the tag vV on the release commit, signed
#                                with the release key's .pub through the agent (local name always vV-gh:
#                                vV belongs to Gitea's marker; a vV-gh made by an earlier run is reused),
#                                checked with verify-tag.sh, pushed to refs/tags/vV
#                    tag-run     find the tag push's release.yml run (recorded at once) and poll it until
#                                its publish job succeeded (a heartbeat every 10 polls; a "probably
#                                stuck" note after 3 h); a failed run, once it has completed: at a
#                                terminal, one rerun of its failed jobs, deleting first the draft a
#                                failed publish left (else there are two). Two or more drafts for vV:
#                                the newest (highest id) is kept if it has every release file, and the
#                                older are deleted after a [y/N] (a published one is never deleted)
#                    verify-presign  release-verify --pre-sign (downloads the draft, the VM rebuild)
#                    sign-macos  sign-macos.sh with FIL_RELEASE_KEY=the .pub (--force on --redo)
#                    sign-packages  sign-packages.sh on target/release-verify/VERSION/files
#                    sign-sums   ssh-keygen -Y sign -f the .pub -n fil-release of the signed SHA256SUMS
#                    upload      gh release upload --clobber of the rpms, SHA256SUMS, .asc and .sig that
#                                differ from the draft's (all five on --redo)
#                    verify-signed  release-verify --reuse-rebuild (with the signature; it reuses step
#                                7's rebuild of the tag's commit)
#                    publish     [confirmation, again in every run that did not confirm `publish` itself;
#                                by default it covers crates and brew too] the draft is read again after
#                                the answer and must be exactly the files step 12 verified (each asset's
#                                digest is the one SHA256SUMS lists, or the signed file's), then
#                                gh release edit vV --draft=false
#                    verify-published  release-verify --no-rebuild of what is published; its success is
#                                recorded in the state (verify_published, with the published SHA256SUMS's
#                                digest). A failure is a loud stop: nothing goes to crates.io or the tap
#                    crates      [confirmation: covered by `publish` in the same run, else asked as
#                                `crates`, covering brew] the crates-io job of the tag's run: its pending
#                                deployment of the crates-io environment is approved through
#                                pending_deployments (once per run attempt; a deployment not created yet
#                                is polled for, one already approved is not approved again; one the gh
#                                user may not approve pauses the run, exit 2, with the run's URL); the
#                                job is polled until it succeeds, then crates.io until it lists every
#                                crate. A failed job, at a terminal: one `gh run rerun --failed`
#                                (publish-crates.sh skips the crates already on crates.io), approved again
#                    brew        [confirmation: covered by `publish` or `crates` in the same run, else asked
#                                as `brew`] done when the tap's formula names VERSION with the published
#                                checksums. Only a tap with no formula, or an older version's, is changed:
#                                the formula rendered from the published SHA256SUMS, committed (signed, git
#                                commit -S) on the tap clone's main (--tap; a commit an earlier run made and
#                                could not push is reused), pushed to the tap's origin. A newer version's,
#                                or VERSION's with other checksums, is never overwritten. Then brew update,
#                                brew upgrade (or install) OWNER/tap/fil, and `fil --version` must name
#                                VERSION; a pass is recorded (brew_tested, with the tested formula's sha256)
#                    finish      (on every run once brew is done; it changes nothing public) the keys
#                                forgotten first (release-keys.sh forget; publish forgot them already),
#                                the AUR PKGBUILD rendered into target/packaging/aur (not published), the
#                                published SHA256SUMS and .sig copied to /Users/Shared/fil-VERSION for the
#                                coordinator (a directory of yours, 755, files 644; a symlink or another
#                                user's directory is refused), and a summary, with the install test's
#                                result (brew_tested)
#                  Exit 0: finish is done; 1: failed (the failure says what, what to do, and to run the
#                  same command again; the step's log is target/release-verify/VERSION/logs/NN-STEP.log);
#                  2: paused for a human (a confirmation or other prompt not answered yes, no terminal
#                  at a prompt, keys locked with no terminal to unlock them, or a crates-io deployment
#                  the gh user may not approve).
#   --status       per step: done (checked), done (recorded), done (waived), done (implied), next or
#                  pending, with the run URLs, and whether the release keys are unlocked
#                  (release-keys.sh status).
#                  Read-only: no fetch of the clone (the tap is fetched), no lock, no state written,
#                  no preflight. With no state bound and HEAD not the release commit of a tag on
#                  GitHub, it shows the commit on origin's main with the tag's tree. Exit 1 only for
#                  a burned tag.
#   --reset-state  forget the state (another release commit before the tag); refused once the tag
#                  is on GitHub, which binds the commit forever. A run rebinds by itself when HEAD is
#                  Gitea's main and vV exists neither in the clone nor on GitHub.
#   --from STEP    for a step done by hand: every step before STEP must detect as done (else the run
#                  is refused); then as the default.
#   --redo STEP    do STEP's action again although it detects as done, and the actions of the steps
#                  that consume its output (dry-run: dry-verify; verify-presign, sign-macos: sign-packages
#                  to verify-signed; sign-packages: sign-sums to verify-signed; sign-sums: upload and
#                  verify-signed; upload: verify-signed; verify-published and brew: only themselves).
#                  Only for a step that is done (verify-presign only before the upload: a signed draft
#                  cannot pass a pre-sign check); dry-run and dry-verify only before the tag,
#                  verify-presign to verify-signed only while the release is a draft, verify-published
#                  and brew only once it is published (--redo brew never commits or pushes: it runs the
#                  install test again). Refused for mirror, tag and tag-run (GitHub keeps them;
#                  a failed tag run is offered a rerun anyway), for publish and crates (they cannot be
#                  undone) and for finish (it runs on every run once brew is done).
#   --confirm-each one confirmation per irreversible point (mirror, tag, publish, crates, brew). The
#                  default is two: `tag` (asked before the mirror push, it covers the mirror and the
#                  tag) and `publish` (it covers publish, crates and brew). Each
#                  shows what becomes public and wants the version typed back; there is no --yes,
#                  and a run whose input is not a terminal stops (exit 2) at the first one.
#   --ttl T        the key session for release-keys.sh unlock (default 8h).
#   --lock-keys    forget the release keys now (release-keys.sh forget), and nothing else.
#   --tap DIR      the Homebrew tap clone (default ~/dev/homebrew-tap; its `origin` is the tap).
#
# The state: target/release-verify/VERSION/release.state, `key=value` lines written atomically
# (temp file and mv), only identifiers the world cannot be asked for cheaply and the results of
# purely local checks or decisions (dry_verify, checks_waived, verify_published, brew_tested). Never a secret, never
# a confirmation, and not the gpg reaper (release-keys.sh keeps it in ~/.cache/fil/release-keys/).
# Real state beats the state file: every recorded identifier is checked against the world before it
# counts.
#
# A step that does not detect as done is "done (implied)" when a later step does: the steps are a
# pipeline, each detection includes the evidence of the earlier steps it consumes (a recorded
# dry-verify counts only with the dry run and both checks green or waived), and the steps after `tag`
# are only looked at once the tag is there (after `publish`, once it is published).
#
# A tag on GitHub is BURNED (a hard stop: bump the version) when it does not verify, when the state
# binds a release commit of another tree, when it is not the tag object the state recorded, or (in a
# run, which has fetched origin) when no commit on origin's main has its tree. A good tag of another
# tree whose commit is on origin's main, with nothing bound, means this checkout is not the release
# commit: the run says which commit to check out and records nothing.
#
# GitHub writes, all in the actions above: the mirror push and the tag push (git), `gh workflow run`
# of the three workflows, `gh run rerun --failed` when the owner says so, the deletion of a stale
# DRAFT for the tag (gh api -X DELETE .../releases/ID) when the owner says so, `gh release upload
# --clobber` to the draft, `gh release edit --draft=false` (publish), the approval of the tag run's
# crates-io deployment (gh api -X POST .../actions/runs/ID/pending_deployments) and the tap push
# (git). Everything else is a read. No token or passphrase passes through this script: gh, git and
# release-keys.sh hold their own. The tools, each a test-only override refused outside the self-test
# (FIL_RELEASE_SELFTEST=1): FIL_GH, FIL_GPG, FIL_CARGO, FIL_CRATES_CURL, FIL_BREW, FIL_RELEASE_ROOT,
# FIL_RELEASE_VERIFY (verify.sh), FIL_SIGN_MACOS (sign-macos.sh), FIL_SIGN_PACKAGES
# (sign-packages.sh), FIL_RELEASE_KEYS (release-keys.sh), FIL_RELEASE_POLL (seconds between polls of
# the tag's run and the crates-io job, default 30), FIL_RELEASE_SHARED (finish's copy for the
# coordinator, default /Users/Shared/fil-VERSION), FIL_RELEASE_TTY (=1: treat the input as a
# terminal), FIL_RELEASE_LOCK_HOLD (hold the lock while a file exists), and the preflight's
# (release-preflight.sh). Written for bash 3.2.
#
# The whole script is one { ... } group that ends in `exit`: bash reads all of it before it runs any
# of it, so this file rewritten IN PLACE under a running release (some editors, `cat >`, `sed -i` on
# some systems; `git pull` writes a new file, which a running bash never reads) cannot make the run
# execute a mix of the old and the new text. The scripts it starts later (verify.sh, release-keys.sh,
# the sign scripts, render.sh) are read when they start, and are not covered.
{
set -euo pipefail
# shellcheck source=tests/release/release-preflight.sh
source "$(dirname "${BASH_SOURCE[0]}")/release-preflight.sh"

usage="usage: release.sh VERSION [--status] [--reset-state] [--from STEP | --redo STEP] [--confirm-each] [--ttl 8h] [--lock-keys] [--repo OWNER/REPO] [--origin REMOTE] [--github REMOTE] [--tap DIR] [--vm NAME]"
version='' mode=run reset=0 repo=anthonytw/fil origin=origin github=github tap=$HOME/dev/homebrew-tap vm='' from='' confirm_each=0 redo='' ttl=8h
while [ $# -gt 0 ]; do
  case $1 in
    --status) mode=status; shift ;;
    --lock-keys) mode=lock-keys; shift ;;
    --reset-state) reset=1; shift ;;
    --confirm-each) confirm_each=1; shift ;;
    --repo | --origin | --github | --tap | --vm | --from | --redo | --ttl)
      [ $# -ge 2 ] || die "$usage"
      case $1 in --repo) repo=$2 ;; --origin) origin=$2 ;; --github) github=$2 ;; --tap) tap=$2 ;; --vm) vm=$2 ;; --from) from=$2 ;; --redo) redo=$2 ;; --ttl) ttl=$2 ;; esac
      shift 2
      ;;
    -*) die "unknown option: $1 ($usage)" ;;
    *) [ -z "$version" ] || die "$usage"; version=$1; shift ;;
  esac
done
[ -n "$version" ] || die "$usage"
V=${version#v}
package_version_ok "$V" || die "'$version' is not X.Y.Z or X.Y.Z-PRE"
if [ "$mode" != run ] && { [ "$reset" = 1 ] || [ "$confirm_each" = 1 ] || [ -n "$from" ] || [ -n "$redo" ]; }; then
  die "--status and --lock-keys do not act on the steps: --reset-state, --from, --redo and --confirm-each do not go with them"
fi
if [ -n "$from" ]; then
  case " $PF_STEPS " in *" $from "*) ;; *) die "--from $from: no such step (one of: $PF_STEPS)" ;; esac
fi
[ -z "$from" ] || [ -z "$redo" ] || die "--from and --redo do not go together"
# --redo STEP: REDO_LEFT is STEP and the steps that consume its output, in order; each is acted on
# again in this run, whatever it detects as.
REDO_LEFT=''
if [ -n "$redo" ]; then
  case $redo in
    dry-run) REDO_LEFT="dry-run dry-verify" ;;
    dry-verify) REDO_LEFT="dry-verify" ;;
    verify-presign) REDO_LEFT="verify-presign sign-packages sign-sums upload verify-signed" ;;
    sign-macos) REDO_LEFT="sign-macos sign-packages sign-sums upload verify-signed" ;;
    sign-packages) REDO_LEFT="sign-packages sign-sums upload verify-signed" ;;
    sign-sums) REDO_LEFT="sign-sums upload verify-signed" ;;
    upload) REDO_LEFT="upload verify-signed" ;;
    verify-signed) REDO_LEFT="verify-signed" ;;
    candidate) die "--redo candidate: it only checks, and every run checks it" ;;
    mirror | tag) die "--redo $redo: refused, it cannot be redone (GitHub keeps what it pushed; a bad $redo means a new version)" ;;
    tag-run) die "--redo tag-run: refused (the tag's run is GitHub's: a failed one is offered a rerun of its failed jobs, and a stale draft its deletion, by the command itself)" ;;
    verify-published) REDO_LEFT="verify-published" ;;
    brew) REDO_LEFT="brew" ;;
    publish | crates) die "--redo $redo: refused, it cannot be undone (a published release is immutable; a crates.io version can only be yanked)" ;;
    finish) die "--redo finish: refused, there is nothing to redo: finish runs on every run once brew is done (run it without --redo)" ;;
    *) die "--redo $redo: no such step (one of: $PF_STEPS)" ;;
  esac
fi
case $ttl in [1-9]*[smh] | [1-9]*) ;; *) die "--ttl '$ttl': N, Ns, Nm or Nh" ;; esac
TAG=v$V
W=$ROOT/target/release-verify/$V
STATE=$W/release.state
LOCK=$W/.lock
AGAIN="tests/release/just.sh release $V"

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
BREW=${FIL_BREW:-brew}
SHARED=${FIL_RELEASE_SHARED:-/Users/Shared/fil-$V}
VERIFIER=${FIL_RELEASE_VERIFY:-$RELEASE_DIR/verify.sh}
SIGN_MACOS=${FIL_SIGN_MACOS:-$RELEASE_DIR/sign-macos.sh}
SIGN_PACKAGES=${FIL_SIGN_PACKAGES:-$RELEASE_DIR/sign-packages.sh}
KEYS=${FIL_RELEASE_KEYS:-$RELEASE_DIR/release-keys.sh}
POLL=${FIL_RELEASE_POLL:-30}
is_poll() { case $1 in "" | *[!0-9]* | ????*) false ;; *) true ;; esac; }
is_poll "$POLL" || die "FIL_RELEASE_POLL: seconds, 0 to 999"
# The release key's public half: every signature (the tag, SHA256SUMS, sign-macos's) names it, so the
# private key is used only through the agent (release-keys.sh unlock put it there with a lifetime).
RELEASE_PUB=${FIL_RELEASE_KEY:-$HOME/.ssh/fil-release}
RELEASE_PUB=${RELEASE_PUB%.pub}.pub

if [ "$mode" = lock-keys ]; then
  "$KEYS" forget --need ssh,gpg
  exit
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/fil-release.XXXXXX")
LOCK_OWNED=0 REAP_OWNED=0
cleanup() {
  rm -rf "$scratch"
  if [ "$LOCK_OWNED" = 1 ] && [ "$(cat "$LOCK/pid" 2> /dev/null || true)" = "$$" ]; then rm -rf "$LOCK"; fi
  if [ "$REAP_OWNED" = 1 ]; then rmdir "$LOCK.reap" 2> /dev/null || true; fi
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
STATE_KEYS="version release_commit bound_at github_commit dry_run checks_linux checks_macos checks_waived dry_verify tag_object tag_run verify_published brew_tested"
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
# st_unset KEY: forget KEY (a recorded identifier the world no longer has).
st_unset() {
  [ -f "$STATE" ] || return 0
  awk -v k="$1" 'index($0, k "=") != 1' "$STATE" > "$STATE.tmp.$$"
  mv "$STATE.tmp.$$" "$STATE"
}
st_hex() { local v; v=$(st_get "$1"); if is_hex40 "$v"; then echo "$v"; fi; }
st_num() { local v; v=$(st_get "$1"); if is_num "$v"; then echo "$v"; fi; }

# ---- output
step_no() { local s i=0; for s in $PF_STEPS; do i=$((i + 1)); [ "$s" = "$1" ] && { echo $i; return; }; done; }
step_name() { local s i=0; for s in $PF_STEPS; do i=$((i + 1)); [ $i = "$1" ] && { echo "$s"; return; }; done; }
NSTEPS=$(set -- $PF_STEPS; echo $#)
LAST_DONE=0
# A stop at a step that is not done, with a later step done already (GitHub's main at the release
# commit makes mirror done before the candidate is): the done steps are named, not "1-N".
then_line() {
  local i d=''
  if [ "$NEXT" -gt 0 ] && [ "$LAST_DONE" -ge "$NEXT" ]; then
    for ((i = 1; i <= LAST_DONE; i++)); do
      case ${KIND[i]:-} in checked | recorded | waived | implied) d="$d, $i $(step_name $i)" ;; esac
    done
    printf '  then:  %s          (step %s %s is not done; done already, and skipped: %s)\n' "$AGAIN" "$NEXT" "$(step_name "$NEXT")" "${d#, }"
  elif [ "$LAST_DONE" -gt 0 ]; then
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
# Every change of the lock (taking it with its pid, removing a dead run's) happens under a mutex,
# the directory $LOCK.reap, so a run never sees a lock without its pid, and two runs never both
# remove a stale lock and both take it (nor can a move nest one lock inside another, as BSD mv does).
# The mutex is held for a few file operations; one left by a run killed in that instant is reported.
lock_take() {
  local pid n=0
  mkdir -p "$W"
  while :; do
    until mkdir "$LOCK.reap" 2> /dev/null; do
      n=$((n + 1))
      [ $n -le 20 ] || report FAILED 0 "$LOCK.reap has been held for 10 s (a release run was stopped in the instant it took or removed the lock)" \
        "if no release run is running (ps -ax | grep release.sh), rmdir '$LOCK.reap'"
      sleep 0.5
    done
    REAP_OWNED=1
    if mkdir "$LOCK" 2> /dev/null; then
      echo $$ > "$LOCK/pid"
      LOCK_OWNED=1
      rmdir "$LOCK.reap"
      REAP_OWNED=0
      return 0
    fi
    pid=$(cat "$LOCK/pid" 2> /dev/null || true)
    if [ -n "$pid" ] && kill -0 "$pid" 2> /dev/null; then
      rmdir "$LOCK.reap"
      REAP_OWNED=0
      report FAILED 0 "another release run (pid $pid) holds $LOCK" "wait for it, or stop it (kill $pid)"
    fi
    rm -rf "$LOCK"
    log "removed a stale lock (pid ${pid:-unknown} is not running): $LOCK"
    rmdir "$LOCK.reap"
    REAP_OWNED=0
  done
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
  out=$(gh_read run view "$1" --repo "$repo" --json headSha,event,workflowName,status,conclusion,attempt,jobs) || return 1
  printf '%s' "$out" | python3 -I -c 'import json, sys
r = json.load(sys.stdin)
for k in ("headSha", "event", "workflowName", "status", "conclusion", "attempt"):
    print(k, r.get(k) or "-")
for j in r.get("jobs") or []:
    print("job", j.get("conclusion") or "-", j.get("name") or "")' > "$f.tmp" || return 1
  mv "$f.tmp" "$f"
}
rv_has() { grep -qx -- "$2" "$scratch/run-$1"; }
rv_attempt() { sed -n 's/^attempt \([0-9][0-9]*\)$/\1/p' "$scratch/run-$1"; }
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
# run_ours ID: a dispatched run that is this release's: triggered by the gh user running the command
# and created no earlier than the state was bound to the release commit (bound_at, 5 minutes of clock
# slack; R can sit on GitHub's main for days before the release, so R's own commit time would let an
# older run in, of another `ref` input, which GitHub does not report). A run sets BOUND_AT before its
# first detection (a new or reset state: now); without it (--status of a state that has none) no
# listed run is this release's. The workflows' inputs are
# not reported, so a run found by listing, not dispatched and recorded by the command, needs this.
# 0 yes, 1 no, 2 GitHub could not be asked.
GH_USER=''
run_ours() {
  local out gt
  [ -n "$GH_USER" ] || GH_USER=$(gh_read api user --jq .login) || return 2
  [ -n "$GH_USER" ] || return 2
  out=$(gh_read api "repos/$repo/actions/runs/$1" --jq '[.triggering_actor.login // "", .created_at // ""] | @tsv') || return 2
  gt=${BOUND_AT:-$(st_num bound_at)}
  is_num "$gt" || return 1
  printf '%s\n' "$out" | python3 -I -c 'import datetime, sys
user, gt = sys.argv[1], int(sys.argv[2])
f = (sys.stdin.read().rstrip("\n").split("\t") + ["", ""])[:2]
try:
    c = datetime.datetime.strptime(f[1], "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc).timestamp()
except ValueError:
    sys.exit(1)
sys.exit(0 if f[0] == user and c >= gt - 300 else 1)' "$GH_USER" "$gt"
}
# find_run PRED RECORDED WORKFLOW EVENT SHA [BRANCH]: the recorded run if it satisfies PRED, else the
# newest of the listed runs that does (at most five are looked at; a dispatched one must also be
# run_ours). Prints the id; returns 1 if none, 2 if GitHub could not be asked.
find_run() {
  local pred=$1 rec=$2 id st concl n=0 ids rc
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
      if [ "$4" = workflow_dispatch ]; then
        rc=0
        run_ours "$id" || rc=$?
        [ $rc != 2 ] || return 2
        [ $rc = 0 ] || continue
      fi
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
  # two or more releases of the tag (the double-draft trap): which one `release view` shows is
  # GitHub's choice, so nothing after tag-run counts until tag-run has left one
  local rels
  if ! rels=$(rel_list); then REL=unknown; return 0; fi
  [ "$(printf '%s\n' "$rels" | grep -c .)" -le 1 ] || REL=multiple
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

# In a run, the candidate is checked once, at the first detection: a merge to Gitea's main while the
# run waits (the dry run, the rebuild) is not the release commit failing its checks; the next run
# fast-forwards to it and rebinds. A fetch of the crates that fails on the network is unknown, not
# "not done".
CAND_ONCE='' CAND_NOTE=''
det_candidate() {
  local log=$scratch/candidate.log
  if [ "$CAND_ONCE" = 1 ]; then
    NOTE="$CAND_NOTE (checked at the start of this run)"
    return 0
  fi
  pf_init "$V"
  PF_REPO=$repo PF_ORIGIN=$origin PF_GITHUB=$github PF_AGAIN=$AGAIN PF_LOCK_OWNER=$$ PF_R='' PF_G=''
  [ -z "$vm" ] || PF_VM=$vm
  PF_COMMIT=''
  [ "$TAG_OK" = 0 ] || PF_COMMIT=$R
  if (PF_CUR=tree && pf_check_tree && PF_CUR=version && pf_check_version && PF_CUR=lockfiles && pf_check_lockfiles) 2> "$log"; then
    NOTE="R $(short "$R"): clean, at the version, lockfiles current"
    [ "$mode" != run ] || CAND_ONCE=1 CAND_NOTE=$NOTE
    return 0
  fi
  NOTE=$(grep -m1 '^preflight: FAIL' "$log" | sed 's/^preflight: FAIL  //' | cut -c1-200)
  ! grep -q '^preflight: FAIL  lockfiles: cargo fetch .*that is the network or the registry' "$log" || UNKNOWN=1
  return 1
}

# The tag on GitHub, read into a scratch repository that borrows the clone's objects (nothing in the
# clone changes; Gitea's marker tag may own vV there), and verified with verify-tag.sh against the
# signers on GitHub's main.
TAG_STATE=unknown TAG_MSG='' TAG_TREE='' TAG_FOUND='' TAG_FIX='' FETCHED=0
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
      if [ -z "$TAG_FOUND" ] && [ "$FETCHED" = 1 ]; then
        # origin is fetched (a run): no release commit has the tag's tree, so the tag is no release's.
        TAG_STATE=burned TAG_MSG="$TAG on GitHub names $(short "$TAG_COMMIT") (tree $(short "$TAG_TREE")), and no commit on $origin's main has that tree: it is not the tag of a release commit"
        return 0
      fi
      TAG_STATE=mismatch TAG_MSG="$TAG on GitHub names $(short "$TAG_COMMIT") (tree $(short "$TAG_TREE")), and HEAD $(short "$R") is not its release commit"
      if [ -n "$TAG_FOUND" ]; then
        TAG_FIX="git checkout $TAG_FOUND (the commit on $origin/main with the tag's tree)"
      else
        TAG_FIX="git fetch $origin, then check out the release commit: git log --format='%H %T' $origin/main | awk '\$2 == \"$TAG_TREE\"' (if no commit has that tree, $TAG is burned: bump to the next patch version)"
      fi
    fi
  else
    TAG_STATE=ok TAG_MSG="$TAG (tag object $(short "$TAG_OBJ")) is signed by a release signer on GitHub's main and names $(short "$TAG_COMMIT")"
    TAG_OK=1
  fi
}

# main_trust: MAIN_BAD is why GitHub's main, past R, cannot be trusted ("" when it can, or when it
# is R, behind R, or not read). The tag and every verification read .github/release-signers from
# GitHub's main, and any account that may push main can fast-forward it, so a commit there past R
# must be one of Gitea's main (reviewed there) and must leave release-signers exactly as R has it.
MAIN_BAD=''
main_trust() {
  MAIN_BAD=''
  [ -n "$GMAIN" ] && [ "$GMAIN" != "$R" ] || return 0
  if ! has_commit "$GMAIN"; then
    MAIN_BAD="$github's main ($(short "$GMAIN")) is not in the clone: git fetch $github"
    return 1
  fi
  rg merge-base --is-ancestor "$R" "$GMAIN" || return 0
  if ! rg merge-base --is-ancestor "$GMAIN" "refs/remotes/$origin/main" 2> /dev/null; then
    MAIN_BAD="$github's main ($(short "$GMAIN")) is past the release commit with commits Gitea's main ($origin/main) lacks: they must reach Gitea's main first (a PR there)"
  elif [ "$(rg rev-parse -q --verify "$GMAIN:.github/release-signers" 2> /dev/null || true)" != "$(rg rev-parse -q --verify "$R:.github/release-signers" 2> /dev/null || true)" ]; then
    MAIN_BAD="$github's main ($(short "$GMAIN")) has a .github/release-signers that differs from the release commit's: the tag and the verifications read it from main"
  fi
  [ -z "$MAIN_BAD" ]
}

# The mirror is done when the release commit R itself is on GitHub's main (main is R, or moved on
# past it): G is then R. Agents fast-forward GitHub's main to Gitea's after each merge batch, so R is
# often there before the release starts. MIRROR is ff (main is an ancestor of R: the step
# fast-forwards it), diverged (main has commits R lacks: refused, never forced) or untrusted (past R,
# but not as main_trust wants it: refused) when not done. With a good tag, G is the commit it names
# (R, or a legacy mirror commit with R's tree); otherwise G is R.
MIRROR=''
# MIRROR names why the step is not done, for a reader of the trace; nothing branches on it.
# shellcheck disable=SC2034
det_mirror() {
  MIRROR=''
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
  if [ "$GMAIN" = "$R" ]; then
    G=$R
    NOTE="$github's main is the release commit $(short "$R")"
    return 0
  fi
  if rg merge-base --is-ancestor "$R" "$GMAIN"; then
    if [ -n "$MAIN_BAD" ]; then
      MIRROR=untrusted NOTE=$MAIN_BAD
      return 1
    fi
    G=$R
    NOTE="the release commit $(short "$R") is on $github's main, which has moved on to $(short "$GMAIN")"
    return 0
  fi
  if rg merge-base --is-ancestor "$GMAIN" "$R"; then
    MIRROR=ff NOTE="$github's main ($(short "$GMAIN")) is behind the release commit: a fast-forward pushes it"
    return 1
  fi
  MIRROR=diverged NOTE="$github's main ($(short "$GMAIN")) has commits the release commit lacks: it cannot be fast-forwarded"
  return 1
}

# The waiver of red suite=all checks (the owner's answer to "do they block the release?", at a
# terminal): checks_waived=<linux-check ID.ATTEMPT or -> <macos-check ID.ATTEMPT or -> <utc>.
# waived_run POS WF: the run of WF the waiver names (field POS), printed, if it is still exactly that:
# the run the state records for WF, on G, completed and not successful, at the waived attempt.
waived_run() {
  local f id at
  f=$(st_get checks_waived | awk -v p="$1" '{ print $p }')
  case $f in *.*) id=${f%%.*} at=${f#*.} ;; *) return 1 ;; esac
  is_num "$id" && is_num "$at" || return 1
  [ "$id" = "$(st_num "$(dr_key "$2")")" ] || return 1
  run_view "$id" || return 1
  rv_has "$id" "workflowName ${2%.yml}" && rv_has "$id" "event workflow_dispatch" && rv_has "$id" "headSha $G" \
    && rv_has "$id" "status completed" && ! rv_has "$id" "conclusion success" && [ "$(rv_attempt "$id")" = "$at" ] || return 1
  echo "$id"
}
# check_holds WF ID POS: ID is a green suite=all run of WF on G, or exactly the waived one.
check_holds() {
  [ -n "$2" ] || return 1
  if run_view "$2" && "$(dr_pred "$1")" "$2"; then return 0; fi
  [ "$(waived_run "$3" "$1" || true)" = "$2" ]
}
WAIVED=''
det_dry_run() {
  local rc miss='' id
  WAIVED=''
  [ -n "$G" ] || { NOTE="needs the release commit on GitHub's main (step mirror)"; return 1; }
  rc=0; DRY_RUN=$(find_run pred_dry "$(st_num dry_run)" release.yml workflow_dispatch "$G") || rc=$?
  [ $rc != 2 ] || { UNKNOWN=1 NOTE="cannot ask GitHub for the runs: $(gh_err)"; return 1; }
  rc=0; CHECKS_LINUX=$(find_run pred_linux_all "$(st_num checks_linux)" linux-check.yml workflow_dispatch "$G") || rc=$?
  [ $rc != 2 ] || { UNKNOWN=1 NOTE="cannot ask GitHub for the runs: $(gh_err)"; return 1; }
  rc=0; CHECKS_MACOS=$(find_run pred_macos_all "$(st_num checks_macos)" macos-check.yml workflow_dispatch "$G") || rc=$?
  [ $rc != 2 ] || { UNKNOWN=1 NOTE="cannot ask GitHub for the runs: $(gh_err)"; return 1; }
  # a red check counts only through the waiver, and only with the dry run itself green
  if [ -n "$DRY_RUN" ]; then
    if [ -z "$CHECKS_LINUX" ] && id=$(waived_run 1 linux-check.yml); then CHECKS_LINUX=$id WAIVED="$WAIVED, linux-check run $id"; fi
    if [ -z "$CHECKS_MACOS" ] && id=$(waived_run 2 macos-check.yml); then CHECKS_MACOS=$id WAIVED="$WAIVED, macos-check run $id"; fi
  fi
  [ -n "$DRY_RUN" ] || miss="$miss, a successful release.yml dry run"
  [ -n "$CHECKS_LINUX" ] || miss="$miss, a successful linux-check suite=all"
  [ -n "$CHECKS_MACOS" ] || miss="$miss, a successful macos-check suite=all"
  if [ -n "$miss" ]; then
    NOTE="on $(short "$G") there is no${miss#,}"
    return 1
  fi
  NOTE="dry run $(run_url "$DRY_RUN"), linux-check $(run_url "$CHECKS_LINUX"), macos-check $(run_url "$CHECKS_MACOS")"
  [ -z "$WAIVED" ] || NOTE="$NOTE; red and WAIVED by you ($(st_get checks_waived | awk '{ print $NF }')):${WAIVED#,}"
}

det_dry_verify() {
  local rec
  rec=$(st_get dry_verify)
  # The record alone does not prove the checks: they are required too, so that a recorded
  # dry-verify never lets a red or missing check read as done (implied).
  # Each check must be green, or exactly the run (and attempt) the waiver names.
  if [ -n "$DRY_RUN" ] && [ -n "$G" ] && check_holds linux-check.yml "$CHECKS_LINUX" 1 && check_holds macos-check.yml "$CHECKS_MACOS" 2; then
    case $rec in
      "ok $DRY_RUN $G "*) NOTE="recorded: release-verify --dry-run of run $DRY_RUN passed (${rec##* })"; return 0 ;;
    esac
  fi
  if [ -z "$DRY_RUN" ] || [ -z "$CHECKS_LINUX" ] || [ -z "$CHECKS_MACOS" ]; then
    NOTE="needs the dry run and both suite=all checks green or waived (step dry-run)"
    return 1
  fi
  NOTE="no recorded release-verify --dry-run of run $DRY_RUN at the release commit"
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
    multiple) NOTE="more than one release of $TAG on GitHub (step tag-run keeps one)"; return 1 ;;
  esac
}

# rel_list: "ID DRAFT CREATED" for every release of the tag (draft or not), by id (GitHub's ids grow,
# so the last is the newest; created_at is the tagged commit's date, shown for information only); 1
# if GitHub could not be asked.
rel_list() {
  local out
  out=$(gh_read api "repos/$repo/releases" --paginate --jq ".[] | select(.tag_name == \"$TAG\") | [.id, .draft, .created_at] | @tsv") || return 1
  printf '%s\n' "$out" | python3 -I -c 'import re, sys
rows = []
for l in sys.stdin.read().splitlines():
    f = (l.split("\t") + ["", "", ""])[:3]
    if re.fullmatch(r"[0-9]{1,19}", f[0]) and f[1] in ("true", "false"):
        rows.append((f[2] if re.fullmatch(r"[0-9T:Z-]{1,30}", f[2]) else "-", int(f[0]), f[1]))
for c, i, d in sorted(rows, key=lambda r: r[1]):
    print(i, d, c)'
}
det_tag_run() {
  local rc rels n f missing=''
  need_tag || return 1
  rc=0; TAG_RUN=$(find_run pred_tag_run "$(st_num tag_run)" release.yml push "$G" "$TAG") || rc=$?
  [ $rc != 2 ] || { UNKNOWN=1 NOTE="cannot ask GitHub for the runs: $(gh_err)"; return 1; }
  [ -n "$TAG_RUN" ] || { NOTE="no release.yml run of the tag push on $(short "$G") whose publish job succeeded (yet)"; return 1; }
  rels=$(rel_list) || { UNKNOWN=1 NOTE="cannot list the releases: $(gh_err)"; return 1; }
  n=$(printf '%s\n' "$rels" | grep -c '[0-9]' || true)
  if [ "$n" -gt 1 ]; then
    NOTE="$n releases for $TAG (ids $(printf '%s\n' "$rels" | awk '{ printf "%s ", $1 }')): the double-draft trap; the command keeps the newest draft and deletes the stale ones (after a [y/N])"
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
  # (not the .signed-locally marker: after verify-signed, $FILES is a fresh download of the uploaded,
  # signed files, which the signatures below prove as well)
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

# formula_read FILE: the version the formula's archive URLs name ("-" unless they all name one), then
# "NAME SHA256" for each url and sha256 pair.
formula_read() {
  python3 -I -c 'import re, sys
t = open(sys.argv[1]).read()
pairs = re.findall(r"url\s+\"[^\"]*/(fil-[^\"/]+\.tar\.gz)\"\s*\n\s*sha256\s+\"([0-9a-f]{64})\"", t)
vs = set()
for n, _ in pairs:
    m = re.fullmatch(r"fil-(.+)-(universal-apple-darwin|x86_64-linux-musl|aarch64-linux-musl)\.tar\.gz", n)
    vs.add(m.group(1) if m else "?")
print(vs.pop() if len(vs) == 1 and "?" not in vs else "-")
for n, h in pairs:
    print(n, h)' "$1"
}
# ver_older A B: version A sorts before B (semver precedence: a pre-release before its release).
# 0 yes, 1 no, 2 not a version.
ver_older() {
  python3 -I -c 'import sys
def key(v):
    core, _, pre = v.partition("-")
    nums = [int(x) for x in core.split(".")]
    if len(nums) != 3:
        raise ValueError(v)
    if not pre:
        return (nums, 1, [])
    return (nums, 0, [(0, int(p), "") if p.isdigit() else (1, 0, p) for p in pre.split(".")])
try:
    sys.exit(0 if key(sys.argv[1]) < key(sys.argv[2]) else 1)
except ValueError:
    sys.exit(2)' "$1" "$2"
}
# tap_formula_state MAIN: TAP_STATE of Formula/fil.rb at the tap's commit MAIN: done (version V, with
# the published SHA256SUMS's checksums: DRAFT_SUMS), other (version V, other checksums), none, older,
# newer or unknown; TAP_VER its version.
TAP_STATE='' TAP_VER=''
tap_formula_state() {
  local n h c=0 bad=0 rc
  TAP_STATE=none TAP_VER=''
  git -C "$tap" show "$1:Formula/fil.rb" > "$scratch/tap-fil.rb" 2> /dev/null || return 0
  formula_read "$scratch/tap-fil.rb" > "$scratch/tap-formula" 2> /dev/null || { TAP_STATE=unknown; return 0; }
  TAP_VER=$(head -n 1 "$scratch/tap-formula")
  if [ "$TAP_VER" = "$V" ]; then
    while read -r n h; do
      c=$((c + 1))
      grep -qxF "$h  $n" "$DRAFT_SUMS" || bad=1
    done < <(tail -n +2 "$scratch/tap-formula")
    if [ "$c" = 3 ] && [ "$bad" = 0 ]; then TAP_STATE='done'; else TAP_STATE='other'; fi
    return 0
  fi
  rc=0
  ver_older "$TAP_VER" "$V" || rc=$?
  case $rc in 0) TAP_STATE=older ;; 1) TAP_STATE=newer ;; *) TAP_STATE=unknown ;; esac
}
# The formula on the tap's main must name V, with the checksums the published SHA256SUMS lists (not
# byte for byte the rendered one: a fix made in the tap afterwards still counts).
det_brew() {
  local main
  need_published || return 1
  draft_sums || { UNKNOWN=1 NOTE="cannot read the published SHA256SUMS: $(gh_err)"; return 1; }
  git -C "$tap" rev-parse --git-dir > /dev/null 2>&1 || { UNKNOWN=1 NOTE="no tap clone at $tap (--tap DIR)"; return 1; }
  main=$(git -C "$tap" ls-remote origin refs/heads/main 2> /dev/null | awk '{ print $1 }') || main=''
  [ -n "$main" ] || { UNKNOWN=1 NOTE="cannot read main of the tap's origin ($tap)"; return 1; }
  git -C "$tap" cat-file -e "$main^{commit}" 2> /dev/null || { UNKNOWN=1 NOTE="the tap's main ($(short "$main")) is not fetched: git -C $tap fetch origin"; return 1; }
  tap_formula_state "$main"
  case $TAP_STATE in
    done) NOTE="Formula/fil.rb on the tap's main ($(short "$main")) is $V's (its version and the published checksums)" ;;
    none) NOTE="the tap's main has no Formula/fil.rb"; return 1 ;;
    other) NOTE="Formula/fil.rb on the tap's main names $V, but not with the published SHA256SUMS's checksums"; return 1 ;;
    *) NOTE="Formula/fil.rb on the tap's main is version ${TAP_VER:--}, not $V"; return 1 ;;
  esac
}

det_finish() { NOTE="the summary, forgetting the keys, the copy for the coordinator"; return 1; }

# ---- detect every step: KIND[i] is checked, recorded, waived, no or unknown, then
# implied/next/pending. A step that --redo still has to do again (REDO_LEFT) reads as no, and is
# never implied by a later one.
KIND=() NOTES=() FORCED=() WAS_DONE=()
in_redo() { case " $REDO_LEFT " in *" $1 "*) true ;; *) false ;; esac; }
detect_all() {
  local s i=0
  TAG_OK=0 TAG_STATE=unknown HARD='' HARD_STEP='' LAST_DONE=0 G='' REL=unknown
  DRY_RUN='' CHECKS_LINUX='' CHECKS_MACOS='' TAG_RUN='' pkg_fpr=''
  rm -f "$scratch"/run-* "$scratch/assets" "$scratch/release"
  rm -rf "$scratch/dl" "$scratch/gpghome" "$scratch/sums" "$scratch/brew"
  GMAIN=$(rg ls-remote "$github" refs/heads/main 2> /dev/null | awk '{ print $1 }') || GMAIN=''
  main_trust || true
  read_tag
  [ "$TAG_OK" = 0 ] || read_release
  for s in $PF_STEPS; do
    i=$((i + 1))
    NOTE='' UNKNOWN=0
    FORCED[i]=0 WAS_DONE[i]=0
    if "det_$(printf '%s' "$s" | tr - _)"; then
      WAS_DONE[i]=1
      case $s in dry-verify | verify-published) KIND[i]=recorded ;; *) KIND[i]=checked ;; esac
      [ "$s" != dry-run ] || [ -z "$WAIVED" ] || KIND[i]=waived
      if in_redo "$s"; then
        KIND[i]=no FORCED[i]=1 NOTE="--redo $redo: done, and done again now ($NOTE)"
      else
        LAST_DONE=$i
      fi
    elif in_redo "$s" && [ "$UNKNOWN" = 0 ]; then
      KIND[i]=no FORCED[i]=1
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
      checked | recorded | waived) ;;
      # verify-published is never implied: a release whose published files did not verify (or whose
      # record was lost) verifies again, whatever reached crates.io or the tap meanwhile. Nor is the
      # candidate before the tag: the mirror is done whenever the agents have pushed GitHub's main to
      # the release commit, which says nothing of the clone, the version or the lockfiles
      *) if [ $i -lt "$LAST_DONE" ] && [ "${FORCED[i]}" = 0 ] && [ "$(step_name $i)" != verify-published ] \
        && { [ "$(step_name $i)" != candidate ] || [ "$TAG_OK" = 1 ]; }; then KIND[i]=implied; elif [ "$NEXT" = 0 ]; then NEXT=$i; else KIND[i]=pending; fi ;;
    esac
  done
}

label() {
  case $1 in
    checked) echo "done (checked)" ;;
    recorded) echo "done (recorded)" ;;
    waived) echo "done (waived)" ;;
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
  [ -z "$G" ] || [ "$G" = "$R" ] || echo "  tagged commit (GitHub): $G"
  [ -z "$MAIN_BAD" ] || echo "  WARNING: $MAIN_BAD"
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
  echo "keys (tests/release/just.sh release-keys status):"
  "$KEYS" status --need ssh,gpg < /dev/null 2>&1 | sed 's/^/  /' || true
}

# ---- the actions, from step 1. Each runs with its step's log open and returns once its action is
# done (the loop then detects every step again); any failure stops the run through step_fail.
utc() { date -u +%Y-%m-%dT%H:%M:%SZ; }
STEP_LOG='' STEP_LOG_START=0
step_log() {
  mkdir -p "$W/logs"
  STEP_LOG=$W/logs/$(printf '%02d' "$(step_no "$1")")-$1.log
  STEP_LOG_START=0
  [ ! -f "$STEP_LOG" ] || STEP_LOG_START=$(wc -l < "$STEP_LOG" | tr -d ' ')
  echo "=== $(utc) step $1 (pid $$)" >> "$STEP_LOG"
}
# this_attempt: this run's part of the step's log (the log keeps every run's, appended).
this_attempt() { tail -n "+$((STEP_LOG_START + 1))" "$STEP_LOG" 2> /dev/null || true; }
# log_has RE: this run's part of the step's log matches RE (extended, any case).
log_has() { this_attempt | grep -Eiq -- "$1"; }
# known_fix STEP: the "do" line for a failure whose log shows a known sign (0.1.0's troubles among them).
known_fix() {
  local m=${vm:-${FIL_VM:-fil}}
  case $1 in
    mirror)
      if log_has 'non-fast-forward|fetch first|stale info'; then
        echo "GitHub's main moved since the fetch to a commit the release commit lacks: run it again (it fetches, and refuses if main has commits Gitea's main does not)"
      elif log_has 'GH006|GH013|protected branch|rule violation|ruleset'; then
        echo "GitHub's main ruleset refused a fast-forward push: it should block only deletion and non-fast-forward pushes (the repository's Settings, Rules); push as $PF_OWNER (ssh -T git@github.com must say Hi $PF_OWNER)"
      elif log_has 'permission denied \(publickey\)|could not read from remote'; then
        echo "ssh-add your GitHub key (ssh -T git@github.com must say Hi $PF_OWNER)"
      fi
      ;;
    dry-run)
      if log_has 'HTTP 401|gh auth login|authentication'; then
        echo "gh auth status; gh auth refresh -h github.com -s repo,workflow (as $PF_OWNER)"
      elif log_has 'rate limit|HTTP 429'; then
        echo "GitHub's API limit: wait (gh api rate_limit says until when)"
      elif log_has 'HTTP 403|resource not accessible|must have admin rights'; then
        echo "gh's token lacks a scope or a right: gh auth refresh -h github.com -s repo,workflow (as $PF_OWNER, an admin of $repo)"
      elif log_has 'workflow_dispatch|HTTP 422'; then
        echo "GitHub refused the dispatch: the workflow on GitHub's main needs workflow_dispatch (gh workflow list --repo $repo)"
      elif log_has 'HTTP 5[0-9][0-9]|bad gateway|timed? ?out|connection (reset|refused)|could not resolve'; then
        echo "GitHub or the network failed for a moment: run it again (it re-attaches to the runs it started)"
      fi
      ;;
    verify-published)
      # the transient and missing-tool failures first: verify.sh says STOP for them too, and the step
      # only verifies, so running it again is always safe
      if log_has 'HTTP 5[0-9][0-9]|bad gateway|timed? ?out|connection (reset|refused)|could not resolve|could not fetch|rpmkeys is needed|not found'; then
        echo "a network, GitHub or tool failure, not necessarily the release: run it again; if it fails the same way, treat it as a STOP (do NOT approve crates.io or touch the tap) and send the log to the coordinator"
      elif log_has 'FAILED|released [0-9a-f]*, rebuilt|STOP: |does not verify|not a valid'; then
        echo "STOP: the PUBLISHED release did not verify: do NOT approve crates.io or touch the tap. Keep the log and send it to the coordinator; a published release cannot change, so the fix is a new patch version"
      fi
      ;;
    dry-verify | verify-presign | verify-signed)
      if log_has "Lima instance .* is not running"; then
        echo "limactl start $m"
      elif log_has 'docker\.sock.*permission denied|permission denied.*docker\.sock'; then
        echo "add yourself to the VM's docker group: limactl shell $m sudo usermod -aG docker \$USER, then limactl stop $m && limactl start $m"
      elif log_has 'Cannot connect to the Docker daemon|docker daemon running'; then
        echo "start Docker (on the Mac: open -a Docker; in the VM: limactl shell $m sudo systemctl start docker)"
      elif log_has 'FAILED: do not sign|released [0-9a-f]*, rebuilt|STOP: '; then
        case $1 in
          dry-verify) echo "STOP: do NOT tag. The dry run did not verify (a rebuild that differs is a reproducibility bug, not a flake); keep the log and send it to the coordinator. The fix is a new release commit" ;;
          verify-presign) echo "STOP: do NOT sign. The draft did not verify; it stays a draft. Keep the log and send it to the coordinator; the fix is a new patch version (the tag cannot move)" ;;
          *) echo "STOP: do NOT publish. The signed draft did not verify; the signed files are kept (target/release-verify/$V/files or files.signed-*). Send the log to the coordinator" ;;
        esac
      elif log_has 'HTTP 5[0-9][0-9]|bad gateway|timed? ?out|connection (reset|refused)|could not resolve'; then
        echo "GitHub or the network failed for a moment: run it again"
      fi
      ;;
    tag)
      if log_has 'GH013|rule violation|ruleset|protected|cannot (create|update) (this|a) (protected )?(ref|tag)'; then
        echo "GitHub refused the tag push: the v* tag ruleset lets only $PF_OWNER's admin bypass create it (ssh -T git@github.com must say Hi $PF_OWNER)"
      elif log_has 'already exists|\[rejected\]'; then
        echo "a $TAG appeared on GitHub meanwhile: run it again (it checks that tag; one that is not this release's burns the version)"
      elif log_has 'permission denied \(publickey\)|could not read from remote'; then
        echo "ssh-add your GitHub key (ssh -T git@github.com must say Hi $PF_OWNER)"
      elif log_has 'failed to sign|no private key found|agent refused|could not sign|signing failed|incorrect passphrase|couldn.t find key|ssh-keygen.*(error|failed)'; then
        echo "the release key is not in the agent: tests/release/just.sh release-keys unlock --need ssh --ttl $ttl, or run it again at a terminal (it asks)"
      fi
      ;;
    tag-run | upload | publish | crates)
      if [ "$1" = crates ] && log_has 'HTTP 422'; then
        echo "GitHub refused the approval (the deployment was approved or rejected meanwhile, or is no longer waiting): run it again (it looks at the job again)"
      elif log_has 'HTTP 401|gh auth login|authentication'; then
        echo "gh auth status; gh auth refresh -h github.com -s repo,workflow (as $PF_OWNER)"
      elif log_has 'rate limit|HTTP 429'; then
        echo "GitHub's API limit: wait (gh api rate_limit says until when)"
      elif log_has 'HTTP 403|resource not accessible|must have admin rights'; then
        echo "gh's token lacks a scope or a right: gh auth refresh -h github.com -s repo,workflow (as $PF_OWNER, an admin of $repo)"
      elif log_has 'HTTP 5[0-9][0-9]|bad gateway|timed? ?out|connection (reset|refused)|could not resolve'; then
        echo "GitHub or the network failed for a moment: run it again (it finishes what is not done, and repeats nothing that is)"
      fi
      ;;
    sign-macos)
      if log_has 'errSecInternalComponent|user interaction is not allowed|keychain.*locked'; then
        echo "unlock the login keychain (security unlock-keychain), then run it again"
      elif log_has '"status": "(Invalid|Rejected)"|status: (Invalid|Rejected)'; then
        echo "Apple rejected the notarization (its log is in target/release-verify/$V/macsign-logs); nothing was uploaded: send it to the coordinator"
      elif log_has 'agent refused|couldn.t find key|signing failed|no private key'; then
        echo "the release key is not in the agent: tests/release/just.sh release-keys unlock --need ssh --ttl $ttl, or run it again at a terminal (it asks)"
      elif log_has 'HTTP 5[0-9][0-9]|bad gateway|timed? ?out|connection (reset|refused)|could not resolve|network'; then
        echo "Apple, GitHub or the network failed for a moment: run it again (sign-macos.sh resumes; nothing is signed twice)"
      fi
      ;;
    sign-packages)
      if log_has 'no pinentry|bad passphrase|inappropriate ioctl|signing failed|no secret key|cannot open .*/dev/tty'; then
        echo "the package key's passphrase is not cached in gpg-agent: tests/release/just.sh release-keys unlock --need gpg --ttl $ttl, or run it again at a terminal (it asks)"
      fi
      ;;
    sign-sums)
      if log_has 'agent refused|couldn.t find key|no such identity|signing failed|incorrect passphrase|load key|no private key'; then
        echo "the release key is not in the agent: tests/release/just.sh release-keys unlock --need ssh --ttl $ttl, or run it again at a terminal (it asks)"
      fi
      ;;
    brew)
      if log_has 'non-fast-forward|fetch first|\[rejected\]|stale info'; then
        echo "the tap's main moved since the fetch: run it again (it fetches the tap and makes the commit on its new main)"
      elif log_has 'permission denied \(publickey\)|could not read from remote'; then
        echo "ssh-add your GitHub key (ssh -T git@github.com must say Hi $PF_OWNER)"
      elif log_has 'failed to sign|no private key found|agent refused|could not sign|signing failed|incorrect passphrase|ssh-keygen.*(error|failed)'; then
        echo "git cannot sign the tap commit: ssh-add the key that git config user.signingkey names, then run it again"
      fi
      ;;
    finish)
      if log_has 'release-keys'; then
        echo "forget the keys yourself: tests/release/just.sh release-keys forget --need ssh,gpg (then run it again for the summary)"
      fi
      ;;
  esac
}
# step_fail WHAT [DO]: the failure of the step being acted on (NEXT): the log's tail, the known fix or
# DO, and exit 1.
step_fail() {
  local fix=${2:-}
  if [ -s "$STEP_LOG" ]; then
    echo "release: the last lines of this run's part of $STEP_LOG:" >&2
    this_attempt | tail -n 20 | sed 's/^/  | /' >&2
  fi
  [ -n "$fix" ] || fix=$(known_fix "$(step_name "$NEXT")")
  [ -n "$fix" ] || fix="not a known failure: read the log; if it does not say what to do, send it to the coordinator"
  LAST_DONE=$((NEXT - 1))
  report FAILED "$NEXT" "$1 (log: $STEP_LOG)" "$fix"
}

# confirm POINT LINE...: shows what becomes public, then wants the version typed back. Input that is
# not a terminal, or anything else typed, pauses the run (exit 2) with nothing done, saying what it
# received. What was typed before the question is discarded (tty_flush), and the white space around
# the answer is ignored. CONFIRMED lists
# the points confirmed in this run, in this process only, never in the state: step tag asks again
# unless this run confirmed `tag` (the default mirror confirmation is `tag`).
CONFIRMED='' R_CHANGED=''
is_tty() { [ "${FIL_RELEASE_TTY:-}" = 1 ] || [ -t 0 ]; }
# tty_flush: before a prompt reads its answer, what was typed ahead (an Enter pressed during a long
# wait) is discarded, so it is not read as the answer. Only a real terminal has a queue to flush
# (tcflush; bash 3.2's read -t takes whole seconds only, and read -t 0 reads nothing in bash 4 and
# later); anything else (the self-test's answers) is read as it is.
tty_flush() {
  [ -t 0 ] || return 0
  python3 -I -c 'import termios; termios.tcflush(0, termios.TCIFLUSH)' 2> /dev/null || true
}
# trim TEXT: TEXT without its leading and trailing white space (a CR, a tab, spaces).
trim() {
  local t=$1
  t=${t#"${t%%[![:space:]]*}"}
  printf '%s' "${t%"${t##*[![:space:]]}"}"
}
# shown TEXT: TEXT quoted for a message, non-printable characters visible (printf %q).
shown() {
  local q
  [ -n "$1" ] || { printf 'an empty line'; return; }
  q=$(printf '%q' "$1")
  if [ "$q" = "$1" ]; then printf "'%s'" "$1"; else printf '%s' "$q"; fi
}
confirm() {
  local point=$1 ans
  shift
  LAST_DONE=$((NEXT - 1))
  {
    echo "release: CONFIRM $point, for $V. What happens and cannot be undone:"
    printf '  - %s\n' "$@"
    # the first confirmation of a run that moved the release commit says so
    [ -z "$R_CHANGED" ] || printf '  - %s\n' "$R_CHANGED"
  } >&2
  R_CHANGED=''
  is_tty || report PAUSED "$NEXT" "step $(step_name "$NEXT") needs your confirmation ($point), and the input is not a terminal: nothing was done" \
    "run it in a terminal (there is no --yes)"
  tty_flush
  printf 'release: type %s to go ahead (anything else stops here): ' "$V" >&2
  IFS= read -r ans || ans=''
  ans=$(trim "$ans")
  [ "$ans" = "$V" ] || report PAUSED "$NEXT" "not confirmed: nothing was done (received $(shown "$ans"), not $V)" "run it again when you are ready"
  CONFIRMED="$CONFIRMED $point"
}

act_candidate() {
  LAST_DONE=0
  report FAILED "$NEXT" "the release commit is not a candidate: ${NOTES[NEXT]}" \
    "fix it on Gitea (a PR to main), then run it from the new main; there is no tag yet, so $AGAIN --reset-state rebinds the state to the new commit"
}

# mirror_refuse WHAT: GitHub's main is not one the mirror fast-forwards (the preflight refuses these
# at the start; this is for a main that moved during the confirmation).
mirror_refuse() {
  echo "$github's main $GMAIN, the release commit $R: $1" >> "$STEP_LOG"
  step_fail "$1; the mirror only fast-forwards GitHub's main to the release commit (it never forces)" \
    "find out what they are (git fetch $github; git log $R..$github/main) and who pushed them; they must reach Gitea's main first (a PR there), then release from the new main ($AGAIN --reset-state rebinds, there is no tag yet)"
}
act_mirror() {
  local point=tag n now
  step_log mirror
  [ "$confirm_each" = 0 ] || point=mirror
  # (a main that is diverged or untrusted at the start never gets here: the preflight's github-main
  # check refuses it; main is judged again after the confirmation, below)
  n=$(rg rev-list --count "$GMAIN..$R")
  set -- "step 2 mirror: GitHub's main fast-forwarded from $(short "$GMAIN") to the release commit $(short "$R") (new commits: $n), pushed to github.com/$repo: public history, which GitHub keeps" \
    "its changes to GitHub's main: $(rg diff --shortstat "$GMAIN" "$R" | sed 's/^ *//')"
  if [ "$point" = tag ]; then
    set -- "$@" "step 5 tag (this answer covers it too): the tag $TAG on the release commit, signed with the release key; the tag ruleset forbids moving or deleting it, so a bad tag means a new version" \
      "between the two: the dry run, the suite=all checks and the dry run's verification, each of which stops the release on its own"
  else
    set -- "$@" "(--confirm-each: the tag is asked for on its own, at step 5)"
  fi
  confirm "$point" "$@"
  # Main may have moved while the question waited: it is read again, and only a main that is still
  # an ancestor of R is fast-forwarded. A main at R or past it (the agents pushed it) needs nothing
  # (main_trust and the next detection judge a main past R); any other is refused.
  now=$(rg ls-remote "$github" refs/heads/main 2> /dev/null | awk '{ print $1 }') || now=''
  [ -n "$now" ] || step_fail "cannot read main on '$github' after the confirmation" "run it again"
  has_commit "$now" || rg fetch -q --no-tags "$github" 2>> "$STEP_LOG" || true
  has_commit "$now" || step_fail "$github's main moved to $(short "$now") during the confirmation, and it cannot be fetched" "run it again"
  if [ "$now" = "$R" ] || rg merge-base --is-ancestor "$R" "$now"; then
    echo "$github's main is $now now, which has the release commit $R: nothing to push" >> "$STEP_LOG"
    log "$github's main moved to $(short "$now") during the confirmation, and has the release commit: nothing to push"
    # the tag step verifies against refs/remotes/$github/main, which must have caught up
    rg fetch -q --no-tags "$github" 2>> "$STEP_LOG" || step_fail "git fetch $github failed" "run it again"
    return 0
  fi
  rg merge-base --is-ancestor "$now" "$R" \
    || { GMAIN=$now; mirror_refuse "GitHub's main moved during the confirmation to $(short "$now"), which the release commit does not contain"; }
  # A plain push, never forced: GitHub takes it only as a fast-forward of what main is then (a move in
  # the last moment to a commit R lacks is refused by git). LC_ALL=C: known_fix reads git's English.
  log "fast-forwarding $github's main to the release commit $(short "$R")"
  echo "git push $github $R:refs/heads/main (main was $GMAIN)" >> "$STEP_LOG"
  LC_ALL=C rg push "$github" "$R:refs/heads/main" >> "$STEP_LOG" 2>&1 || step_fail "git push $github $(short "$R"):main failed"
}

# The dry run's three runs: release.yml, and linux-check and macos-check with suite=all.
DR_WFS="release.yml linux-check.yml macos-check.yml"
dr_key() { case $1 in release.yml) echo dry_run ;; linux-check.yml) echo checks_linux ;; *) echo checks_macos ;; esac; }
dr_pred() { case $1 in release.yml) echo pred_dry ;; linux-check.yml) echo pred_linux_all ;; *) echo pred_macos_all ;; esac; }
# dr_class WF: DR_CLASS is passed, running, failed or none for WF's run on G, DR_ID its id: the run the
# state names if it is one of WF on G, else a run of WF on G still in progress (the state was lost
# while it ran: it is attached to, not dispatched again).
DR_CLASS='' DR_ID=''
dr_class() {
  local wf=$1 id st concl ids rc
  DR_CLASS=none DR_ID=''
  id=$(st_num "$(dr_key "$wf")")
  if [ -n "$id" ]; then
    rm -f "$scratch/run-$id"
    if ! run_view "$id"; then
      grep -Eqi 'not found|HTTP 404|could not find' "$scratch/gh.err" 2> /dev/null || step_fail "cannot ask GitHub for run $id: $(gh_err)"
      # GitHub no longer has it (deleted): forget it, and look for the run as if none was recorded
      echo "the recorded $wf run $id is not on GitHub: forgotten" >> "$STEP_LOG"
      st_unset "$(dr_key "$wf")"
      id=''
    fi
  fi
  if [ -n "$id" ]; then
    if rv_has "$id" "workflowName ${wf%.yml}" && rv_has "$id" "event workflow_dispatch" && rv_has "$id" "headSha $G"; then
      DR_ID=$id
      if "$(dr_pred "$wf")" "$id"; then DR_CLASS=passed; return 0; fi
      if ! rv_has "$id" "status completed"; then DR_CLASS=running; return 0; fi
      if ! rv_has "$id" "conclusion success"; then DR_CLASS=failed; return 0; fi
      # it succeeded but is not the run this step needs (not suite=all): a new one is dispatched
      DR_ID=''
    fi
  fi
  ids=$(run_ids "$wf" workflow_dispatch "$G") || step_fail "cannot ask GitHub for the runs of $wf: $(gh_err)"
  while read -r id st concl; do
    is_num "$id" || continue
    [ "$st" != completed ] || continue
    # only this release's: a run someone else dispatched, or one older than the release commit, may
    # test other inputs (suite, ref)
    rc=0
    run_ours "$id" || rc=$?
    [ $rc != 2 ] || step_fail "cannot ask GitHub about run $id: $(gh_err)"
    [ $rc = 0 ] || { echo "$wf run $id is running on $(short "$G") but is not this release's: not attached to" >> "$STEP_LOG"; continue; }
    DR_CLASS=running DR_ID=$id
    return 0
  done <<< "$ids"
}
# dr_dispatch WF: dispatches WF on GitHub's main (which must be G), finds its run (the URL gh prints,
# else the new run of WF on G) and records it at once. Sets DR_ID.
dr_dispatch() {
  local wf=$1 out id='' n=0 now
  local -a args=(--ref main)
  [ "$wf" = release.yml ] || args+=(-f suite=all -f "ref=$G")
  now=$(rg ls-remote "$github" refs/heads/main 2> /dev/null | awk '{ print $1 }') || now=''
  [ "$now" = "$G" ] || step_fail "GitHub's main is ${now:-unreadable}, not the release commit $(short "$G"): a dispatch on main would test another commit (release.yml's dry run builds main's head)" \
    "find out who pushed it (git fetch $github; git log $G..$github/main): GitHub's main must stay at the release commit until the dry run is dispatched, so hold the agents' pushes of main; if main moved on with merges of Gitea's main, release its new head instead (git checkout it; $AGAIN --reset-state rebinds, there is no tag yet)"
  run_ids "$wf" workflow_dispatch "$G" > "$scratch/before" || step_fail "cannot ask GitHub for the runs of $wf: $(gh_err)"
  echo "gh workflow run $wf --repo $repo ${args[*]}" >> "$STEP_LOG"
  if ! out=$("$GH" workflow run "$wf" --repo "$repo" "${args[@]}" 2>&1); then
    printf '%s\n' "$out" >> "$STEP_LOG"
    step_fail "gh workflow run $wf failed: $(printf '%s' "$out" | head -n 1 | cut -c1-200)"
  fi
  printf '%s\n' "$out" >> "$STEP_LOG"
  id=$(printf '%s\n' "$out" | sed -n "s|.*github\.com/$repo/actions/runs/\([0-9][0-9]*\).*|\1|p" | head -n 1)
  while [ -z "$id" ] && [ $n -lt 20 ]; do
    n=$((n + 1))
    id=$(run_ids "$wf" workflow_dispatch "$G" 2> /dev/null | awk -v before="$(cut -d' ' -f1 "$scratch/before" | tr '\n' ' ')" '
      BEGIN { n = split(before, b, " "); for (i = 1; i <= n; i++) seen[b[i]] = 1 }
      $1 != "" && !($1 in seen) { print $1; exit }' || true)
    [ -n "$id" ] || sleep 3
  done
  is_num "$id" || step_fail "dispatched $wf, but no new run of it on $(short "$G") appeared within a minute" \
    "find it (gh run list --repo $repo --workflow $wf --event workflow_dispatch), wait for it, then run it again: it finds a successful run by itself"
  st_set "$(dr_key "$wf")" "$id"
  DR_ID=$id
  log "dispatched $wf: $(run_url "$id")"
}
# dr_watch ID WF: waits for the run (gh run watch); the run must have completed, whatever its result.
dr_watch() {
  log "waiting for the $2 run $(run_url "$1") (Ctrl-C is safe: run the same command again to re-attach)"
  echo "gh run watch $1 ($2)" >> "$STEP_LOG"
  "$GH" run watch "$1" --repo "$repo" --exit-status --interval 30 >> "$STEP_LOG" 2>&1 < /dev/null || true
  rm -f "$scratch/run-$1"
  run_view "$1" || step_fail "cannot ask GitHub for run $1 after waiting for it: $(gh_err)"
  rv_has "$1" "status completed" || step_fail "the wait for $(run_url "$1") ended before the run did" "run it again: it re-attaches to the run"
}
# dr_rerun_started ID ATTEMPT: GitHub shows a rerun a moment after `gh run rerun`; until then the run
# still reads as the finished attempt ATTEMPT, which a watch would take for the rerun's result.
dr_rerun_started() {
  local n=0 a
  while :; do
    rm -f "$scratch/run-$1"
    run_view "$1" || step_fail "cannot ask GitHub for run $1 after the rerun: $(gh_err)"
    a=$(rv_attempt "$1")
    if ! rv_has "$1" "status completed" || [ "${a:-0}" -gt "$2" ]; then return 0; fi
    n=$((n + 1))
    [ $n -lt 12 ] || step_fail "a minute after gh run rerun, run $1 still shows its finished attempt $2" "run it again: it re-attaches to the rerun, or offers it again"
    sleep 5
  done
}
# waive_prompt FAILED: at a terminal, the red checks of FAILED ("ID:WF" words; not release.yml) may be
# waived: recorded as checks_waived with their run ids and attempts. 0 if waived.
waive_prompt() {
  local x l=- m=- ans
  echo "release: the dry run (release.yml) passed, but suite=all checks failed:$list" >&2
  echo "release: a check that fails for a reason unrelated to what is released (a flaky harness, a runner) may be waived; anything else blocks the release" >&2
  tty_flush
  printf 'release: waive them, i.e. they do NOT block the release (recorded with their run ids)? [y/N] ' >&2
  ans=''
  IFS= read -r ans || ans=''
  case $ans in y | Y | yes) ;; *) return 1 ;; esac
  for x in $1; do
    case ${x#*:} in
      linux-check.yml) l=${x%%:*}.$(rv_attempt "${x%%:*}") ;;
      macos-check.yml) m=${x%%:*}.$(rv_attempt "${x%%:*}") ;;
    esac
  done
  st_set checks_waived "$l $m $(utc)"
  # the tag confirmation given before the mirror did not know of this waiver: step 5 asks again,
  # listing it
  CONFIRMED=$(printf '%s\n' $CONFIRMED | grep -vx tag | tr '\n' ' ' || true)
  echo "the red checks waived by the owner: linux-check $l, macos-check $m" >> "$STEP_LOG"
  log "waived (recorded in $STATE): linux-check $l, macos-check $m"
}
act_dry_run() {
  local wf x watch failed round list a0 only_checks
  step_log dry-run
  watch=''
  if in_redo dry-run; then
    # new runs: the recorded ones (and a waiver of them) are forgotten, so nothing is attached to
    for x in dry_run checks_linux checks_macos checks_waived dry_verify; do st_unset "$x"; done
    echo "--redo dry-run: the recorded runs, waiver and dry-verify are forgotten; new runs are dispatched" >> "$STEP_LOG"
  fi
  for wf in $DR_WFS; do
    dr_class "$wf"
    case $DR_CLASS in
      passed) echo "$wf: run $DR_ID passed" >> "$STEP_LOG" ;;
      running) st_set "$(dr_key "$wf")" "$DR_ID"; watch="$watch $DR_ID:$wf" ;;
      failed) ;;
      none) dr_dispatch "$wf"; watch="$watch $DR_ID:$wf" ;;
    esac
  done
  for x in $watch; do dr_watch "${x%%:*}" "${x#*:}"; done
  # The results; a failure is offered one re-run of its failed jobs, at a terminal, and red checks
  # with a green dry run the waiver.
  for round in first rerun; do
    failed='' list='' only_checks=1
    for wf in $DR_WFS; do
      dr_class "$wf"
      case $DR_CLASS in
        passed) ;;
        failed)
          failed="$failed $DR_ID:$wf" list="$list $wf $(run_url "$DR_ID"),"
          [ "$wf" != release.yml ] || only_checks=0
          ;;
        *) step_fail "the $wf run on $(short "$G") ${DR_ID:+($(run_url "$DR_ID")) }is not a successful run of what this step needs" "run it again (it dispatches a new one)" ;;
      esac
    done
    [ -n "$failed" ] || return 0
    list=${list%,}
    for x in $failed; do
      echo "== the failed jobs of ${x#*:} run ${x%%:*} (gh run view --log-failed, the end):" >> "$STEP_LOG"
      "$GH" run view "${x%%:*}" --repo "$repo" --log-failed < /dev/null 2>&1 | tail -n 30 >> "$STEP_LOG" || true
    done
    if ! is_tty; then
      if [ "$only_checks" = 1 ]; then
        LAST_DONE=$((NEXT - 1))
        report PAUSED "$NEXT" "the dry run passed and suite=all checks failed:$list; whether they block the release is your decision, at a terminal: nothing was done (log: $STEP_LOG)" \
          "run it in a terminal: it offers gh run rerun ID --failed (a transient failure), then asks whether the failures block the release; a real failure: fix it on Gitea, then release the new commit ($AGAIN --reset-state)"
      fi
      step_fail "failed:$list" \
        "if it is transient (a runner or the network): gh run rerun ID --failed --repo $repo for each, then the same command (it re-attaches); a real failure: fix it on Gitea, then release the new commit ($AGAIN --reset-state)"
    fi
    echo "release: failed:$list (the end of each log is in $STEP_LOG)" >&2
    x=''
    if [ "$round" = first ]; then
      tty_flush
      printf 'release: re-run the failed jobs (for a transient failure: a runner or the network)? [y/N] ' >&2
      IFS= read -r x || x=''
    fi
    case $x in
      y | Y | yes) ;;
      *)
        if [ "$only_checks" = 1 ] && waive_prompt "$failed"; then return 0; fi
        step_fail "failed:$list; $([ "$round" = first ] && echo "not re-run" || echo "failed again after the rerun")$([ "$only_checks" = 1 ] && echo ", not waived" || true)" \
          "a real failure: fix it on Gitea, then release the new commit ($AGAIN --reset-state); a transient one: run it again and answer y"
        ;;
    esac
    for x in $failed; do
      a0=$(rv_attempt "${x%%:*}")
      echo "gh run rerun ${x%%:*} --failed (attempt ${a0:-?})" >> "$STEP_LOG"
      "$GH" run rerun "${x%%:*}" --repo "$repo" --failed >> "$STEP_LOG" 2>&1 < /dev/null || step_fail "gh run rerun ${x%%:*} --failed failed"
      dr_rerun_started "${x%%:*}" "${a0:-0}"
    done
    for x in $failed; do dr_watch "${x%%:*}" "${x#*:}"; done
  done
}

act_dry_verify() {
  local verifier=$VERIFIER rc=0
  step_log dry-verify
  log "verifying the dry run $DRY_RUN of $(short "$G"): release-verify --dry-run, with the rebuild in the VM (about 15 to 30 minutes)"
  echo "release-verify $V --dry-run --from-run $DRY_RUN --ref $G --repo $repo${vm:+ --vm $vm}" >> "$STEP_LOG"
  "$verifier" "$V" --dry-run --from-run "$DRY_RUN" --ref "$G" --repo "$repo" ${vm:+--vm "$vm"} < /dev/null 2>&1 | tee -a "$STEP_LOG" >&2 || rc=$?
  [ "$rc" = 0 ] || step_fail "release-verify --dry-run of run $DRY_RUN exited $rc"
  st_set dry_verify "ok $DRY_RUN $G $(utc)"
}

# ---- the keys (release-keys.sh). step_keys STEP: the keys STEP's action signs with.
step_keys() {
  case $1 in
    tag | sign-sums) echo ssh ;;
    sign-macos) echo ssh,apple ;;
    sign-packages) echo gpg ;;
  esac
}
# keys_needed: the keys of the steps from NEXT through verify-signed that are still to do.
keys_needed() {
  local i k ssh=0 gpg=0 apple=0 out=''
  for ((i = NEXT; i <= $(step_no verify-signed); i++)); do
    case ${KIND[i]} in checked | recorded | waived) continue ;; esac
    for k in $(step_keys "$(step_name $i)" | tr ',' ' '); do eval "$k=1"; done
  done
  [ $ssh = 0 ] || out="$out,ssh"
  [ $gpg = 0 ] || out="$out,gpg"
  [ $apple = 0 ] || out="$out,apple"
  echo "${out#,}"
}
# keys_ready NEED: the keys are unlocked (release-keys.sh status); else, at a terminal, unlocked now
# (it asks for the passphrases; no secret passes through this script); else the run pauses.
keys_ready() {
  [ -n "$1" ] || return 0
  if "$KEYS" status --need "$1" < /dev/null > "$scratch/keys.out" 2>&1; then return 0; fi
  LAST_DONE=$((NEXT - 1))
  if ! is_tty; then
    sed 's/^/  /' "$scratch/keys.out" >&2
    report PAUSED "$NEXT" "the keys $1 are not unlocked (above), and the input is not a terminal to unlock them: nothing was done" \
      "run it in a terminal (it asks for the passphrases once), or first: tests/release/just.sh release-keys unlock --need $1 --ttl $ttl"
  fi
  log "unlocking the keys the remaining steps need ($1), for $ttl"
  "$KEYS" unlock --need "$1" --ttl "$ttl" || report FAILED "$NEXT" "release-keys.sh unlock --need $1 failed (it says why above)" "fix that (tests/release/just.sh release-keys status --need $1), then run it again"
}

# ---- the actions of steps 5-12.
# tag_ours NAME: the local tag NAME is one this command made: annotated, on G, signed with the
# release key.
tag_ours() {
  local s=$scratch/tag-signers
  [ "$(rg cat-file -t "refs/tags/$1" 2> /dev/null)" = tag ] || return 1
  [ "$(rg rev-parse -q --verify "refs/tags/$1^{commit}" 2> /dev/null)" = "$G" ] || return 1
  printf 'release namespaces="git" %s\n' "$(cut -d' ' -f1,2 "$RELEASE_PUB")" > "$s"
  rg -c gpg.format=ssh -c "gpg.ssh.allowedSignersFile=$s" verify-tag "$1" > /dev/null 2>&1
}
act_tag() {
  local name='' n t
  step_log tag
  [ "$TAG_STATE" = none ] || step_fail "$TAG on GitHub is in a state the tag step does not act on: $TAG_MSG" "run it again; if it says the same, send the log to the coordinator"
  t=$(tree_of "$G")
  # The confirmation is asked again in every run that did not get it itself (it lives only in this
  # process: a run that pushed the mirror earlier does not cover the tag now).
  case " $CONFIRMED " in
    *" tag "*) echo "the tag confirmation was given in this run" >> "$STEP_LOG" ;;
    *)
      set -- "step 5 tag: the tag $TAG on the release commit $(short "$G") (tree $(short "$t")), signed with the release key ($RELEASE_PUB), pushed to github.com/$repo; the tag ruleset forbids moving or deleting it, so a bad tag means a new version" \
        "it starts release.yml on the tag, which makes the draft release (step 6)"
      [ -z "$(st_get checks_waived)" ] || set -- "$@" "the red suite=all checks you waived: $(st_get checks_waived)"
      confirm tag "$@"
      ;;
  esac
  keys_ready ssh
  # The local name is always vV-gh (as in 0.1.0): vV in the clone belongs to Gitea's marker tag,
  # whether or not this clone has fetched it yet. A vV-gh an earlier run made (and did not get
  # pushed) is reused, so the tag object stays the same.
  name=$TAG-gh
  if rg rev-parse -q --verify "refs/tags/$name" > /dev/null 2>&1; then
    tag_ours "$name" || step_fail "the local tag $name exists and is not this release's (a signed tag of $(short "$G") by the release key)" \
      "look at it (git show $name); if it is a leftover: git tag -d $name, then run it again"
    echo "reusing the local tag $name (made by an earlier run): $(rg rev-parse "refs/tags/$name")" >> "$STEP_LOG"
  else
    log "signing the tag $TAG (local name $name) on $(short "$G") with the release key"
    LC_ALL=C rg -c gpg.format=ssh -c "user.signingkey=$RELEASE_PUB" tag -s -m "fil $V" "$name" "$G" >> "$STEP_LOG" 2>&1 \
      || step_fail "git tag -s could not make or sign the tag $name"
    echo "the tag: $name = $(rg rev-parse "refs/tags/$name") on $G" >> "$STEP_LOG"
  fi
  "$RELEASE_DIR/verify-tag.sh" "$ROOT" "$name" "refs/remotes/$github/main" >> "$STEP_LOG" 2>&1 \
    || step_fail "the tag $name does not verify against the signers on $github's main: it is not pushed" \
      "the release key ($RELEASE_PUB) must be a release signer on GitHub's main (.github/release-signers); git tag -d $name, then run it again"
  log "pushing $TAG to $github"
  LC_ALL=C rg push "$github" "refs/tags/$name:refs/tags/$TAG" >> "$STEP_LOG" 2>&1 || step_fail "git push $github $name:refs/tags/$TAG failed"
  st_set tag_object "$(rg rev-parse "refs/tags/$name")"
}

# The tag's run: TR_ID, the recorded one if it is still the tag push's release.yml run on G, else the
# newest such run GitHub lists (none: "").
TR_ID=''
tr_find() {
  local id
  TR_ID=''
  id=$(st_num tag_run)
  if [ -n "$id" ]; then
    rm -f "$scratch/run-$id"
    if run_view "$id"; then
      if rv_has "$id" "workflowName release" && rv_has "$id" "event push" && rv_has "$id" "headSha $G"; then TR_ID=$id; return 0; fi
      echo "the recorded tag run $id is not the tag push's release.yml run on $(short "$G"): forgotten" >> "$STEP_LOG"
    else
      grep -Eqi 'not found|HTTP 404|could not find' "$scratch/gh.err" 2> /dev/null || step_fail "cannot ask GitHub for run $id: $(gh_err)"
      echo "the recorded tag run $id is not on GitHub: forgotten" >> "$STEP_LOG"
    fi
    st_unset tag_run
  fi
  id=$(run_ids release.yml push "$G" "$TAG") || step_fail "cannot ask GitHub for the runs of release.yml: $(gh_err)"
  id=$(printf '%s\n' "$id" | awk 'NR == 1 { print $1 }')
  ! is_num "$id" || TR_ID=$id
}
# tr_delete IDS: deletes the releases IDS, each of which must still be a draft of the tag.
tr_delete() {
  local id rels
  rels=$(rel_list) || step_fail "cannot list the releases of $TAG: $(gh_err)"
  for id in $1; do
    printf '%s\n' "$rels" | awk -v i="$id" '$1 == i && $2 == "true" { f = 1 } END { exit !f }' \
      || step_fail "release $id is no longer a draft of $TAG: it is not deleted" "send the log to the coordinator"
    echo "gh api -X DELETE repos/$repo/releases/$id (a stale draft of $TAG)" >> "$STEP_LOG"
    "$GH" api -X DELETE "repos/$repo/releases/$id" < /dev/null >> "$STEP_LOG" 2>&1 || step_fail "gh could not delete the stale draft $id"
    log "deleted the stale draft $id of $TAG"
  done
}
act_tag_run() {
  local n=0 rerun=0 rels drafts x a0 keep names missing f t0 el stuck=0 failed_seen=0
  step_log tag-run
  while :; do
    tr_find
    [ -z "$TR_ID" ] || break
    n=$((n + 1))
    [ $n -le 20 ] || step_fail "no release.yml run of the tag push of $TAG on $(short "$G") appeared within 20 polls" \
      "look for it (gh run list --repo $repo --workflow release.yml --event push); if GitHub started none, the tag ruleset or the workflow's tag filter is to blame: send the log to the coordinator"
    sleep "$POLL"
  done
  st_set tag_run "$TR_ID"
  log "waiting for the tag's run $(run_url "$TR_ID") to publish the draft (about an hour; Ctrl-C is safe: run the same command again to re-attach)"
  n=0 t0=$(date +%s)
  while :; do
    rm -f "$scratch/run-$TR_ID"
    run_view "$TR_ID" || step_fail "cannot ask GitHub for run $TR_ID: $(gh_err)"
    rv_has "$TR_ID" "job success publish" && break
    # still going (a failed matrix leg leaves the run in progress while the others finish): wait;
    # a rerun is possible only once the run has completed
    if ! rv_has "$TR_ID" "status completed"; then
      if ! rv_no_failed_job "$TR_ID" && [ "$failed_seen" = 0 ]; then
        failed_seen=1
        echo "$(utc) a job of run $TR_ID failed: $(grep -E '^job (failure|cancelled|timed_out) ' "$scratch/run-$TR_ID" | cut -d' ' -f3- | tr '\n' ' ')" >> "$STEP_LOG"
        log "a job of the tag's run failed; waiting for the run to finish before offering a rerun"
      fi
      n=$((n + 1))
      if [ $((n % 10)) = 0 ]; then
        el=$(( ($(date +%s) - t0) / 60 ))
        x="still waiting for the publish job of run $(run_url "$TR_ID") ($(sed -n 's/^status //p' "$scratch/run-$TR_ID"), ${el} min)"
        echo "$(utc) $x" >> "$STEP_LOG"
        log "$x"
        if [ "$el" -ge 180 ] && [ "$stuck" = 0 ]; then
          stuck=1
          log "3 hours and the draft is not out: a job is probably stuck (queued with no runner, or hung); look at $(run_url "$TR_ID"). The wait goes on; Ctrl-C is safe"
        fi
      fi
      sleep "$POLL"
      continue
    fi
    # it failed: its log; a failed publish may have left a draft, which a rerun would double
    echo "== the failed jobs of the tag's run $TR_ID (gh run view --log-failed, the end):" >> "$STEP_LOG"
    "$GH" run view "$TR_ID" --repo "$repo" --log-failed < /dev/null 2>&1 | tail -n 30 >> "$STEP_LOG" || true
    rels=$(rel_list) || step_fail "cannot list the releases of $TAG: $(gh_err)"
    drafts=$(printf '%s\n' "$rels" | awk '$2 == "true" { printf "%s ", $1 }')
    if printf '%s\n' "$rels" | grep -q ' false '; then
      step_fail "the tag's run $(run_url "$TR_ID") failed, and a PUBLISHED release of $TAG exists" "send the log to the coordinator"
    fi
    if [ "$rerun" = 1 ] || ! is_tty; then
      step_fail "the tag's run $(run_url "$TR_ID") failed${drafts:+ (and left the draft(s) $drafts)}" \
        "if it is transient (a runner or the network): ${drafts:+delete the stale draft(s) first (gh api -X DELETE repos/$repo/releases/ID), then }gh run rerun $TR_ID --failed --repo $repo, then the same command; a real failure: the tag cannot move, so this version is burned: bump to the next patch version"
    fi
    echo "release: the tag's run $(run_url "$TR_ID") failed (the end of its log is in $STEP_LOG)" >&2
    tty_flush
    printf 'release: re-run its failed jobs%s? [y/N] ' "${drafts:+, deleting first the stale draft(s) ${drafts% } that the failed attempt left (else publish makes a second one)}" >&2
    x=''
    IFS= read -r x || x=''
    case $x in
      y | Y | yes) ;;
      *) step_fail "the tag's run $(run_url "$TR_ID") failed; not re-run" "a transient failure: run it again and answer y; a real one: the tag cannot move, so this version is burned: bump to the next patch version" ;;
    esac
    [ -z "$drafts" ] || tr_delete "$drafts"
    a0=$(rv_attempt "$TR_ID")
    echo "gh run rerun $TR_ID --failed (attempt ${a0:-?})" >> "$STEP_LOG"
    "$GH" run rerun "$TR_ID" --repo "$repo" --failed >> "$STEP_LOG" 2>&1 < /dev/null || step_fail "gh run rerun $TR_ID --failed failed"
    dr_rerun_started "$TR_ID" "${a0:-0}"
    rerun=1
  done
  # publish succeeded: its draft (GitHub may list it a moment later), and no other release of the tag
  n=0
  while :; do
    rels=$(rel_list) || step_fail "cannot list the releases of $TAG: $(gh_err)"
    [ -z "$rels" ] || break
    n=$((n + 1))
    [ $n -le 10 ] || step_fail "the publish job of run $(run_url "$TR_ID") succeeded, but GitHub lists no release of $TAG" "send the log to the coordinator"
    sleep "$POLL"
  done
  [ "$(printf '%s\n' "$rels" | grep -c .)" -gt 1 ] || return 0
  printf '%s\n' "$rels" | sed 's/^/release of the tag: /' >> "$STEP_LOG"
  if printf '%s\n' "$rels" | grep -q ' false '; then
    step_fail "$(printf '%s\n' "$rels" | grep -c .) releases of $TAG, one of them published" "send the log to the coordinator (a published release is never deleted)"
  fi
  # the double-draft trap: the newest draft is the last publish's; the older ones are stale
  drafts=$(printf '%s\n' "$rels" | sed '$d' | awk '{ printf "%s ", $1 }')
  keep=$(printf '%s\n' "$rels" | tail -n 1 | cut -d' ' -f1)
  # the one kept must be complete, or nothing is deleted
  names=$(gh_read api "repos/$repo/releases/$keep" --jq '.assets[].name') || step_fail "cannot list the assets of release $keep: $(gh_err)"
  missing=''
  for f in $(release_files) SHA256SUMS; do printf '%s\n' "$names" | grep -qx -- "$f" || missing="$missing $f"; done
  [ -z "$missing" ] || step_fail "$(printf '%s\n' "$rels" | grep -c .) drafts of $TAG, and the newest ($keep) lacks:$missing; nothing is deleted" \
    "look at the drafts (gh release list --repo $repo); send the log to the coordinator"
  LAST_DONE=$((NEXT - 1))
  echo "release: $(printf '%s\n' "$rels" | grep -c .) drafts of $TAG (the double-draft trap): keeping the newest, $keep, which has every release file" >&2
  is_tty || report PAUSED "$NEXT" "stale drafts of $TAG to delete (${drafts% }), and the input is not a terminal: nothing was done" "run it in a terminal (it asks before deleting), or delete them yourself: gh api -X DELETE repos/$repo/releases/ID"
  tty_flush
  printf 'release: delete the stale draft(s) %s? [y/N] ' "${drafts% }" >&2
  x=''
  IFS= read -r x || x=''
  case $x in y | Y | yes) ;; *) report PAUSED "$NEXT" "the stale draft(s) ${drafts% } not deleted: nothing was done" "run it again and answer y, or delete them yourself" ;; esac
  tr_delete "$drafts"
}

# run_step_cmd WHAT CMD...: CMD, with its output in the step's log and on stderr; a failure is the step's.
run_step_cmd() {
  local what=$1 rc=0
  shift
  echo "$what: $*" >> "$STEP_LOG"
  "$@" < /dev/null 2>&1 | tee -a "$STEP_LOG" >&2 || rc=$?
  [ "$rc" = 0 ] || step_fail "$what exited $rc"
}
act_verify_presign() {
  step_log verify-presign
  log "verifying the draft before signing: release-verify --pre-sign, with the rebuild in the VM (about 15 to 30 minutes)"
  run_step_cmd "release-verify $V --pre-sign" "$VERIFIER" "$V" --pre-sign --repo "$repo" ${vm:+--vm "$vm"}
}
act_sign_macos() {
  local -a a=()
  step_log sign-macos
  keys_ready ssh,apple
  # --redo sign-macos signs and notarizes again (the script wants --force for a signed release)
  ! in_redo sign-macos || a+=(--force)
  log "signing and notarizing the macOS binary (sign-macos.sh; Apple's verdict takes minutes)"
  run_step_cmd "sign-macos $V" env FIL_RELEASE_KEY="$RELEASE_PUB" "$SIGN_MACOS" "$V" --repo "$repo" ${a[@]+"${a[@]}"}
}
act_sign_packages() {
  step_log sign-packages
  keys_ready gpg
  log "signing the two rpms and SHA256SUMS (sign-packages.sh, with the package key in gpg-agent)"
  run_step_cmd "sign-packages $V" "$SIGN_PACKAGES" "$V" "$FILES"
}
act_sign_sums() {
  step_log sign-sums
  keys_ready ssh
  rm -f "$FILES/SHA256SUMS.sig"
  log "signing $FILES/SHA256SUMS with the release key"
  echo "ssh-keygen -Y sign -f $RELEASE_PUB -n fil-release $FILES/SHA256SUMS" >> "$STEP_LOG"
  LC_ALL=C ssh-keygen -Y sign -f "$RELEASE_PUB" -n fil-release "$FILES/SHA256SUMS" < /dev/null >> "$STEP_LOG" 2>&1 \
    || { rm -f "$FILES/SHA256SUMS.sig"; step_fail "ssh-keygen -Y sign could not sign SHA256SUMS"; }
  sig_verifies || step_fail "the new SHA256SUMS.sig does not verify against the signers on GitHub's main" "the release key ($RELEASE_PUB) must be a release signer there; send the log to the coordinator"
}
act_upload() {
  local f up=''
  step_log upload
  [ "$REL" = draft ] || step_fail "$TAG is not a draft on GitHub ($REL): nothing is uploaded" "send the log to the coordinator"
  # only what differs (a partial upload is finished, not repeated); all five on --redo
  for f in $RPMS SHA256SUMS SHA256SUMS.asc SHA256SUMS.sig; do
    if in_redo upload || [ "$(asset_digest "$f")" != "$(local_sha "$f")" ]; then up="$up $f"; fi
  done
  [ -n "$up" ] || return 0
  log "uploading to the draft $TAG:$up"
  echo "gh release upload $TAG --repo $repo --clobber$up (in $FILES)" >> "$STEP_LOG"
  # shellcheck disable=SC2086 # file names, no spaces
  (cd "$FILES" && LC_ALL=C "$GH" release upload "$TAG" --repo "$repo" --clobber $up) < /dev/null >> "$STEP_LOG" 2>&1 \
    || step_fail "gh release upload failed (the files are still in $FILES)"
}
act_verify_signed() {
  step_log verify-signed
  log "verifying the signed draft: release-verify (with the signature), reusing the rebuild of step verify-presign"
  run_step_cmd "release-verify $V --reuse-rebuild" "$VERIFIER" "$V" --repo "$repo" --reuse-rebuild ${vm:+--vm "$vm"}
}

# ---- the actions of steps 13-17.
# The publish confirmation: by default one answer, `publish`, covers publish, crates and brew; a run
# that did not get it (a resume) asks at the first of them it acts on, under that step's name, and
# the answer covers the steps after it. --confirm-each asks at each. Never stored, as the tag's.
pub_covered() {
  case " $CONFIRMED " in *" $1 "*) return 0 ;; esac
  [ "$confirm_each" = 0 ] || return 1
  case $1 in
    crates) case " $CONFIRMED " in *" publish "*) return 0 ;; esac ;;
    brew) case " $CONFIRMED " in *" publish "* | *" crates "*) return 0 ;; esac ;;
  esac
  return 1
}
pub_line() {
  local d
  case $1 in
    publish)
      d=$(asset_digest SHA256SUMS)
      echo "step 13 publish: the draft $TAG on github.com/$repo becomes the public release, with the files step 12 verified (SHA256SUMS $(short "$d")); immutable releases: they can never be changed or deleted"
      ;;
    crates) echo "step 15 crates: the approval of the crates-io deployment of the tag's run ${TAG_RUN:+$(run_url "$TAG_RUN")}, which publishes the crates at $V to crates.io, where a version can only be yanked, never replaced" ;;
    brew) echo "step 16 brew: $V's formula, committed (signed by you) on the main branch of the tap clone $tap and pushed to its origin; whoever runs brew upgrade gets it" ;;
  esac
}
# pub_confirm STEP: asks for STEP's confirmation unless this run's answers cover it.
pub_confirm() {
  local s on=0
  local -a lines=()
  if pub_covered "$1"; then
    echo "the confirmation for $1 was given in this run" >> "$STEP_LOG"
    return 0
  fi
  for s in publish crates brew; do
    [ "$s" != "$1" ] || on=1
    [ "$on" = 1 ] || continue
    if [ "$s" = "$1" ]; then
      lines+=("$(pub_line "$s")")
    elif [ "$confirm_each" = 0 ]; then
      lines+=("$(pub_line "$s") (this answer covers it too)")
    fi
  done
  if [ "$confirm_each" = 1 ]; then
    [ "$1" = brew ] || lines+=("(--confirm-each: the steps after it are asked for on their own)")
  elif [ "$1" = publish ]; then
    lines+=("between them: verify-published (release-verify of what is published), which stops the release on its own")
  fi
  confirm "$1" "${lines[@]}"
}

act_publish() {
  local p=$NEXT want names diff h n
  step_log publish
  [ "$REL" = draft ] || step_fail "$TAG is not a draft on GitHub ($REL): nothing is published" "send the log to the coordinator"
  # nothing from here on signs with the release key or the package key: forget them now, not after
  # the crates wait or a failure (a failure to forget is reported, and does not stop the release)
  echo "release-keys.sh forget --need ssh,gpg" >> "$STEP_LOG"
  "$KEYS" forget --need ssh,gpg < /dev/null >> "$STEP_LOG" 2>&1 \
    || log "WARNING: release-keys.sh forget failed (the log says why): forget them yourself, tests/release/just.sh release-keys forget --need ssh,gpg"
  pub_confirm publish
  # The answer may have taken hours: the draft is read again, every step must still hold, and the
  # draft must hold exactly the release's files, each the one step 12 verified (any writer can
  # replace a draft's assets).
  detect_all
  if [ "$NEXT" != "$p" ] || [ "$REL" != draft ]; then
    NOTE="step $(step_name "$NEXT") reads: ${NOTES[NEXT]}"
    NEXT=$p
    step_fail "the release changed while the confirmation waited ($NOTE): nothing is published" "run it again: it checks everything again; if it says the same, send the log to the coordinator"
  fi
  want=$({ release_files; printf '%s\n' SHA256SUMS SHA256SUMS.asc SHA256SUMS.sig; } | LC_ALL=C sort)
  names=$(cut -d' ' -f1 "$scratch/assets" | LC_ALL=C sort)
  if [ "$names" != "$want" ]; then
    diff=$(printf '%s\n' "$names" | LC_ALL=C comm -3 - <(printf '%s\n' "$want") | tr -d '\t' | tr '\n' ' ')
    step_fail "the draft's files are not exactly the release's (extra or missing: $diff): nothing is published" "look at the draft (gh release view $TAG --repo $repo); send the log to the coordinator"
  fi
  while read -r h n; do
    [ -n "$n" ] || continue
    [ "$(asset_digest "$n")" = "$h" ] || step_fail "the draft's $n is not the file SHA256SUMS lists ($(short "$h")): nothing is published" "send the log to the coordinator (the draft was changed after step 12 verified it)"
  done < "$FILES/SHA256SUMS"
  for n in SHA256SUMS SHA256SUMS.asc SHA256SUMS.sig; do
    [ "$(asset_digest "$n")" = "$(local_sha "$n")" ] || step_fail "the draft's $n is not the one in $FILES: nothing is published" "send the log to the coordinator"
  done
  echo "the draft holds exactly the verified files" >> "$STEP_LOG"
  log "publishing the release $TAG"
  echo "gh release edit $TAG --repo $repo --draft=false" >> "$STEP_LOG"
  LC_ALL=C "$GH" release edit "$TAG" --repo "$repo" --draft=false < /dev/null >> "$STEP_LOG" 2>&1 || step_fail "gh release edit $TAG --draft=false failed"
}

act_verify_published() {
  local d
  step_log verify-published
  d=$(asset_digest SHA256SUMS)
  [ -n "$d" ] || step_fail "cannot read the digest of the published SHA256SUMS: $(gh_err)"
  log "verifying the published release: release-verify --no-rebuild (the tag, the signatures, the attestations, release-check)"
  run_step_cmd "release-verify $V --no-rebuild" "$VERIFIER" "$V" --repo "$repo" --no-rebuild
  [ "$(local_sha SHA256SUMS)" = "$d" ] || step_fail "release-verify passed, but $FILES/SHA256SUMS is not the published one ($(short "$d"))" "send the log to the coordinator"
  st_set verify_published "ok $d $(utc)"
}

# crates_job ID: the conclusion of the run's crates-io job ("-" while it has none).
crates_job() { sed -n 's/^job \([^ ]*\) crates-io$/\1/p' "$scratch/run-$1" | head -n 1; }
# pending_crates ID: "ENVIRONMENT-ID CAN-APPROVE" of the run's pending crates-io deployment, nothing
# if there is none (not created yet, or already reviewed); 1 if GitHub could not be asked.
pending_crates() {
  local out
  out=$(gh_read api "repos/$repo/actions/runs/$1/pending_deployments" --jq '.[] | [.environment.id, .environment.name, .current_user_can_approve] | @tsv') || return 1
  printf '%s\n' "$out" | awk -F'\t' '$2 == "crates-io" && $1 ~ /^[0-9]+$/ { print $1, $3; exit }'
}
act_crates() {
  local id=$TAG_RUN n=0 t0 el x a a0 rerun=0 approved='' p env
  step_log crates
  [ "$REL" = published ] || step_fail "$TAG is not published ($REL): no crates-io deployment is approved" "send the log to the coordinator"
  [ -n "$id" ] || step_fail "no release.yml run of the tag push whose publish job succeeded: the crates-io job is that run's" "send the log to the coordinator"
  pub_confirm crates
  log "the crates-io job of the tag's run $(run_url "$id"): approving its deployment and waiting for it (Ctrl-C is safe: run the same command again to re-attach)"
  t0=$(date +%s)
  while :; do
    rm -f "$scratch/run-$id"
    run_view "$id" || step_fail "cannot ask GitHub for run $id: $(gh_err)"
    pred_tag_run "$id" || step_fail "run $id is no longer the tag push's release.yml run on $(short "$G") with a successful publish job" "send the log to the coordinator"
    x=$(crates_job "$id")
    [ "$x" != success ] || break
    if rv_has "$id" "status completed"; then
      # the crates-io job failed (the gate, crates.io, the network) or was rejected
      echo "== the failed jobs of the tag's run $id (gh run view --log-failed, the end):" >> "$STEP_LOG"
      "$GH" run view "$id" --repo "$repo" --log-failed < /dev/null 2>&1 | tail -n 30 >> "$STEP_LOG" || true
      if [ "$rerun" = 1 ] || ! is_tty; then
        step_fail "the crates-io job of $(run_url "$id") ended ${x:--} (the end of its log is above)" \
          "if it is transient (crates.io or the network): run it again at a terminal and answer y to the rerun (publish-crates.sh skips the crates already on crates.io); a gate refusal: send the log to the coordinator"
      fi
      echo "release: the crates-io job of $(run_url "$id") ended ${x:--} (the end of its log is in $STEP_LOG)" >&2
      tty_flush
      printf 'release: re-run it (publish-crates.sh skips the crates already on crates.io; its deployment is approved again)? [y/N] ' >&2
      x=''
      IFS= read -r x || x=''
      case $x in
        y | Y | yes) ;;
        *) step_fail "the crates-io job of $(run_url "$id") failed; not re-run" "a transient failure: run it again and answer y; anything else: send the log to the coordinator" ;;
      esac
      a0=$(rv_attempt "$id")
      echo "gh run rerun $id --failed (attempt ${a0:-?})" >> "$STEP_LOG"
      "$GH" run rerun "$id" --repo "$repo" --failed >> "$STEP_LOG" 2>&1 < /dev/null || step_fail "gh run rerun $id --failed failed"
      dr_rerun_started "$id" "${a0:-0}"
      rerun=1
      continue
    fi
    # the run goes on: its crates-io job waits for the review, or runs
    p=$(pending_crates "$id") || step_fail "cannot ask GitHub for the pending deployments of run $id: $(gh_err)"
    a=$(rv_attempt "$id")
    if [ -n "$p" ] && [ "$approved" != "${a:-0}" ]; then
      env=${p%% *}
      if [ "${p#* }" != true ]; then
        LAST_DONE=$((NEXT - 1))
        report PAUSED "$NEXT" "the crates-io deployment of $(run_url "$id") waits for a review the gh user may not give (GitHub: current_user_can_approve is false; \"Prevent self-review\" is on, or the user is not a required reviewer): nothing was approved" \
          "approve it on the run's page (Review deployments), or fix the crates-io environment's reviewers; then run it again: it waits for the job"
      fi
      echo "gh api -X POST repos/$repo/actions/runs/$id/pending_deployments -F environment_ids[]=$env -f state=approved (attempt ${a:-?})" >> "$STEP_LOG"
      LC_ALL=C "$GH" api -X POST "repos/$repo/actions/runs/$id/pending_deployments" -F "environment_ids[]=$env" -f state=approved \
        -f "comment=approved by just release $V, after its typed confirmation" < /dev/null >> "$STEP_LOG" 2>&1 \
        || step_fail "gh could not approve the crates-io deployment of run $id"
      approved=${a:-0}
      log "approved the crates-io deployment of $(run_url "$id")"
    fi
    n=$((n + 1))
    if [ $((n % 10)) = 0 ]; then
      el=$(( ($(date +%s) - t0) / 60 ))
      x="still waiting for the crates-io job of $(run_url "$id") ($(sed -n 's/^status //p' "$scratch/run-$id"), ${el} min)"
      echo "$(utc) $x" >> "$STEP_LOG"
      log "$x"
    fi
    sleep "$POLL"
  done
  echo "$(utc) the crates-io job of run $id succeeded" >> "$STEP_LOG"
  # crates.io's API can lag behind the job for a few minutes
  n=0
  while :; do
    NOTE='' UNKNOWN=0
    det_crates && break
    n=$((n + 1))
    [ $n -le 20 ] || step_fail "the crates-io job of $(run_url "$id") succeeded, but: $NOTE" "crates.io can lag a few minutes: run it again later; if a crate stays missing, send the log to the coordinator"
    [ $((n % 5)) != 1 ] || log "the crates-io job succeeded; waiting for crates.io ($NOTE)"
    sleep "$POLL"
  done
  echo "$NOTE" >> "$STEP_LOG"
}

tg() { LC_ALL=C git -C "$tap" "$@"; }
# tap_ours FORMULA BASE: the tap clone's HEAD is the commit an earlier run made for this release and
# did not push: "fil V", one parent, committed by you (git var GIT_COMMITTER_IDENT) and signed,
# changing only Formula/fil.rb to FORMULA, on a commit that BASE has.
tap_ours() {
  local me
  [ "$(tg log -1 --format=%s HEAD 2> /dev/null)" = "fil $V" ] || return 1
  [ "$(tg rev-list --parents -n 1 HEAD 2> /dev/null | wc -w | tr -d ' ')" = 2 ] || return 1
  me=$(tg var GIT_COMMITTER_IDENT 2> /dev/null | sed -n 's/.*<\(.*\)>.*/\1/p')
  [ -n "$me" ] && [ "$(tg log -1 --format=%ce HEAD)" = "$me" ] || return 1
  tg cat-file commit HEAD | grep -q '^gpgsig ' || return 1
  tg show HEAD:Formula/fil.rb 2> /dev/null | cmp -s - "$1" || return 1
  [ "$(tg diff --name-only HEAD^ HEAD 2> /dev/null)" = Formula/fil.rb ] || return 1
  tg merge-base --is-ancestor HEAD^ "$2" 2> /dev/null
}
# tap_formula_sha: the sha256 of Formula/fil.rb on the tap's origin main (nothing if none).
tap_formula_sha() {
  local main
  main=$(tg ls-remote origin refs/heads/main 2> /dev/null | awk '{ print $1 }') || return 0
  [ -n "$main" ] && tg show "$main:Formula/fil.rb" > "$scratch/tap-now.rb" 2> /dev/null || return 0
  sha256 "$scratch/tap-now.rb"
}
act_brew() {
  local f=$scratch/brew-act/fil.rb tmain head out bin x formula=${repo%%/*}/tap/fil reuse=0 remake=0
  step_log brew
  draft_sums || step_fail "cannot read the published SHA256SUMS: $(gh_err)"
  "$REPO_ROOT/packaging/render.sh" brew "$V" "$DRAFT_SUMS" "$scratch/brew-act" >> "$STEP_LOG" 2>&1 || step_fail "packaging/render.sh could not make the formula from the published SHA256SUMS"
  tg rev-parse --git-dir > /dev/null 2>&1 || step_fail "no tap clone at $tap" "clone the tap there, or name your clone with --tap DIR; then run it again"
  tg fetch -q --no-tags origin 2>> "$STEP_LOG" || step_fail "git -C $tap fetch origin failed"
  tmain=$(tg ls-remote origin refs/heads/main 2>> "$STEP_LOG" | awk '{ print $1 }') || tmain=''
  [ -n "$tmain" ] && tg cat-file -e "$tmain^{commit}" 2> /dev/null || step_fail "cannot read main of the tap's origin ($tap)" "git -C $tap fetch origin, then run it again"
  tap_formula_state "$tmain"
  if in_redo brew; then
    # --redo brew never commits or pushes: the install test again, on whatever the tap has
    echo "--redo brew: the install test only (the tap's formula is ${TAP_VER:-none}, $TAP_STATE); nothing is committed or pushed" >> "$STEP_LOG"
  else
    case $TAP_STATE in
      none | older) ;;
      done) step_fail "the tap's formula is $V's already, but the step did not detect as done" "run it again; if it says the same, send the log to the coordinator" ;;
      other) step_fail "the tap's formula names $V, but not with the published SHA256SUMS's checksums: it is not changed" "look at it (git -C $tap log -p origin/main -- Formula/fil.rb); fix the tap by hand (a PR there), then run it again" ;;
      newer) step_fail "the tap's formula is version $TAP_VER, newer than $V: it is not changed (no downgrade)" "nothing to do for the tap: it has moved past $V, and running it again stops here again. The release is out, but finish does not run without brew: no summary, no AUR PKGBUILD, no copy for the coordinator (the published SHA256SUMS and .sig are in $FILES)" ;;
      *) step_fail "the tap's formula names no version this command can read (${TAP_VER:--}): it is not changed" "look at it (git -C $tap show origin/main:Formula/fil.rb); fix the tap by hand, then run it again" ;;
    esac
    [ "$(tg symbolic-ref -q HEAD)" = refs/heads/main ] || step_fail "the tap clone $tap is not on its main branch" "git -C $tap checkout main, then run it again"
    [ -z "$(tg status --porcelain --untracked-files=no)" ] || step_fail "the tap clone $tap has uncommitted changes" "commit or discard them, then run it again"
    head=$(tg rev-parse HEAD)
    if tap_ours "$f" "$tmain"; then
      if [ "$(tg rev-parse HEAD^)" = "$tmain" ]; then reuse=1; else remake=1; fi
    else
      tg merge-base --is-ancestor "$head" "$tmain" || step_fail "the tap clone's main ($(short "$head")) has commits that are not on its origin's main" \
        "push or drop them (git -C $tap log origin/main..main), then run it again"
    fi
    pub_confirm brew
    if [ "$remake" = 1 ]; then
      # an earlier run's commit, on a main that has moved since: dropped (only it: tap_ours), and
      # made again on the new main
      echo "the tap's HEAD $(short "$head") is an earlier run's formula commit on an older main: made again on $(short "$tmain")" >> "$STEP_LOG"
      tg reset -q --hard HEAD^ >> "$STEP_LOG" 2>&1 || step_fail "git -C $tap reset HEAD^ failed"
    fi
    if [ "$reuse" = 1 ]; then
      echo "the tap's HEAD $(short "$head") is this release's formula commit, made by an earlier run and not pushed: it is pushed" >> "$STEP_LOG"
    else
      tg merge -q --ff-only "$tmain" >> "$STEP_LOG" 2>&1 || step_fail "git -C $tap merge --ff-only of the tap's main failed"
      mkdir -p "$tap/Formula"
      cp "$f" "$tap/Formula/fil.rb"
      tg add Formula/fil.rb
      tg commit -q -S -m "fil $V" -- Formula/fil.rb >> "$STEP_LOG" 2>&1 || step_fail "git commit -S in the tap could not make or sign the formula commit"
      tg cat-file commit HEAD | grep -q '^gpgsig ' || step_fail "git commit -S made an unsigned commit in the tap" "git -C $tap reset --hard $tmain; check git config gpg.format and user.signingkey; then run it again"
      echo "the formula commit: $(tg rev-parse HEAD) on $tmain" >> "$STEP_LOG"
    fi
    log "pushing $V's formula to the tap"
    tg push origin HEAD:refs/heads/main >> "$STEP_LOG" 2>&1 || step_fail "git push of the tap ($tap) failed"
  fi
  # the install test, on the owner's Homebrew
  log "the install test: brew update, then brew upgrade (or install) $formula, then fil --version"
  echo "brew update" >> "$STEP_LOG"
  "$BREW" update < /dev/null >> "$STEP_LOG" 2>&1 || step_fail "brew update failed" "the tap has $V's formula (the release is out); fix Homebrew, then $AGAIN --redo brew runs the install test again"
  if "$BREW" list --versions "$formula" > /dev/null 2>&1; then x=upgrade; else x=install; fi
  echo "brew $x $formula" >> "$STEP_LOG"
  "$BREW" "$x" "$formula" < /dev/null >> "$STEP_LOG" 2>&1 || step_fail "brew $x $formula failed" "the tap has $V's formula (the release is out): fix the formula (a PR to the tap), then $AGAIN --redo brew runs the install test again"
  bin=$("$BREW" --prefix 2>> "$STEP_LOG")/bin/fil
  out=$("$bin" --version 2>&1 < /dev/null | head -n 1) || out=''
  echo "$bin --version: $out" >> "$STEP_LOG"
  case $out in
    "fil $V (wire "*) log "Homebrew's fil says: $out" ;;
    *) step_fail "Homebrew's fil --version says '$out', not fil $V" "the tap has $V's formula (the release is out): look at brew info $formula; fix the formula (a PR to the tap), then $AGAIN --redo brew" ;;
  esac
  # the result, for finish's summary: bound to the formula that was tested
  x=$(tap_formula_sha)
  [ -z "$x" ] || st_set brew_tested "ok $x $(utc)"
}

act_finish() {
  local x d t aur=$ROOT/target/packaging/aur
  step_log finish
  # first: the keys are forgotten now, whatever fails below
  "$KEYS" forget --need ssh,gpg < /dev/null >> "$STEP_LOG" 2>&1 || step_fail "release-keys.sh forget failed"
  draft_sums || step_fail "cannot read the published SHA256SUMS: $(gh_err)"
  "$REPO_ROOT/packaging/render.sh" aur "$V" "$DRAFT_SUMS" "$aur" >> "$STEP_LOG" 2>&1 || step_fail "packaging/render.sh could not make the AUR PKGBUILD"
  # the copy for the coordinator: the published SHA256SUMS and its signature
  for x in SHA256SUMS SHA256SUMS.sig; do
    [ "$(local_sha "$x")" = "$(asset_digest "$x")" ] || step_fail "$FILES/$x is not the published one" "$AGAIN --redo verify-published (it downloads the published files again)"
  done
  if [ -d "$(dirname "$SHARED")" ]; then
    # /Users/Shared is writable by every local user and the name is predictable: the directory must
    # be ours (not a symlink, not another user's), and links planted in it are removed, not followed
    if [ -L "$SHARED" ] || { [ -e "$SHARED" ] && ! { [ -d "$SHARED" ] && [ -O "$SHARED" ]; }; }; then
      step_fail "$SHARED exists and is not a directory of yours (a symlink, or another user's): nothing is copied" "remove it (sudo if needed), then run it again"
    fi
    [ -d "$SHARED" ] || mkdir -m 755 "$SHARED" 2>> "$STEP_LOG" || step_fail "cannot make $SHARED"
    { [ -O "$SHARED" ] && [ ! -L "$SHARED" ]; } || step_fail "$SHARED is not a directory of yours" "remove it (sudo if needed), then run it again"
    rm -f "$SHARED/SHA256SUMS" "$SHARED/SHA256SUMS.sig"
    cp "$FILES/SHA256SUMS" "$FILES/SHA256SUMS.sig" "$SHARED/" 2>> "$STEP_LOG" && chmod 644 "$SHARED/SHA256SUMS" "$SHARED/SHA256SUMS.sig" \
      || step_fail "cannot copy SHA256SUMS and SHA256SUMS.sig to $SHARED"
  else
    log "note: no $(dirname "$SHARED") on this machine: the copy for the coordinator is skipped (the files are in $FILES)"
  fi
  d=$(asset_digest SHA256SUMS)
  t=$(st_get brew_tested)
  x=$(tap_formula_sha)
  case $t in
    "ok $x "*) [ -n "$x" ] && t="passed (${t##* })" || t='' ;;
    *) t='' ;;
  esac
  [ -n "$t" ] || t="NOT passed for the tap's formula: $AGAIN --redo brew"
  {
    echo "release: fil $V is released"
    echo "  release commit   $R"
    [ "$G" = "$R" ] || echo "  tagged commit    $G"
    echo "  tag              $TAG (tag object $(st_get tag_object))"
    [ -z "$(st_num dry_run)" ] || echo "  dry run          $(run_url "$(st_num dry_run)")"
    [ -z "$(st_num tag_run)" ] || echo "  tag's run        $(run_url "$(st_num tag_run)")"
    echo "  release          https://github.com/$repo/releases/tag/$TAG"
    echo "  SHA256SUMS       sha256 $d"
    echo "  crates.io        ${NOTES[$(step_no crates)]}"
    echo "  Homebrew         ${NOTES[$(step_no brew)]}"
    echo "  install test     $t"
    echo "  AUR              $aur/PKGBUILD (rendered, not published)"
    [ ! -d "$SHARED" ] || echo "  for the coordinator: $SHARED/SHA256SUMS and SHA256SUMS.sig"
    echo "  keys             forgotten (tests/release/just.sh release-keys status)"
  } | tee -a "$STEP_LOG"
}

# ---- run
if [ "$mode" = status ]; then
  if git -C "$tap" rev-parse --git-dir > /dev/null 2>&1; then
    git -C "$tap" fetch -q --no-tags origin 2> /dev/null || true
  fi
  PF_CARGO_FETCH=0
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
# Self-test only (refused outside it, as every override): hold the lock while this file exists
# (at most 30 s), so a test can prove that other runs find it held.
if [ -n "${FIL_RELEASE_LOCK_HOLD:-}" ]; then
  n=0
  while [ -e "$FIL_RELEASE_LOCK_HOLD" ] && [ $n -lt 300 ]; do n=$((n + 1)); sleep 0.1; done
fi
rg fetch -q --no-tags "$origin" 2> "$scratch/git.err" || report FAILED 0 "git fetch $origin failed: $(head -n 1 "$scratch/git.err" | cut -c1-200)" "check the remote (git remote -v; git fetch $origin)"
rg fetch -q --no-tags "$github" 2> "$scratch/git.err" || report FAILED 0 "git fetch $github failed: $(head -n 1 "$scratch/git.err" | cut -c1-200)" "check the remote (git remote -v; git fetch $github)"
FETCHED=1
if git -C "$tap" rev-parse --git-dir > /dev/null 2>&1; then
  git -C "$tap" fetch -q --no-tags origin 2> /dev/null || log "note: could not fetch the tap ($tap); step brew reads it as it is"
fi

# tag_known: whether vV exists anywhere that binds the release commit: in the clone (vV, Gitea's
# marker, or vV-gh, the local name of the tag this command makes) or on GitHub. 0: it does
# (TAG_WHERE says where: GitHub first; TAG_LOCAL names a tag only the clone has); 1: it does not;
# 2: GitHub could not be asked, and the clone has none.
TAG_WHERE='' TAG_LOCAL=''
tag_known() {
  local n t gh=1
  TAG_WHERE='' TAG_LOCAL=''
  if t=$(rg ls-remote "$github" "refs/tags/$TAG" 2> /dev/null); then
    [ -z "$t" ] || { TAG_WHERE="on $github"; return 0; }
  else
    gh=2
  fi
  for n in "$TAG" "$TAG-gh"; do
    if rg rev-parse -q --verify "refs/tags/$n" > /dev/null 2>&1; then
      TAG_WHERE="in the clone (refs/tags/$n)" TAG_LOCAL=$n
      [ "$gh" = 2 ] || TAG_WHERE="$TAG_WHERE, not on $github"
      return 0
    fi
  done
  return "$gh"
}

# The clone behind Gitea's main: a clean main that Gitea's main fast-forwards (a fix merged there
# after an earlier run) is fast-forwarded to it, before the tag only (once vV exists, the release
# commit is the tagged one). Anything else (a dirty tree, a detached HEAD or another branch, a main
# with commits Gitea's lacks) is left as it is, for the preflight's tree check to stop with its advice.
ff_main() {
  local head main rc=0
  [ -z "$(rg status --porcelain --untracked-files=no 2> /dev/null)" ] || return 0
  [ "$(rg symbolic-ref -q HEAD 2> /dev/null || true)" = refs/heads/main ] || return 0
  head=$(rg rev-parse -q --verify 'HEAD^{commit}' 2> /dev/null) || return 0
  main=$(rg rev-parse -q --verify "refs/remotes/$origin/main^{commit}" 2> /dev/null) || return 0
  [ "$head" != "$main" ] || return 0
  rg merge-base --is-ancestor "$head" "$main" || return 0
  # a version Gitea's main is not at (mistyped, or a later one) changes nothing
  if [ "$(pf_cargo_version "$main")" != "$V" ]; then
    log "note: main ($(short "$head")) is behind $origin's main ($(short "$main")), whose Cargo.toml is not at $V: not fast-forwarded"
    return 0
  fi
  tag_known || rc=$?
  case $rc in
    0) log "note: main ($(short "$head")) is behind $origin's main ($(short "$main")), and $TAG exists $TAG_WHERE: not fast-forwarded (the tag binds its commit)"; return 0 ;;
    2) log "note: main ($(short "$head")) is behind $origin's main ($(short "$main")), and '$github' cannot be asked whether $TAG exists: not fast-forwarded"; return 0 ;;
  esac
  LC_ALL=C rg merge -q --ff-only "refs/remotes/$origin/main" > "$scratch/ff.log" 2>&1 \
    || report FAILED 0 "main ($(short "$head")) is behind $origin's main ($(short "$main")), and git merge --ff-only $origin/main failed: $(head -n 1 "$scratch/ff.log" | cut -c1-200)" \
      "git status; then git merge --ff-only $origin/main by hand"
  log "main was behind $origin's main: fast-forwarded from $(short "$head") to $(short "$main") (git merge --ff-only $origin/main; new commits: $(rg rev-list --count "$head..$main"))"
  r_changed "$head" "$main" ''
}
# r_changed FROM TO [DROPPED]: the line the first confirmation of this run shows when the run moved
# the release commit (the fast-forward, the rebind), with full commits.
r_changed() {
  local n
  if rg merge-base --is-ancestor "$1" "$2" 2> /dev/null; then
    n=$(rg rev-list --count "$1..$2")
    if [ "$n" = 1 ]; then n="1 new commit"; else n="$n new commits"; fi
  else
    n="not a descendant of it"
  fi
  R_CHANGED="the release commit changed in this run: $1 -> $2 ($n${3:+; $3})"
}
ff_main
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
# A state bound to another commit is rebound to HEAD (as --reset-state) when HEAD is Gitea's main
# and no vV exists yet; once vV exists (in the clone or on GitHub), or GitHub cannot say, or HEAD is
# not Gitea's main (a checkout by mistake would lose the records), it is a stop.
if [ -n "$sr" ] && [ "$sr" != "$R" ]; then
  rc=0
  tag_known || rc=$?
  if [ "$rc" = 1 ] && [ "$R" != "$(rg rev-parse -q --verify "refs/remotes/$origin/main^{commit}" 2> /dev/null || true)" ]; then
    report FAILED 0 "the state is for $V at $(short "$sr"), HEAD is $(short "$R"), which is not $origin's main; no $TAG yet" \
      "git checkout $sr; or, for a new release commit: check out $origin's main (git checkout main && git merge --ff-only $origin/main), then run it again (it rebinds)"
  fi
  case $rc in
    1)
      rm -f "$STATE"
      log "the state was bound to $V at $(short "$sr"), HEAD is $(short "$R"), and there is no $TAG yet (in the clone or on $github): the state is rebound to $(short "$R") (as --reset-state would)"
      r_changed "$sr" "$R" "the earlier records, any waiver included, were dropped"
      sr=''
      ;;
    2)
      report FAILED 0 "the state is for $V at $(short "$sr"), HEAD is $(short "$R"), and '$github' cannot be asked whether $TAG exists (the tag would bind its commit): nothing is rebound" \
        "check the remote (git ls-remote $github refs/tags/$TAG), then run it again; or git checkout $sr"
      ;;
    *)
      case $TAG_WHERE in
        "on "*) report FAILED 0 "the state is for $V at $(short "$sr"), HEAD is $(short "$R"), and $TAG is $TAG_WHERE: the tag binds its commit, and a new commit after it is a new version" \
          "git checkout $sr" ;;
        *) report FAILED 0 "the state is for $V at $(short "$sr"), HEAD is $(short "$R"), and $TAG exists $TAG_WHERE: it may be a tag an earlier run made and did not push" \
          "git checkout $sr; or, if it was never pushed and the new commit is the one to release: git tag -d $TAG_LOCAL, then run it again (it rebinds)" ;;
      esac
      ;;
  esac
fi

# record_found: the identifiers the detections found, into the state. github_commit (the commit on
# GitHub's main the release is, G) is a record for whoever reads the state: no detection reads it.
record_found() {
  [ -z "$G" ] || st_set github_commit "$G"
  [ -z "$DRY_RUN" ] || st_set dry_run "$DRY_RUN"
  [ -z "$CHECKS_LINUX" ] || st_set checks_linux "$CHECKS_LINUX"
  [ -z "$CHECKS_MACOS" ] || st_set checks_macos "$CHECKS_MACOS"
  [ "$TAG_OK" = 0 ] || st_set tag_object "$TAG_OBJ"
  [ -z "$TAG_RUN" ] || st_set tag_run "$TAG_RUN"
}
# main_stop: GitHub's main is past R and not to be trusted (main_trust): nothing that reads the
# signers on main may go on (from the tag on; before it, step mirror refuses it). Before hard_stop: a
# tag that fails against an untrusted signers file is not burned.
main_stop() {
  [ -n "$MAIN_BAD" ] && [ "$NEXT" -gt "$(step_no mirror)" ] || return 0
  LAST_DONE=$((NEXT - 1))
  report FAILED "$NEXT" "$MAIN_BAD" "find out who pushed GitHub's main (git fetch $github; git log $R..$github/main); a commit only GitHub has must reach Gitea's main first, and a change of the signers needs a new release commit; then run it again"
}
hard_stop() {
  [ -n "$HARD_STEP" ] || return 0
  LAST_DONE=$((HARD_STEP - 1))
  report FAILED "$HARD_STEP" "$HARD" "this version is burned: bump to the next patch version (a PR on Gitea: just set-version), and release that"
}

# The window for runs found by listing opens when the state is bound (run_ours): now, for a new or
# reset state, so no run from before this release's first run counts. Written to the state below,
# once the detection has not refused the checkout.
BOUND_AT=$(st_num bound_at)
[ -n "$BOUND_AT" ] || BOUND_AT=$(date -u +%s)
detect_all
if [ "$TAG_STATE" = mismatch ]; then
  LAST_DONE=0
  report FAILED "$(step_no tag)" "$TAG_MSG (nothing is recorded)" "$TAG_FIX, then run it again"
fi
st_set version "$V"
st_set release_commit "$R"
st_set bound_at "$BOUND_AT"
record_found
main_stop
hard_stop
# --from STEP: every step before it must be done (by the command or by hand).
if [ -n "$from" ]; then
  fn=$(step_no "$from")
  for ((i = 1; i < fn; i++)); do
    case ${KIND[i]} in
      checked | recorded | waived | implied) ;;
      *)
        LAST_DONE=$((i - 1))
        report FAILED "$i" "--from $from: step $i $(step_name $i) is not done (${NOTES[i]})" "do it first: run the command without --from (it does every step), or do it by hand, then run it again"
        ;;
    esac
  done
fi
# --redo STEP: only a step that is done, and only while its redo can change anything.
if [ -n "$redo" ]; then
  rn=$(step_no "$redo")
  # done: detected as done, or implied by a later step that is
  rdone=0
  for ((i = rn; i <= NSTEPS; i++)); do [ "${WAS_DONE[i]}" = 0 ] || rdone=1; done
  if [ "$rdone" = 0 ]; then
    LAST_DONE=0
    report FAILED "$rn" "--redo $redo: the step is not done (${NOTES[rn]}): there is nothing to redo" "run it without --redo"
  fi
  case $redo in
    dry-run | dry-verify) [ "$TAG_OK" = 0 ] || report FAILED "$rn" "--redo $redo: $TAG is on GitHub; the dry run decides nothing any more" "nothing to do: the steps after the tag check the release itself" ;;
    verify-published | brew) [ "$REL" = published ] || report FAILED "$rn" "--redo $redo: $TAG is not published ($REL)" "run it without --redo" ;;
    *) [ "$REL" = draft ] || report FAILED "$rn" "--redo $redo: $TAG is not a draft ($REL); a published release is not changed" "nothing to do" ;;
  esac
  # a pre-sign verification judges the unsigned rpms CI built: once the draft carries the package
  # signatures it can only fail, which would read as a reason to burn a good version
  if [ "$redo" = verify-presign ] && { has_asset SHA256SUMS.asc || [ "${WAS_DONE[$(step_no upload)]}" = 1 ]; }; then
    report FAILED "$rn" "--redo verify-presign: the draft is signed already (it has the signed rpms and SHA256SUMS.asc); a pre-sign verification is only meaningful before the upload" \
      "$AGAIN --redo verify-signed re-verifies the signed draft"
  fi
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

# The keys the remaining steps sign with, in one block at the start (a terminal only: without one,
# each step that needs a key pauses there if it is locked); each step checks again before it signs.
if is_tty && [ "$NEXT" -le "$(step_no verify-signed)" ]; then keys_ready "$(keys_needed)"; fi

# The actions, step by step through finish; after each, every step is detected again.
while :; do
  if [ "$NEXT" = "$NSTEPS" ]; then
    act_finish
    exit 0
  fi
  name=$(step_name "$NEXT")
  if [ "${KIND[NEXT]}" = unknown ]; then
    report FAILED "$NEXT" "cannot tell whether $name is done: ${NOTES[NEXT]}" "fix that, then run it again"
  fi
  prev=$NEXT
  "act_$(printf '%s' "$name" | tr - _)"
  # this step is redone: it reads as it really is from now on
  REDO_LEFT=$(printf '%s\n' $REDO_LEFT | grep -vx -- "$name" | tr '\n' ' ' || true)
  detect_all
  main_stop
  hard_stop
  record_found
  if [ "$NEXT" -lt "$prev" ]; then
    # an earlier step, done before this action, no longer detects as done: the world moved under the
    # run (GitHub's main pushed, a run or release changed); say which step, not this one
    rn=$NEXT
    NEXT=$prev
    step_fail "after $name, step $rn $(step_name "$rn"), done before, no longer detects as done: ${NOTES[rn]}" \
      "if Gitea's or GitHub's main moved during the run (a merge, the agents' push of main): run it again, it fast-forwards, rebinds and checks again from the new main; otherwise send the log to the coordinator"
  elif [ "$NEXT" = "$prev" ]; then
    step_fail "the action of $name finished, but the step does not detect as done: ${NOTES[prev]}" "send the log to the coordinator (a bug in the command or a change in GitHub's answers)"
  fi
done
# (never reached: the loop exits. This `exit` keeps bash from reading past the group.)
exit
}
