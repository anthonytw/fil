#!/usr/bin/env bash
# `tests/release/just.sh release-keys unlock|status|forget`: the release's keys, unlocked once per session (the
# one-command release, its section 3; docs/m4/release.md section 10.4 has the one-time setup).
#
#   release-keys.sh unlock [--ttl 8h] [--need ssh,gpg,apple]
#   release-keys.sh status [--need ...]
#   release-keys.sh forget [--need ...]
#
# ssh    the release key (FIL_RELEASE_KEY, default ~/.ssh/fil-release) goes into the ssh-agent with
#        a lifetime (`ssh-add -t`); ssh-add asks for its passphrase. Every later signature uses the
#        public key and the agent. Checked with `ssh-add -T KEY.pub`, or, where ssh-add has no -T, a
#        test signature (`ssh-keygen -Y sign` with a copy of the public key alone) verified with it.
# gpg    the package key (the one key of .github/package-signing-key.asc, by fingerprint): its
#        passphrase is read here (no echo) and handed to gpg-agent with gpg-preset-passphrase on
#        stdin, then proved with a real signature (`--pinentry-mode error`: a wrong passphrase fails
#        here, not hours later) verified against the committed public key. A preset passphrase
#        ignores gpg-agent's own cache lifetimes, so a detached reaper forgets it at a wall-clock
#        deadline (it checks the clock every minute, so a Mac that sleeps does not stretch the TTL).
#        There is one reaper per keygrip and gpg home, recorded in one place per user (the reaper
#        directory below), whichever checkout or command started it; `forget` stops it and forgets
#        now. Needs `allow-preset-passphrase` in gpg-agent.conf (once; this script never edits it).
# apple  the Developer ID identity is in the keychain, the notarytool profile works, and a test
#        codesign of a scratch copy of a tiny binary succeeds (any keychain dialog shows up now,
#        not mid-release). Nothing is cached here: the keychain holds it.
#
# No secret is ever written to disk, put in an argument list or logged: the passphrase lives in one
# shell variable (bash's printf is a builtin, so piping it starts no process with it in argv),
# xtrace is off while it exists, and it is unset right after the preset. The reaper records hold a
# pid, a deadline and the gpg home, nothing else.
#
# --ttl    the session: N, Ns, Nm, Nh (default 8h, at most 24h).
# --need   which keys (default all three).
#
# The reaper directory: ${XDG_CACHE_HOME:-~/.cache}/fil/release-keys. status exits 0 if every needed
# key is ready, 1 if not. Environment: FIL_RELEASE_KEY, FIL_CODESIGN_IDENTITY, FIL_NOTARY_PROFILE
# (as sign-macos.sh). Test-only overrides (a NOTE each, refused unless the self-test's
# FIL_RELEASE_SELFTEST=1): FIL_SSH_ADD, FIL_GPG, FIL_GPG_PRESET, FIL_SECURITY, FIL_CODESIGN,
# FIL_NOTARYTOOL, FIL_PACKAGE_KEY_FILE, FIL_RELEASE_ROOT.
set -euo pipefail
# shellcheck source=tests/release/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

usage="usage: release-keys.sh unlock|status|forget [--ttl 8h] [--need ssh,gpg,apple]"
[ $# -ge 1 ] || die "$usage"
cmd=$1
shift
case $cmd in unlock | status | forget) ;; *) die "$usage" ;; esac
ttl_arg=8h need=ssh,gpg,apple
while [ $# -gt 0 ]; do
  case $1 in
    --ttl) [ $# -ge 2 ] || die "$usage"; ttl_arg=$2; shift 2 ;;
    --need) [ $# -ge 2 ] || die "$usage"; need=$2; shift 2 ;;
    *) die "$usage" ;;
  esac
done

# Every test-only override of common.sh's TEST_OVERRIDES: a NOTE each, and refused outside the
# self-test (this script reads FIL_SSH_ADD, FIL_GPG, FIL_GPG_PRESET, FIL_SECURITY, FIL_CODESIGN,
# FIL_NOTARYTOOL, FIL_PACKAGE_KEY_FILE and FIL_RELEASE_ROOT; any other set one is refused too).
test_override_notes
if [ "${FIL_RELEASE_SELFTEST:-}" != 1 ]; then
  for v in $TEST_OVERRIDES; do
    [ "$v" != FIL_RELEASE_SELFTEST ] || continue
    [ -z "${!v:-}" ] || die "STOP: $v is set (a test-only override): unlocking or checking a key with a faked tool is refused. Unset $v."
  done
