#!/usr/bin/env bash
# `just reserve-crate-names`: reserve the crates.io names of the nine published crates with a 0.0.1
# placeholder each (docs/m4/release.md section 10; docs/m4/plan.md OQ-3, decided by the owner
# 2026-11-04). Run by whoever holds the publishing crates.io account, after `cargo login`. No
# crates.io token goes to CI, and nothing here publishes the real workspace crates: each placeholder is
# built in a fresh temporary directory outside the repository, from text in this file.
#
#   reserve-crate-names.sh [--dry-run | --publish] [--account LOGIN] [--invite OWNER] [--yes]
#                          [--skip-login-check]
#
#   --dry-run (the default)  `cargo publish --dry-run` for each crate; nothing is uploaded. The
#                            crates.io API is read (GET) to see which names are taken.
#   --publish                really publish. Asks for a "yes" first unless --yes is given.
#   --account LOGIN          the crates.io login of the publishing account. With it, a name that is
#                            taken and whose owners (the public owners list) do not include LOGIN is
#                            reported as "taken by others", with its owners, and the run exits 1.
#   --invite OWNER           after each publish, `cargo owner --add OWNER CRATE` (an invitation OWNER
#                            accepts on crates.io). Skipped for a crate OWNER already owns or has been
#                            invited to, so a rerun also invites to the crates published before, and
#                            for a crate this account does not own (known with --account; without it
#                            the owners-only error of crates.io is reported and left alone). With
#                            --dry-run it only says what it would do.
#   --skip-login-check       for a cargo credential provider (a keychain, for example) that keeps
#                            the token somewhere this script cannot see.
#
# A name that this account already owns is skipped with a message, never a failure, so a rerun after
# an interruption (or a rate-limit stop) carries on with the names that are left. Without --account,
# any taken name counts as ours.
#
# Pacing. crates.io limits NEW crates per account: a burst of 5, then one more every 10 minutes
# (crates.io source, src/rate_limiter.rs, LimitedAction::PublishNew: default_rate_seconds = 10 * 60,
# default_burst = 5; the server's error text is "You have published too many new crates in a short
# period of time", HTTP 429). Nine names therefore take about 40 minutes: the first five go at once, and
# the script waits PACE_SECS before each one after that, and, if the server still answers 429 (the
# bucket was not full), waits and retries instead of failing. Overrides for the waits, in seconds:
# FIL_RESERVE_PACE_SECS (default 600) and FIL_RESERVE_RETRY_SECS (default 600); the wait after a 429
# from the crates.io API (the name check) is FIL_RESERVE_API_WAIT_SECS (default 60).
#
# Written for bash 3.2 (the Mac's /bin/bash) and GNU bash.
set -euo pipefail

# The nine crates that are published (docs/m4/plan.md OQ-3): the user-facing crate fil-shell (the name
# `fil` was taken on crates.io on 2026-11-04) and the eight library crates. The test-only crates
# (fil-testkit, fil-testbins, fil-netsim) stay unpublished and are not listed.
CRATES="fil-shell fil-proto fil-transport fil-session fil-daemon fil-client fil-term fil-predict fil-forward"
PLACEHOLDER_VERSION=0.0.1
REPO_URL=https://github.com/anthonytw/fil
USER_AGENT="fil-reserve-crate-names (+$REPO_URL)"
BURST=5
PACE_SECS=${FIL_RESERVE_PACE_SECS:-600}
RETRY_SECS=${FIL_RESERVE_RETRY_SECS:-600}
API_WAIT_SECS=${FIL_RESERVE_API_WAIT_SECS:-60}
MAX_RETRIES=${FIL_RESERVE_MAX_RETRIES:-12}
CARGO=${FIL_RESERVE_CARGO:-cargo}
CURL=${FIL_RESERVE_CURL:-curl}
# Set by main: the scratch directory, the mode (dry-run or publish), the --account and --invite logins,
# the summary lists.
work="" mode=dry-run account="" invite="" done_list="" skipped_list="" failed_list="" invited_list="" others_list=""

say() { echo "reserve-crate-names: $*" >&2; }
die() {
  say "$*"
  exit 1
}

# run_retry_429 LOGFILE CMD...: run CMD with its output in LOGFILE. If it fails with crates.io's
# "too many requests" answer (HTTP 429, or the new-crate message), wait RETRY_SECS and run it again,
# up to MAX_RETRIES times. Any other failure, or the last 429, is returned as the command's status.
run_retry_429() {
  local log=$1 tries=0 rc
  shift
  while :; do
    rc=0
    "$@" > "$log" 2>&1 || rc=$?
    if [ "$rc" = 0 ]; then return 0; fi
    if ! grep -Eqi 'status 429|429 Too Many Requests|too many new crates' "$log"; then return "$rc"; fi
    tries=$((tries + 1))
    if [ "$tries" -gt "$MAX_RETRIES" ]; then return "$rc"; fi
    say "  crates.io rate limit (HTTP 429); waiting ${RETRY_SECS}s, then retrying ($tries of $MAX_RETRIES)"
    sleep "$RETRY_SECS"
  done
}

# name_status NAME: prints "taken", "free" or "unknown:CODE". GET /api/v1/crates/NAME: 200 means the
# name exists (owned by someone: us on a rerun, or another account), 404 means it is free. A 429 is
# waited out like the publish one.
name_status() {
  local name=$1 code tries=0
  while :; do
    code=$("$CURL" -s --max-time 30 -o /dev/null -w '%{http_code}' -A "$USER_AGENT" "https://crates.io/api/v1/crates/$name" || true)
    case $code in
      200) echo taken; return 0 ;;
      404) echo free; return 0 ;;
      429)
        tries=$((tries + 1))
        if [ "$tries" -gt "$MAX_RETRIES" ]; then echo "unknown:$code"; return 0; fi
        say "  crates.io API rate limit (HTTP 429); waiting ${API_WAIT_SECS}s"
        sleep "$API_WAIT_SECS"
        ;;
      *) echo "unknown:${code:-none}"; return 0 ;;
    esac
  done
}

# owners_of NAME: the logins on the public owners list of the crate NAME, space-separated (empty if
# the list cannot be read). Invited owners who have not accepted yet are not on it.
owners_of() {
  { "$CURL" -s --max-time 30 -A "$USER_AGENT" "https://crates.io/api/v1/crates/$1/owners" || true; } |
    { grep -o '"login":"[^"]*"' || true; } | sed 's/^"login":"\(.*\)"$/\1/' | tr '\n' ' '
}

# invite_owner OWNER CRATE: `cargo owner --add`, unless OWNER already owns CRATE (the public owners
# list). crates.io answers a second invitation with a success whose message says the invitation is
# pending ("already has a pending invitation"), which counts as "already". Prints one of "invited",
# "already", "notours" (crates.io says only owners can change the owners: this account is not one) or
# "failed" (the output is in $work/CRATE.owner.log).
invite_owner() {
  local owner=$1 name=$2 log=$work/$2.owner.log
  case " $(owners_of "$name") " in *" $owner "*) echo already; return 0 ;; esac
  if (cd "$work" && "$CARGO" owner --add "$owner" "$name") > "$log" 2>&1; then
    if grep -Fqi 'already has a pending invitation' "$log"; then echo already; else echo invited; fi
  elif grep -Eqi "only owners have permission to modify owners|team members don't have permission" "$log"; then
    echo notours
  else
    echo failed
  fi
}

# handle_taken NAME: a name crates.io already has. Ours (this account is on its owners list, or no
# --account was given): skipped, and invited if --invite. Someone else's: listed with its owners, no
# invitation, and the run will exit 1.
handle_taken() {
  local name=$1 owners
  if [ -n "$account" ]; then
    owners=$(owners_of "$name")
    if [ -z "$owners" ]; then
      say "  taken, but its owners could not be read; not attempted"
      failed_list="$failed_list $name"
      return 0
    fi
    case " $owners " in
      *" $account "*) ;;
      *)
        say "  taken by others: ${owners% } (see https://crates.io/crates/$name); not ours, no invitation"
        others_list="$others_list $name(${owners% })"
        return 0
        ;;
    esac
  fi
  say "  already registered on crates.io (ours, or ownership unchecked: see https://crates.io/crates/$name); skipping"
  skipped_list="$skipped_list $name"
  do_invite "$name"
}