fi

# The keys asked for.
want_ssh=0 want_gpg=0 want_apple=0
for k in $(printf '%s' "$need" | tr ',' ' '); do
  case $k in
    ssh) want_ssh=1 ;;
    gpg) want_gpg=1 ;;
    apple) want_apple=1 ;;
    *) die "--need: unknown key '$k' (ssh, gpg, apple)" ;;
  esac
done
[ "$want_ssh$want_gpg$want_apple" != 000 ] || die "--need names no key"

# The TTL, in seconds: 1 to 5 decimal digits (base 10 even with a leading zero), then a unit.
n=${ttl_arg%[smh]}
case $n in
  "" | *[!0-9]* | ??????*) die "--ttl '$ttl_arg': N, Ns, Nm or Nh" ;;
esac
case $ttl_arg in
  *h) ttl=$((10#$n * 3600)) ;;
  *m) ttl=$((10#$n * 60)) ;;
  *) ttl=$((10#$n)) ;;
esac
[ "$ttl" -gt 0 ] && [ "$ttl" -le 86400 ] || die "--ttl '$ttl_arg': more than 0 and at most 24h"

scratch=$(mktemp -d "${TMPDIR:-/tmp}/fil-keys.XXXXXX")
cleanup() { rm -rf "$scratch"; }
trap cleanup EXIT

hms() { local s=$1; [ "$s" -gt 0 ] 2> /dev/null || s=0; printf '%dh%02dm' $((s / 3600)) $((s % 3600 / 60)); }

# ---- ssh
ssh_add=${FIL_SSH_ADD:-ssh-add}
key=${FIL_RELEASE_KEY:-$HOME/.ssh/fil-release}
key=${key%.pub}
pub=$key.pub
ssh_agent_up() { "$ssh_add" -l > /dev/null 2>&1 || [ $? = 1 ]; } # 1: an agent with no keys
# ssh_loaded: the release key is in the agent and signs. ssh-add -T (OpenSSH 8.9+) where it exists;
# else a test signature with a copy of the public key ALONE, so that ssh-keygen cannot fall back to
# the private file next to it (that would prompt for the passphrase and prove nothing about the agent).
ssh_has_T() {
  local out
  out=$("$ssh_add" -T 2>&1 || true)
  case $out in *"illegal option"* | *"unknown option"* | *"unrecognized option"* | *"invalid option"*) return 1 ;; esac
}
ssh_loaded() {
  [ -f "$pub" ] || return 1
  if ssh_has_T; then
    "$ssh_add" -T "$pub" > /dev/null 2>&1
    return
  fi
  local d=$scratch/ssh-check
  rm -rf "$d" && mkdir "$d"
  cp "$pub" "$d/key.pub"
  printf 'fil-release-keys check\n' > "$d/msg"
  printf 'release %s\n' "$(cut -d' ' -f1,2 "$pub")" > "$d/signers"
  ssh-keygen -Y sign -f "$d/key.pub" -n fil-release-keycheck "$d/msg" < /dev/null > /dev/null 2>&1 \
    && ssh-keygen -Y verify -f "$d/signers" -I release -n fil-release-keycheck -s "$d/msg.sig" < "$d/msg" > /dev/null 2>&1
}
ssh_fp() { ssh-keygen -l -f "$pub" 2> /dev/null | cut -d' ' -f2; }
ssh_unlock() {
  [ -f "$key" ] || die "ssh: no release key at $key (FIL_RELEASE_KEY names another)"
  [ -f "$pub" ] || die "ssh: no public key at $pub (ssh-keygen -y -f $key > $pub)"
  ssh_agent_up || die "ssh: no ssh-agent (SSH_AUTH_SOCK); on macOS launchd starts one for a login session, over ssh run: eval \"\$(ssh-agent -s)\""
  if ssh_loaded; then
    # ssh-add cannot show a key's lifetime: one added by hand or by AddKeysToAgent may have none.
    log "ssh: the release key $(ssh_fp) is already in the agent, with whatever lifetime it was added with (ssh-add cannot show it); for this TTL: forget --need ssh, then unlock"
    return
  fi
  log "ssh: adding the release key $key to the agent for $(hms "$ttl") (ssh-add asks for its passphrase)"
  "$ssh_add" -t "$ttl" "$key" || die "ssh: ssh-add failed (wrong passphrase?); run the same command again"
  ssh_loaded || die "ssh: the key was added but a test signature with $pub failed"
  log "ssh: ready: $(ssh_fp) signs through the agent"
}
ssh_status() {
  if ssh_loaded; then echo "ssh    ready      the release key $(ssh_fp) is in the agent"; return 0; fi
  echo "ssh    locked     the release key ($pub) is not in the agent"
  return 1
}
ssh_forget() {
  if ssh_loaded; then
    "$ssh_add" -d "$pub" > /dev/null 2>&1 || die "ssh: ssh-add -d $pub failed"
    ! ssh_loaded || die "ssh: the release key is still in the agent after ssh-add -d"
    log "ssh: removed the release key from the agent"
  fi
}

# ---- gpg
gpg=${FIL_GPG:-gpg}
keyfile=${FIL_PACKAGE_KEY_FILE:-$ROOT/.github/package-signing-key.asc}
gpg_fpr() {
  command -v "$gpg" > /dev/null || die "gpg: needs gpg (brew install gnupg)"
  grep -q 'BEGIN PGP PUBLIC KEY BLOCK' "$keyfile" 2> /dev/null || die "gpg: $keyfile holds no public key"
  package_key_fingerprint "$keyfile" || die "gpg: $keyfile is not exactly one OpenPGP public key"
}
# gpg_grips FPR: the keygrips of the secret key's parts that sign (the key is sign-only, so the
# primary's; a signing subkey would be listed too).
gpg_grips() {
  LC_ALL=C "$gpg" -K --with-colons --with-keygrip "$1" 2> /dev/null \
    | awk -F: '($1 == "sec" || $1 == "ssb") { s = ($12 ~ /s/) } $1 == "grp" && s { print $10; s = 0 }' || true
}
agent() { LC_ALL=C gpg-connect-agent --no-autostart "$@" /bye 2> /dev/null; }
# gpg_cached GRIP: gpg-agent holds a passphrase for GRIP (KEYINFO's seventh field is 1).
gpg_cached() { agent "KEYINFO $1" | awk '$1 == "S" && $2 == "KEYINFO" { c = $7 } END { exit c != "1" }'; }
# gpg_clear GRIP...: forget, then require that the agent really holds nothing for each.
gpg_clear() {
  local g bad=0
  for g in "$@"; do
    agent "CLEAR_PASSPHRASE --mode=normal $g" > /dev/null || true
    if gpg_cached "$g"; then log "gpg: STILL CACHED after CLEAR_PASSPHRASE: keygrip $g"; bad=1; fi
  done
  return $bad
}
preset_tool() {
  if [ -n "${FIL_GPG_PRESET:-}" ]; then echo "$FIL_GPG_PRESET"; else echo "$(gpgconf --list-dirs libexecdir)/gpg-preset-passphrase"; fi
}

# The reapers: one per keygrip and gpg home, whatever started it, so a second unlock (another
# checkout, `tests/release/just.sh release`) replaces it instead of leaving one behind that fires mid-release.
# Record: REAPERS/<cksum of the gpg home>-<GRIP>, lines pid=, deadline= (epoch seconds), homedir=.
reapers=${XDG_CACHE_HOME:-$HOME/.cache}/fil/release-keys
gpg_home() { gpgconf --list-dirs homedir; }
reaper_file() { printf '%s/%s-%s' "$reapers" "$(gpg_home | cksum | cut -d' ' -f1)" "$1"; }
reaper_get() { sed -n "s/^$2=//p" "$(reaper_file "$1")" 2> /dev/null | tail -n 1 || true; }
# reaper_num GRIP FIELD: the field if it is a decimal number, else nothing (never arithmetic on a
# record's raw text).
reaper_num() {
  local v
  v=$(reaper_get "$1" "$2")
  case $v in "" | *[!0-9]* | ?????????????*) ;; *) echo "$v" ;; esac
}
# reaper_corrupt GRIP: a record exists but its pid or deadline is not a number.
reaper_corrupt() { [ -f "$(reaper_file "$1")" ] && { [ -z "$(reaper_num "$1" pid)" ] || [ -z "$(reaper_num "$1" deadline)" ]; }; }
# The record directory: private (it steers which pid forget and unlock signal), and ours.
reapers_dir() {
  (umask 077 && mkdir -p "$reapers")
  chmod 700 "$reapers"
  [ -O "$reapers" ] || die "gpg: $reapers is not owned by this user"
}
# The unlock lock, one per key (its first keygrip): a second unlock waits instead of racing the
# stop-preset-start sequence and orphaning a reaper. A lock whose pid is dead is taken over.
lock_dir=''
lock_take() {
  local l n=0 holder
  reapers_dir
  l=$(reaper_file "$1").lock
  until mkdir "$l" 2> /dev/null; do
    holder=$(cat "$l/pid" 2> /dev/null || true)
    if [ -n "$holder" ] && ! kill -0 "$holder" 2> /dev/null; then
      log "gpg: removing a stale unlock lock (pid $holder is gone)"
      rm -rf "$l"
      continue
    fi
    [ $n -lt 120 ] || die "gpg: another unlock (pid ${holder:-?}) holds $l; wait for it or remove the lock"
    [ $n -gt 0 ] || log "gpg: waiting for another unlock (pid ${holder:-?})"
    n=$((n + 1))
    sleep 1
  done
  echo $$ > "$l/pid"
  lock_dir=$l
}
lock_drop() { [ -z "$lock_dir" ] || rm -rf "$lock_dir"; lock_dir=''; }
reaper_alive() { # PID: alive and ours (the marker is in its argument list), not a reused pid
  [ -n "$1" ] && ps -p "$1" -o command= 2> /dev/null | grep -q 'fil-release-keys-reaper'
}
reaper_stop() { # GRIP
  local pid
  pid=$(reaper_num "$1" pid)
  if reaper_alive "$pid"; then kill "$pid" 2> /dev/null || true; fi
  rm -f "$(reaper_file "$1")"
}
# reaper_start GRIP DEADLINE: detached; it sleeps at most a minute at a time and compares the clock
# with the deadline, so time asleep counts. TERM (from `forget`) stops the sleep too. It removes its
# record only if the record still names it.
reaper_start() {
  local f pid
  f=$(reaper_file "$1")
  reapers_dir
  # shellcheck disable=SC2016 # the script is single-quoted on purpose
  nohup /bin/sh -c 'trap "kill \$s 2>/dev/null; exit 0" TERM
    while :; do
      now=$(date +%s); [ "$now" -ge "$2" ] && break
      r=$(($2 - now)); [ "$r" -gt 60 ] && r=60
      sleep "$r" & s=$!; wait $s
    done
    gpg-connect-agent --no-autostart "CLEAR_PASSPHRASE --mode=normal $1" /bye
    grep -qx "pid=$$" "$3" 2>/dev/null && rm -f "$3"' \
    fil-release-keys-reaper "$1" "$2" "$f" < /dev/null > /dev/null 2>&1 &
  pid=$!
  (umask 077 && printf 'pid=%s\ndeadline=%s\nhomedir=%s\n' "$pid" "$2" "$(gpg_home)" > "$f.tmp.$$")
  mv "$f.tmp.$$" "$f"
  echo "$pid"
}
agent_conf_allows_preset() {
  grep -Eq '^[[:space:]]*allow-preset-passphrase[[:space:]]*$' "$(gpg_home)/gpg-agent.conf" 2> /dev/null
}
gpg_setup_text() {
  cat >&2 << 'EOF'
release-keys: gpg: STOP: gpg-agent does not allow a preset passphrase. Once, before unlocking
  (docs/m4/release.md section 10.4; a reload empties the agent's cache):

    echo allow-preset-passphrase >> "$(gpgconf --list-dirs homedir)/gpg-agent.conf"
    gpgconf --reload gpg-agent

  then run the same command again.
EOF
}
# gpg_test_sign FPR: a real signature with no pinentry, verified with the committed public key in a
# scratch gpg home of its own (never the owner's keyring, whatever common.conf says about keyboxd).
gpg_test_sign() {
  local d=$scratch/gpg-check
  rm -rf "$d" && mkdir -m 700 "$d" "$d/home"
  printf 'fil-release-keys check\n' > "$d/msg"
  "$gpg" --batch --pinentry-mode error --local-user "$1" --detach-sign --output "$d/msg.sig" "$d/msg" > /dev/null 2>&1 || return 1
  "$gpg" --batch --no-autostart --homedir "$d/home" --import "$keyfile" > /dev/null 2>&1 || return 1
  LC_ALL=C "$gpg" --batch --no-autostart --homedir "$d/home" --status-fd 1 --verify "$d/msg.sig" "$d/msg" 2> /dev/null \
    | awk -v f="$1" '$1 == "[GNUPG:]" && $2 == "VALIDSIG" && ($3 == f || $NF == f) { ok = 1 } END { exit !ok }'
}
# gpg_reaped GRIPS: every grip has a live reaper whose deadline is still ahead.
gpg_reaped() {
  local g
  for g in "$@"; do
    reaper_alive "$(reaper_num "$g" pid)" && [ "$(reaper_num "$g" deadline)" -gt "$(date +%s)" ] 2> /dev/null || return 1
  done
}
gpg_unlock() {
  local fpr grips g preset pass deadline pid xtrace=0
  fpr=$(gpg_fpr)
  grips=$(gpg_grips "$fpr")
  [ -n "$grips" ] || die "gpg: the package key $fpr ($keyfile) is not a secret key in this keyring ($(gpg_home))"
  # shellcheck disable=SC2086 # hex words
  set -- $grips
  lock_take "$1"
  if gpg_reaped "$@" && gpg_test_sign "$fpr"; then
    lock_drop
    log "gpg: the package key $fpr is already unlocked, forgotten in $(hms $(($(reaper_num "$1" deadline) - $(date +%s)))) (run 'forget' first for a fresh TTL)"
    return
  fi
  trap 'lock_drop; cleanup' EXIT
  agent_conf_allows_preset || { gpg_setup_text; exit 1; }
  preset=$(preset_tool)
  [ -x "$preset" ] || die "gpg: no gpg-preset-passphrase at $preset"
  for g in $grips; do reaper_stop "$g"; done
  # From here until the reapers run, a failure or an interrupt must not leave a passphrase behind.
  # The trap names $grips, a local of this function: it works because every exit until the trap is
  # replaced (die, exit, a signal) happens inside this function, where the local is in scope.
  # shellcheck disable=SC2086 # the grips are hex words
  trap 'gpg_clear $grips || true; lock_drop; cleanup' EXIT
  trap 'exit 130' INT TERM HUP
  # No xtrace while the passphrase exists (bash -x or SHELLOPTS=xtrace would print it).
  case $- in *x*) xtrace=1 ;; esac
  { set +x; } 2> /dev/null
  printf 'release-keys: gpg: passphrase of the package key %s: ' "$fpr" >&2
  if ! IFS= read -rs pass; then
    unset pass
    echo >&2
    [ "$xtrace" = 0 ] || set -x
    die "gpg: no passphrase read"
  fi
  echo >&2
  for g in $grips; do
    if ! printf '%s\n' "$pass" | "$preset" --preset "$g" > "$scratch/preset.log" 2>&1; then
      unset pass
      [ "$xtrace" = 0 ] || set -x
      sed 's/^/  /' "$scratch/preset.log" >&2
      if grep -q 'Not supported' "$scratch/preset.log"; then gpg_setup_text; exit 1; fi
      die "gpg: gpg-preset-passphrase failed"
    fi
  done
  unset pass
  [ "$xtrace" = 0 ] || set -x
  gpg_test_sign "$fpr" || die "gpg: a test signature with the package key $fpr failed (wrong passphrase?); nothing is cached; run the same command again"
  deadline=$(($(date +%s) + ttl))
  for g in $grips; do pid=$(reaper_start "$g" "$deadline"); done
  lock_drop
  trap cleanup EXIT
  trap - INT TERM HUP
  log "gpg: ready: the package key $fpr signs with no prompt; forgotten in $(hms "$ttl") (reaper pid $pid)"
}
gpg_status() {
  local fpr grips g pid left
  fpr=$(gpg_fpr) || { echo "gpg    missing    no package key ($keyfile)"; return 1; }
  grips=$(gpg_grips "$fpr")
  if [ -z "$grips" ]; then echo "gpg    missing    the package key $fpr is not in this keyring"; return 1; fi
  for g in $grips; do
    gpg_cached "$g" || { echo "gpg    locked     the package key $fpr has no cached passphrase"; return 1; }
  done
  for g in $grips; do
    if reaper_corrupt "$g"; then
      echo "gpg    CORRUPT    the reaper record $(reaper_file "$g") is not pid=/deadline= numbers; run: tests/release/just.sh release-keys forget --need gpg"
      return 1
    fi
  done
  # shellcheck disable=SC2086 # hex words
  set -- $grips
  pid=$(reaper_num "$1" pid)
  left=$(($(reaper_num "$1" deadline) + 0 - $(date +%s)))
  if ! reaper_alive "$pid"; then
    echo "gpg    NO-REAPER  the package key $fpr is cached with no reaper: a preset passphrase does not expire; run: tests/release/just.sh release-keys forget --need gpg"
    return 1
  elif [ "$left" -le 0 ]; then
    echo "gpg    OVERDUE    the package key $fpr is still cached past its deadline (reaper pid $pid); run: tests/release/just.sh release-keys forget --need gpg"
    return 1
  fi
  echo "gpg    ready      the package key $fpr is cached, forgotten in $(hms "$left") (reaper pid $pid)"
}
gpg_forget() {
  local fpr grips g
  fpr=$(gpg_fpr)
  grips=$(gpg_grips "$fpr")
  for g in $grips; do reaper_stop "$g"; done
  # shellcheck disable=SC2086 # hex words
  gpg_clear $grips || die "gpg: the passphrase is still cached after forgetting (see above); try: gpgconf --reload gpg-agent"
  log "gpg: forgot the package key's passphrase"
}

# ---- apple (the defaults of sign-macos.sh)
identity=${FIL_CODESIGN_IDENTITY:-$DEFAULT_CODESIGN_IDENTITY}
profile=${FIL_NOTARY_PROFILE:-fil-notary}
security=${FIL_SECURITY:-security}
read -r -a notarytool <<< "${FIL_NOTARYTOOL:-xcrun notarytool}"
apple_platform() { [ "$(uname -s)" = Darwin ] || [ -n "${FIL_SECURITY:-}" ]; }
# (The listing is read whole, not piped into `grep -q`: grep quitting early would SIGPIPE security,
# and pipefail would turn a found identity into a failure.)
apple_identity() {
  local ids
  ids=$("$security" find-identity -v -p codesigning 2> /dev/null) || return 1
  case $ids in *"\"$identity\""*) ;; *) return 1 ;; esac
}
apple_unlock() {
  local bin
  apple_platform || die "apple: the Developer ID checks need macOS (or --need without apple)"
  # Over ssh the login keychain is often locked; security asks for its password itself.
  if [ -n "${SSH_CONNECTION:-}" ]; then
    "$security" unlock-keychain login.keychain-db || die "apple: the login keychain stays locked"
  fi
  apple_identity || die "apple: no valid codesigning identity \"$identity\" in the keychain (security find-identity -v -p codesigning)"
  "${notarytool[@]}" history --keychain-profile "$profile" > "$scratch/notary.log" 2>&1 \
    || { tail -n 5 "$scratch/notary.log" >&2; die "apple: notarytool profile '$profile' does not work (xcrun notarytool store-credentials $profile)"; }
  bin=/usr/bin/true
  [ -f "$bin" ] || bin=/bin/true
  cp "$bin" "$scratch/fil-keycheck"
  chmod u+w "$scratch/fil-keycheck"
  log "apple: a test codesign with \"$identity\" (allow the keychain dialog, if one shows: \"Always Allow\")"
  cs --force --sign "$identity" --timestamp --options runtime "$scratch/fil-keycheck" > "$scratch/cs.log" 2>&1 \
    || { cat "$scratch/cs.log" >&2; die "apple: the test codesign failed"; }
  cs --verify --strict "$scratch/fil-keycheck" > /dev/null 2>&1 || die "apple: the test signature does not verify"
  log "apple: ready: \"$identity\" signs, profile '$profile' works"
}
apple_status() {
  if ! apple_platform; then echo "apple  n/a        not macOS"; return 1; fi
  if apple_identity; then echo "apple  ready      \"$identity\" is in the keychain"; return 0; fi
  echo "apple  missing    no valid codesigning identity \"$identity\""
  return 1
}

# ---- the command
rc=0
case $cmd in
  unlock)
    [ "$want_ssh" = 0 ] || ssh_unlock
    [ "$want_gpg" = 0 ] || gpg_unlock
    [ "$want_apple" = 0 ] || apple_unlock
    log "unlocked: $need"
    ;;
  status)
    if [ "$want_ssh" = 1 ]; then ssh_status || rc=1; fi
    if [ "$want_gpg" = 1 ]; then gpg_status || rc=1; fi
    if [ "$want_apple" = 1 ]; then apple_status || rc=1; fi
    ;;
  forget)
    [ "$want_ssh" = 0 ] || ssh_forget
    [ "$want_gpg" = 0 ] || gpg_forget
    ;;
esac
exit "$rc"