# do_invite NAME: the --invite step for one crate (a no-op without --invite).
do_invite() {
  local name=$1 r
  [ -n "$invite" ] || return 0
  if [ "$mode" = dry-run ]; then
    say "  would try: cargo owner --add $invite $name"
    return 0
  fi
  r=$(invite_owner "$invite" "$name")
  case $r in
    invited)
      say "  invited $invite as an owner of $name"
      invited_list="$invited_list $name"
      ;;
    already) say "  $invite already owns $name or has been invited" ;;
    notours) say "  crates.io says this account is not an owner of $name; not inviting" ;;
    *)
      say "  inviting $invite to $name FAILED:"
      sed 's/^/    | /' "$work/$name.owner.log" >&2
      failed_list="$failed_list $name(invite)"
      ;;
  esac
}

# logged_in: a token where cargo looks first: CARGO_REGISTRY_TOKEN, or `cargo login`'s file.
logged_in() {
  local home=${CARGO_HOME:-$HOME/.cargo} f
  if [ -n "${CARGO_REGISTRY_TOKEN:-}" ]; then return 0; fi
  for f in "$home/credentials.toml" "$home/credentials"; do
    if [ -f "$f" ] && grep -Eq '^[[:space:]]*token[[:space:]]*=' "$f"; then return 0; fi
  done
  return 1
}

# write_placeholder DIR NAME: the whole placeholder crate. Only a name, a version, the license, the
# repository and a README: no author, no email, nothing from the workspace.
write_placeholder() {
  local dir=$1 name=$2
  mkdir -p "$dir/src"
  cat > "$dir/Cargo.toml" << EOF
[package]
name = "$name"
version = "$PLACEHOLDER_VERSION"
edition = "2021"
description = "Placeholder for the fil remote shell; first release coming soon"
license = "GPL-3.0-or-later"
repository = "$REPO_URL"
readme = "README.md"

# Not part of any workspace that happens to contain the scratch directory.
[workspace]
EOF
  cat > "$dir/src/lib.rs" << EOF
//! Placeholder for the \`$name\` crate of the fil remote shell. It reserves the name; the first
//! release is coming. See <$REPO_URL>.
EOF
  cat > "$dir/README.md" << EOF
# $name

This crate reserves the name \`$name\` for the [fil project]($REPO_URL), a remote shell that is in
active development. It contains no code. The first real release is coming; see the repository for
the current state.
EOF
}

main() {
  local yes=0 skip_login=0 arg
  while [ $# -gt 0 ]; do
    arg=$1
    shift
    case $arg in
      --dry-run) mode=dry-run ;;
      --publish) mode=publish ;;
      --yes) yes=1 ;;
      --account)
        [ $# -gt 0 ] || { say "--account needs a crates.io login"; exit 2; }
        account=$1
        shift
        case $account in
          *[!A-Za-z0-9_-]* | "") say "--account: not a crates.io login: $account"; exit 2 ;;
        esac
        ;;
      --invite)
        [ $# -gt 0 ] || { say "--invite needs a crates.io login"; exit 2; }
        invite=$1
        shift
        case $invite in
          *[!A-Za-z0-9_-]* | "") say "--invite: not a crates.io login: $invite"; exit 2 ;;
        esac
        ;;
      --skip-login-check) skip_login=1 ;;
      -h | --help)
        sed -n '2,/^set -e/p' "${BASH_SOURCE[0]}" | sed -n '/^#/s/^# \{0,1\}//p' | sed -n '1,/^Written for/p' | sed '$d' >&2
        exit 0
        ;;
      *) say "unknown argument: $arg (try --help)"; exit 2 ;;
    esac
  done

  command -v "$CARGO" > /dev/null 2>&1 || die "cargo is not on the PATH"
  command -v "$CURL" > /dev/null 2>&1 || die "curl is not on the PATH"

  if [ "$mode" = publish ]; then
    if [ "$skip_login" = 0 ] && ! logged_in; then
      die "no crates.io token found. Run \`cargo login\` once (a token from https://crates.io/settings/tokens with the publish-new and change-owners scopes), then rerun. (--skip-login-check if a credential provider keeps your token.)"
    fi
    if [ "$yes" = 0 ]; then
      say "about to PUBLISH $PLACEHOLDER_VERSION placeholders for: $CRATES"
      say "Publishing a name is permanent. Type yes to go on:"
      read -r answer
      [ "$answer" = yes ] || die "not confirmed; nothing was published"
    fi
  else
    say "dry run: nothing is uploaded (use --publish to reserve the names)"
    if ! logged_in; then say "(no crates.io token found; a dry run does not need one, --publish does: run \`cargo login\` first)"; fi
  fi

  work=$(mktemp -d "${TMPDIR:-/tmp}/fil-reserve.XXXXXX")
  [ -n "$work" ] && [ -d "$work" ] || die "could not make a scratch directory"
  # shellcheck disable=SC2064 # $work is fixed now
  trap "rm -rf '$work'" EXIT
  # Building outside the repository keeps the repo's workspace and toolchain file out of the way.
  cd "$work"

  local name st published=0 rc
  for name in $CRATES; do
    say "$name:"
    st=$(name_status "$name")
    case $st in
      taken)
        handle_taken "$name"
        continue
        ;;
      unknown:*)
        say "  could not tell whether the name is taken (HTTP ${st#unknown:}); not attempted"
        failed_list="$failed_list $name"
        continue
        ;;
    esac
    write_placeholder "$work/$name" "$name"
    if [ "$mode" = dry-run ]; then
      rc=0
      (cd "$work/$name" && "$CARGO" publish --dry-run --quiet > "$work/$name.log" 2>&1) || rc=$?
      # `cargo publish --dry-run` ends with "warning: aborting upload due to dry run".
      if [ "$rc" = 0 ]; then
        say "  dry run ok: $name $PLACEHOLDER_VERSION would be published"
        done_list="$done_list $name"
        do_invite "$name"
      else
        say "  dry run FAILED:"
        sed 's/^/    | /' "$work/$name.log" >&2
        failed_list="$failed_list $name"
      fi
      continue
    fi
    if [ "$published" -ge "$BURST" ]; then
      say "  the new-crate burst ($BURST) is used up; waiting ${PACE_SECS}s for the limit to refill"
      sleep "$PACE_SECS"
    fi
    rc=0
    (cd "$work/$name" && run_retry_429 "$work/$name.log" "$CARGO" publish --quiet) || rc=$?
    if [ "$rc" = 0 ]; then
      say "  published $name $PLACEHOLDER_VERSION"
      published=$((published + 1))
      done_list="$done_list $name"
      do_invite "$name"
    elif [ "$(name_status "$name")" = taken ]; then
      # Lost a race with an earlier, interrupted run that did upload it.
      say "  the name is now registered"
      handle_taken "$name"
    else
      say "  publish FAILED:"
      sed 's/^/    | /' "$work/$name.log" >&2
      failed_list="$failed_list $name"
    fi
  done

  say "summary ($mode):"
  say "  ${mode/dry-run/dry-run ok}:${done_list:- (none)}"
  say "  already registered, skipped:${skipped_list:- (none)}"
  say "  taken by others:${others_list:- (none)}"
  if [ -n "$invite" ]; then say "  invited $invite:${invited_list:- (none)}"; fi
  say "  failed or not attempted:${failed_list:- (none)}"
  if [ -n "$others_list" ]; then
    say "names taken by others cannot be reserved: pick other names for them"
  fi
  if [ -n "$failed_list" ] || [ -n "$others_list" ]; then
    say "rerun to retry the rest: names already registered are skipped"
    exit 1
  fi
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then main "$@"; fi
