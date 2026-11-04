#!/usr/bin/env bash
# macOS as the REMOTE host (DESIGN.md §16.9, docs/m4/plan.md MAC). The real release `fil` runs its
# server side on this Mac and its client drives it over a real ssh login to localhost: an sshd on
# this machine (tests/macos/sshd.sh: a user-mode one by default, the system one with
# MACOS_SSHD=system), the client on a pseudo-terminal (ptydrive, as in the real-network harness).
# Run: `just macos-remote [SCENARIO...]` (builds first); `tests/macos/remote.sh --list`.
#
# Needs: macOS, the binaries `fil`, `ptydrive` and `fwdprobe` in $FIL_BIN_DIR (the recipe builds
# them: `fil` in a build of its own, so it has no test hooks), python3 and curl (the -D check).
# It starts an sshd and a `fil` daemon of its own and stops them (by pid) at the end. The daemon
# lives in /tmp/fil-$UID, the same place as your own, so the run REFUSES to start when a daemon of
# yours is alive (set MACOS_REMOTE_SHARED=1 to run anyway; sessions here are all named `mac-*`).
#
# Env: FIL_BIN_DIR (default target/release), MACOS_SSHD=user|system, MACOS_STRICT=1 (a known finding
# fails its scenario instead of being recorded), MACOS_OUT (results; default target/macos-remote),
# MACOS_WORK (scratch; default a fresh /tmp/filmac.XXXXXX).
#
#   attach_type          attach, type, uname says Darwin
#   kill_ssh_resume      kill -9 the client's ssh in the middle of output: nothing lost, it resumes
#   detach_reattach      ~. and come back: the same shell, the output kept
#   daemon_survives_login  every ssh login ended: the daemon and the session are still there (macOS
#                        has no logind)
#   takeover             a second client takes the session; the first is told, the second sees it all
#   forwards_reconnect   -L, -R and -D, and a transfer through each that outlives a killed ssh
#   agent                -A with a throwaway ssh-agent: a real ssh hop from the session; refused without -A
#   quic_upgrade         pipe to QUIC on loopback, the session continues with the ssh gone
#   upgrade_in_place     the binary is replaced under a running daemon (the trampoline, plan §1)
#   install_acl          PLACEHOLDER: fil install and the macOS ACL cases (U1)
#   tmp_cleaners         PLACEHOLDER: TMP's mechanism checks and macOS's clean-tmps (TMP)
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$HERE/../.." && pwd)
export REALNET_OUT=${MACOS_OUT:-$REPO_ROOT/target/macos-remote}
OUT=$REALNET_OUT
ENVFILE=$OUT/macos.env
# A scenario process (`--one`) reads what the setup decided; this must come before fil.sh, which
# reads $PD when it is sourced.
# shellcheck disable=SC1090
[[ -f $ENVFILE ]] && source "$ENVFILE"

# The helpers of the real-network harness: assertions, the scenario runner and summary table
# (lib.sh), and the pseudo-terminal client driver (fil.sh). Sourced read-only.
# shellcheck source=../realnet/lib.sh
source "$HERE/../realnet/lib.sh"
# shellcheck source=../realnet/fil.sh
source "$HERE/../realnet/fil.sh"
# shellcheck source=sshd.sh
source "$HERE/sshd.sh"
REALNET_STRICT=${MACOS_STRICT:-0}

SCENARIOS=(
  attach_type kill_ssh_resume detach_reattach daemon_survives_login takeover
  forwards_reconnect agent quic_upgrade upgrade_in_place install_acl tmp_cleaners
)

# ---------------------------------------------------------------------------------------------
# What the harness provides to fil.sh: the client and everything it starts run on this machine.

client_exec_on() { shift; "$@"; }
client_bg_on() {
  shift
  "$@" > /dev/null 2>&1 &
}

# Sessions and helper processes of the running scenario; scenario_cleanup (called by fc_finish)
# removes them. All by name or pid this scenario made.
SESSIONS=()
HELPERS=()

fil_cmd() { env "XDG_STATE_HOME=$W/state-cmd" fil --config "$CFG" "$@"; }

scenario_cleanup() {
  local s h
  for s in ${SESSIONS[@]+"${SESSIONS[@]}"}; do fil_cmd kill macremote "$s" > /dev/null 2>&1; done
  for h in ${HELPERS[@]+"${HELPERS[@]}"}; do kill "$h" > /dev/null 2>&1; done
  clock_restore
  return 0
}

# attach NAME SESSION [FIL_ARGS...]: a client on a pty, attached over the pipe unless told otherwise.
attach() {
  local name=$1 session=$2
  shift 2
  SESSIONS+=("$session")
  fc_start "$name" -- --config "$CFG" "$@" macremote "$session"
  fc_ready "$name"
}

# helper CMD...: a background process that the scenario stops at its end.
helper() {
  "$@" > /dev/null 2>&1 &
  HELPERS+=($!)
}

wait_log() {
  local i
  for ((i = 0; i < $3 * 10; i++)); do
    if [[ $(fc_count_log "$1" "$2") -ge ${4:-1} ]]; then return 0; fi
    sleep 0.1
  done
  fail "$1: the client log never showed '$2' (at least ${4:-1}) within $3 s: $(fc_log "$1" | tail -n 5)"
}

# The client's ssh process (the pipe), if it has one.
pipe_ssh() { pgrep -P "$1" -x ssh | head -n 1 || true; }

# The live daemons of this user: the pids in the lock files.
daemon_pids() {
  local f p
  for f in "/tmp/fil-$(id -u)"/*.lock; do
    [[ -f $f ]] || continue
    p=$(tr -dc '0-9' < "$f")
    [[ -n $p ]] && kill -0 "$p" 2> /dev/null && echo "$p"
  done
  return 0
}

# The daemon of the wire this build speaks (one, after our setup refused to start with any).
the_daemon() {
  local p
  p=$(daemon_pids | head -n 1)
  [[ -n $p ]] || fail "no daemon is running"
  printf '%s' "$p"
}

# A free TCP port, one the system chose.
free_port() { python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }

wait_listen() {
  local i
  for ((i = 0; i < ${2:-100}; i++)); do
    nc -z 127.0.0.1 "$1" 2> /dev/null && return 0
    sleep 0.1
  done
  fail "nothing listens on 127.0.0.1:$1"
}

fpv() { sed -n "s/^$2=//p" "$1" | tail -n 1; }

# ---------------------------------------------------------------------------------------------
# Scenarios

scenario_attach_type() {
  attach a mac-attach
  fc_line a 'echo OS-$(uname -s)-$((6*7))'
  fc_wait a 'OS-Darwin-42'
  fc_line a 'echo ARCH-$(uname -m)-$((6*7))'
  fc_wait a "ARCH-$(uname -m)-42"
  note "arch=$(uname -m)"
  fc_line a 'echo SHELL-$(basename "$SHELL")-X$((6*7))'
  fc_wait a '-X42'
  note "login_shell=$(fc_screen a | grep -o 'SHELL-[a-z]*-X42' | tail -n 1)"
  fc_detach a
}

scenario_kill_ssh_resume() {
  attach a mac-kill
  fc_line a 'for i in $(seq 1 60); do echo TICK-$i; sleep 0.2; done; echo TICKDONE-$((6*7))'
  fc_wait a 'TICK-12' 20
  local pid ssh1 ssh2 i
  pid=$(fc_pid a)
  ssh1=$(pipe_ssh "$pid")
  [[ -n $ssh1 ]] || fail "the client has no ssh child (pid $pid)"
  kill -9 "$ssh1"
  fc_wait a 'TICKDONE-42' 90
  fc_seq_ok a TICK 1 60 || fail "the ticks are not 1..60 exactly, once each: $(fc_screen a | grep -c '^TICK-')"
  ok "all 60 ticks, in order, once"
  ssh2=""
  for ((i = 0; i < 50; i++)); do ssh2=$(pipe_ssh "$pid"); [[ -n $ssh2 ]] && break; sleep 0.2; done
  [[ -n $ssh2 && $ssh2 != "$ssh1" ]] || fail "no new ssh after the kill (old $ssh1, now '$ssh2')"
  fc_ready a
  fc_detach a
}

scenario_detach_reattach() {
  attach a mac-reattach
  fc_show_pid a
  local sp
  sp=$(shell_pid a)
  [[ -n $sp ]] || fail "no shell pid"
  fc_line a 'seq 1 100 | sed s/^/LINE-/'
  fc_wait a 'LINE-100'
  fc_detach a
  attach b mac-reattach
  fc_show_pid b
  [[ $(shell_pid b) == "$sp" ]] || fail "the shell changed: $sp, now $(shell_pid b)"
  ok "reattached to the same shell ($sp)"
  fc_seq_ok b LINE 1 100 || fail "the scrollback does not hold LINE-1..100 once each"
  fc_detach b
}

# macOS has no logind: nothing ends a user's processes when the login ends. Check that, with every
# ssh gone, the daemon and the session stay.
scenario_daemon_survives_login() {
  attach a mac-survive
  fc_show_pid a
  local sp d
  sp=$(shell_pid a)
  d=$(the_daemon)
  fc_line a 'sleep 1000 & echo BGOK-$((6*7))'
  fc_wait a 'BGOK-42'
  fc_detach a
  sleep 3
  # No relay and no login of ours is left.
  if pgrep -u "$(id -u)" -f 'fil server --stdio' > /dev/null; then
    fail "a fil server --stdio relay is still running: $(pgrep -u "$(id -u)" -fl 'fil server --stdio')"
  fi
  sleep 12
  kill -0 "$d" 2> /dev/null || fail "the daemon (pid $d) is gone after the logins ended"
  note "daemon_ppid=$(ps -o ppid= -p "$d" | tr -d ' ')"
  local ls
  ls=$(fil_cmd ls macremote 2>&1) || fail "fil ls failed: $ls"
  grep -q 'mac-survive' <<< "$ls" || fail "fil ls does not list the session: $ls"
  ok "the daemon (pid $d) and the session survived with no login"
  attach b mac-survive
  fc_show_pid b
  [[ $(shell_pid b) == "$sp" ]] || fail "the shell changed: $sp, now $(shell_pid b)"
  fc_line b 'jobs'
  fc_wait b 'sleep 1000'
  ok "the same shell, and its background job"
  fc_line b 'kill %1'
  fc_detach b
}

scenario_takeover() {
  attach a mac-take
  fc_line a 'for i in $(seq 1 60); do echo TICK-$i; sleep 0.25; done; echo TICKDONE-$((6*7))'
  fc_wait a 'TICK-8' 20
  fc_start b -- --config "$CFG" macremote mac-take
  fc_wait a 'was taken over' 30
  # The client asks whether to take it back; no: it leaves the session alone and exits 1.
  fc_wait a 'Take it back?' 10
  fc_answer a n
  fc_expect_exit a 1 15
  fc_wait b 'TICKDONE-42' 60
  fc_seq_ok b TICK 1 60 || fail "the second client's ticks are not 1..60 exactly"
  ok "the second client saw ticks 1..60 exactly"
  fc_ready b
  fc_detach b
}

# -L, -R and -D, each used before and after the pipe is cut, and a transfer through -L and -R that
# is running when the ssh is killed and must still arrive whole.
scenario_forwards_reconnect() {
  local lport rport dport lt rt ht
  lport=$(free_port) rport=$(free_port) dport=$(free_port) lt=$(free_port) rt=$(free_port) ht=$(free_port)
  mkdir -p "$W/www" "$OUT/run/$SCENARIO"
  echo http-ok-42 > "$W/www/f.txt"
  # -L: the client listens on $lport, the host connects to the echo server on $lt.
  fwdprobe serve "127.0.0.1:$lt" echo > "$OUT/run/$SCENARIO/lt.out" 2>&1 &
  HELPERS+=($!)
  # -R: the host listens on $rport, the client connects to the echo server on $rt.
  fwdprobe serve "127.0.0.1:$rt" echo > "$OUT/run/$SCENARIO/rt.out" 2>&1 &
  HELPERS+=($!)
  # -D: a web server the SOCKS proxy reaches.
  (cd "$W/www" && exec python3 -m http.server --bind 127.0.0.1 "$ht") > /dev/null 2>&1 &
  HELPERS+=($!)
  wait_listen "$lt"
  wait_listen "$rt"
  wait_listen "$ht"
  attach a mac-fwd -L "$lport:127.0.0.1:$lt" -R "$rport:127.0.0.1:$rt" -D "$dport"
  wait_listen "$lport"
  wait_listen "$dport"
  # The -R listener is made by the daemon on the host: a connection through it needs the client end.
  local i d1 d2
  d1=$(fwdprobe digest 100000 --seed 5 | sed -n 's/^SHA256=//p')
  d2=$(fwdprobe digest 3000000 --seed 6 | sed -n 's/^SHA256=//p')
  # Before the cut: one of each.
  fwdprobe connect "127.0.0.1:$lport" send 100000 --seed 5 > "$OUT/run/$SCENARIO/l1.out" 2>&1 || fail "-L before: $(cat "$OUT/run/$SCENARIO/l1.out")"
  [[ $(fpv "$OUT/run/$SCENARIO/l1.out" IN_SHA256) == "$d1" ]] || fail "-L before: the echo differs: $(cat "$OUT/run/$SCENARIO/l1.out")"
  for ((i = 0; i < 100; i++)); do nc -z 127.0.0.1 "$rport" 2> /dev/null && break; sleep 0.1; done
  fwdprobe connect "127.0.0.1:$rport" send 100000 --seed 5 > "$OUT/run/$SCENARIO/r1.out" 2>&1 || fail "-R before: $(cat "$OUT/run/$SCENARIO/r1.out")"
  [[ $(fpv "$OUT/run/$SCENARIO/r1.out" IN_SHA256) == "$d1" ]] || fail "-R before: the echo differs: $(cat "$OUT/run/$SCENARIO/r1.out")"
  [[ $(curl -s --max-time 10 --socks5-hostname "127.0.0.1:$dport" "http://127.0.0.1:$ht/f.txt") == http-ok-42 ]] || fail "-D before: no answer through the proxy"
  ok "-L, -R and -D each carried a connection"
  # A paced 3 MB echo through each of -L and -R (about 10 s at 300 kB/s), and the ssh killed in the
  # middle: the bytes must all arrive, whole.
  fwdprobe connect "127.0.0.1:$lport" send 3000000 --seed 6 --rate 300000 > "$OUT/run/$SCENARIO/l2.out" 2>&1 &
  local lp=$!
  fwdprobe connect "127.0.0.1:$rport" send 3000000 --seed 6 --rate 300000 > "$OUT/run/$SCENARIO/r2.out" 2>&1 &
  local rp=$!
  sleep 3
  local pid ssh1
  pid=$(fc_pid a)
  ssh1=$(pipe_ssh "$pid")
  [[ -n $ssh1 ]] || fail "no ssh child of the client"
  kill -9 "$ssh1"
  wait "$lp" || fail "-L transfer through the cut failed: $(cat "$OUT/run/$SCENARIO/l2.out")"
  wait "$rp" || fail "-R transfer through the cut failed: $(cat "$OUT/run/$SCENARIO/r2.out")"
  [[ $(fpv "$OUT/run/$SCENARIO/l2.out" IN_SHA256) == "$d2" && $(fpv "$OUT/run/$SCENARIO/l2.out" IN_BYTES) == 3000000 ]] \
    || fail "-L through the cut: the echo differs: $(cat "$OUT/run/$SCENARIO/l2.out")"
  [[ $(fpv "$OUT/run/$SCENARIO/r2.out" IN_SHA256) == "$d2" && $(fpv "$OUT/run/$SCENARIO/r2.out" IN_BYTES) == 3000000 ]] \
    || fail "-R through the cut: the echo differs: $(cat "$OUT/run/$SCENARIO/r2.out")"
  ok "a 3 MB echo through -L and one through -R survived a killed ssh, byte for byte"
  # After the cut: new connections on all three.
  fc_ready a
  fwdprobe connect "127.0.0.1:$lport" burst --count 20 --parallel 4 > "$OUT/run/$SCENARIO/l3.out" 2>&1 || fail "-L after: $(cat "$OUT/run/$SCENARIO/l3.out")"
  fwdprobe connect "127.0.0.1:$rport" burst --count 20 --parallel 4 > "$OUT/run/$SCENARIO/r3.out" 2>&1 || fail "-R after: $(cat "$OUT/run/$SCENARIO/r3.out")"
  [[ $(curl -s --max-time 10 --socks5-hostname "127.0.0.1:$dport" "http://127.0.0.1:$ht/f.txt") == http-ok-42 ]] || fail "-D after: no answer through the proxy"
  ok "20 connections each through -L and -R, and -D, after the reconnect"
  fc_detach a
}

scenario_agent() {
  local sock=$W/agent.sock key=$W/agentkey out pid fp
  rm -f "$sock" "$key" "$key.pub"
  ssh-keygen -q -t ed25519 -N '' -C fil-macos-agent -f "$key"
  out=$(ssh-agent -a "$sock")
  pid=$(sed -n 's/.*SSH_AGENT_PID=\([0-9]*\).*/\1/p' <<< "$out" | head -n 1)
  [[ -n $pid ]] || fail "ssh-agent did not start: $out"
  HELPERS+=("$pid")
  SSH_AUTH_SOCK=$sock ssh-add -q "$key" || fail "ssh-add into the throwaway agent failed"
  fp=$(ssh-keygen -lf "$key.pub" | awk '{ print $2 }')
  [[ $fp == SHA256:* ]] || fail "no fingerprint: $fp"
  # sshd takes the agent's key too (the harness's own key stays for the pipe).
  auth_add "$key.pub"
  # shellcheck disable=SC2034 # read by fc_start
  FC_ENV=("SSH_AUTH_SOCK=$sock")
  attach a mac-agent -A
  # shellcheck disable=SC2034
  FC_ENV=()
  fc_line a 'ssh-add -l; echo LIST-$((6*7))'
  fc_wait a 'LIST-42' 20
  fc_has a "$fp" || fail "ssh-add -l in the session does not list the agent's key $fp: $(fc_screen a | tail -n 5)"
  ok "the session sees the agent's key"
  local hop="ssh -F /dev/null -o BatchMode=yes -o ConnectTimeout=10 -o UserKnownHostsFile=$SSH_KNOWN_HOSTS -o StrictHostKeyChecking=yes -p $SSH_PORT $(id -un)@127.0.0.1"
  fc_line a "$hop 'echo HOP-\$((6*7))'; echo RC-\$((1000+\$?))"
  fc_wait a 'RC-1' 30
  fc_has a 'HOP-42' && fc_has a 'RC-1000' || fail "the ssh hop with -A failed: $(fc_screen a | tail -n 6)"
  ok "a real ssh hop from the session authenticated with the relayed key"
  fc_detach a
  # The control: no -A, the same hop is refused for want of a key.
  attach b mac-agent-no
  fc_line b "$hop 'echo HOP-\$((6*7))'; echo RC-\$((2000+\$?))"
  fc_wait b 'RC-2' 30
  fc_has b 'RC-2000' && fail "the hop without -A succeeded: $(fc_screen b | tail -n 6)"
  fc_has b 'Permission denied (publickey' || fail "the hop without -A did not fail with a publickey refusal: $(fc_screen b | tail -n 6)"
  ok "without -A the hop is refused (publickey)"
  fc_detach b
}

scenario_quic_upgrade() {
  attach a mac-quic --transport auto
  wait_log a 'Switched(Direct)' 30
  local pid ssh1 d
  pid=$(fc_pid a)
  d=$(the_daemon)
  note "daemon_udp=$(lsof -nP -a -p "$d" -iUDP 2> /dev/null | awk 'NR > 1 { print $9 }' | tr '\n' ',')"
  ssh1=$(pipe_ssh "$pid")
  if [[ -n $ssh1 ]]; then kill -9 "$ssh1"; fi
  sleep 1
  fc_line a 'for i in $(seq 1 30); do echo QT-$i; sleep 0.1; done; echo QTDONE-$((6*7))'
  fc_wait a 'QTDONE-42' 30
  fc_seq_ok a QT 1 30 || fail "ticks over QUIC are not exact"
  [[ $(fc_count_log a 'Switched(Pipe)') -le 1 ]] || fail "the client went back to the pipe: $(fc_log a | grep -F 'Switched(')"
  ok "on QUIC (loopback) with the ssh gone, the session works"
  fc_detach a
}

# The plan's trampoline case (§1): a running daemon starts a session's child through the
# trampoline, `current_exe()` on macOS, a PATH. A package upgrade replaces the file; the old daemon
# must not start a new session through the NEW file. The replacement here is a script that records
# every trampoline call and hands on to a copy of the real binary, so the session still starts and
# the record says whether the replaced path was used.
scenario_upgrade_in_place() {
  local mark=$W/trampoline.mark d1 d2 exe newfil=$W/fil-new nofile
  rm -f "$mark"
  # The trampoline is in use only when the daemon raised its descriptor limit: the login's soft
  # limit must be below the macOS cap (10,240), as launchd's default of 256 is.
  nofile=$(ssh -F "$SSH_CONFIG" -T macremote 'ulimit -Sn')
  note "login_nofile=$nofile"
  if [[ $nofile =~ ^[0-9]+$ ]] && [[ $nofile -ge 10240 ]]; then
    fail "the login's soft descriptor limit is $nofile: the daemon would not raise it, there would be no trampoline, and this scenario would test nothing"
  fi
  attach a mac-up1
  d1=$(the_daemon)
  exe=$(ps -o command= -p "$d1")
  note "daemon_command=${exe%% server*}"
  [[ $exe == "$W/bin/fil "* ]] || fail "the daemon is not the installed binary ($W/bin/fil): $exe"
  fc_detach a
  cp "$W/bin/fil" "$newfil"
  cat > "$W/stub.sh" << EOF
#!/bin/sh
if [ "\$1" = __fil-exec-nofile ]; then echo "\$*" >> "$mark"; fi
exec "$newfil" "\$@"
EOF
  chmod 755 "$W/stub.sh"
  # What a package manager does: the new file takes the old one's name (a rename, a new inode).
  mv -f "$W/stub.sh" "$W/bin/fil"
  attach b mac-up1
  fc_show_pid b
  d2=$(the_daemon)
  [[ $d1 == "$d2" ]] || fail "the daemon changed (pid $d1, now $d2): the test needs the old daemon to answer"
  ok "the reconnect reached the OLD daemon (pid $d1) through the replaced binary"
  attach c mac-up2
  fc_show_pid c
  [[ $(the_daemon) == "$d1" ]] || fail "the new session did not start in the old daemon"
  if [[ -s $mark ]]; then
    known_bug "current_exe() on macOS: the old daemon (pid $d1) started a NEW session through the REPLACED binary's trampoline ($(head -n 1 "$mark" | cut -d' ' -f1-3) ...). DESIGN/plan §1: freeze the trampoline's argument contract within a PROTOCOL_MAJOR, pinned by a test (fil-session::nofile, after M3 closes); U1's versioned install directories avoid the case for fil-installed copies"
    note "trampoline_via_replaced_path=yes"
  else
    note "trampoline_via_replaced_path=no"
    ok "the old daemon started the new session without touching the replaced file"
  fi
  fc_detach b
  fc_detach c
  # The consequence, if a release ever changed the trampoline's arguments: the replaced file refuses
  # them (status 127, as a trampoline given malformed arguments does). A new session in the old
  # daemon then has no shell.
  cat > "$W/stub.sh" << EOF
#!/bin/sh
if [ "\$1" = __fil-exec-nofile ]; then exit 127; fi
exec "$newfil" "\$@"
EOF
  chmod 755 "$W/stub.sh"
  mv -f "$W/stub.sh" "$W/bin/fil"
  SESSIONS+=(mac-up3)
  fc_start d -- --config "$CFG" macremote mac-up3
  sleep 4
  fc d send 'echo UP3-$((6*7))\r' > /dev/null || true
  if fc d wait --timeout 8 'UP3-42' > /dev/null 2>&1; then
    note "new_session_with_refusing_trampoline=works"
  else
    note "new_session_with_refusing_trampoline=NO_SHELL"
    log "with a trampoline that refuses its arguments the new session has no shell: $(fc_screen d | tail -n 3)"
  fi
}

# U1 has not landed (no `fil install`). When it does, this scenario is the place for the macOS ACL
# cases of the install bootstrap (docs/m4/plan.md, U1 "On macOS"): `fil install` to this Mac itself;
# an inheritable `allow` ACL on ~/.local/share/fil and an `allow` entry on ~/.local/share, both
# refused; an `allow` ACL together with an extended attribute (`ls -l` shows @, not +) on
# ~/.local/share/fil, on the version directory and on the binary, all refused, and an upload
# refused before anything is written; an extended attribute alone, accepted; a home with only the
# default deny entry, accepted.
# TODO(U1): write those cases once `fil install` exists.
scenario_install_acl() {
  if fil --help 2> /dev/null | grep -q '^  install '; then
    log "WARNING: this fil has an install command: U1 has landed and the macOS ACL cases are still to be written"
    note "SKIPPED: fil install exists now; write the macOS ACL cases (TODO U1)"
  else
    note "SKIPPED: no fil install yet (TODO U1)"
  fi
}

# Apple's real, unscaled /usr/libexec/tmp_cleaner (the script launchd's com.apple.tmp_cleaner runs
# at midnight: `find` on /tmp with `-atime/-mtime/-ctime +3`, regular files only) against a live
# daemon, as root, with the clock moved forward a day at a time (docs/m4/plan.md, TMP; DESIGN.md
# section 5.3). The change time cannot be set back, so the only honest test is to move the clock.
#
# The daemon is `tmpclean-daemon` (tests/macos/tmpclean): fil's daemon with TMP's `Keeping` seam
# given on the command line (the shipped binary has none), at the standard /tmp/fil-$UID paths,
# and a real `fil` client holds a session on it.
#   refresh   the daemon looks after its files every second: after each of five one-day steps
#             (day 1 to day 5, past the cleaner's limit of 3) the cleaner runs, and the directory, the
#             socket, the lock and start files must be the same files (same inodes: a repair
#             cannot pass for a survival), and the session must be alive.
#   control   no refresh, no repair: the same steps; the lock and start files must be GONE at day 4
#             and the socket must still be there (the cleaner takes regular files, never a socket),
#             and nothing before day 4. A cleaner that takes nothing proves nothing.
# It moves the system clock and runs the cleaner over all of /tmp as root, so it runs only on a
# throwaway GitHub runner (MACOS_MOVE_CLOCK=1, set by macos-check.yml) and puts everything it needs
# outside /tmp, under $HOME. It is active once fil-daemon has the seam (TMP, PR #96).
scenario_tmp_cleaners() {
  if ! grep -q 'pub struct Keeping' "$REPO_ROOT/crates/fil-daemon/src/paths.rs" 2> /dev/null; then
    note "SKIPPED: fil-daemon has no Keeping seam on this ref yet (TODO TMP, PR #96)"
    return 0
  fi
  if [[ -z ${MACOS_MOVE_CLOCK:-} ]]; then
    note "SKIPPED: moves the system clock and runs tmp_cleaner as root: a throwaway runner only (MACOS_MOVE_CLOCK=1)"
    return 0
  fi
  [[ ${GITHUB_ACTIONS:-} == true ]] || fail "MACOS_MOVE_CLOCK is set outside a GitHub Actions runner: refusing to move the clock"
  sudo -n true || fail "tmp_cleaners: no passwordless sudo"
  [[ -x /usr/libexec/tmp_cleaner ]] || fail "tmp_cleaners: no /usr/libexec/tmp_cleaner on this macOS"

  TS=$HOME/.filmac-tmpclean
  rm -rf "$TS"
  mkdir -p "$TS/bin" "$TS/pd"
  cp "$REPO_ROOT/Cargo.lock" "$REPO_ROOT/tests/macos/tmpclean/Cargo.lock"
  cargo build -q --release --manifest-path "$REPO_ROOT/tests/macos/tmpclean/Cargo.toml" \
    --target-dir "$REPO_ROOT/target/macos-tmpclean" || fail "tmpclean-daemon did not build"
  cp "$REPO_ROOT/target/macos-tmpclean/release/tmpclean-daemon" "$W/bin/fil" "$W/tools/ptydrive" "$TS/bin/"
  # Nothing the scenario runs is in /tmp from here on; the clients' state is in $TS/pd too.
  PATH=$TS/bin:$PATH
  export PATH
  PD=$TS/pd
  cat > "$TS/config.toml" << EOF
[defaults]
transport = "pipe"
pipe = ["$TS/bin/fil", "server", "--stdio"]
EOF
  CFG=$TS/config.toml
  mkdir -p "$OUT/run/$SCENARIO"
  # The earlier scenarios' daemon (it idles out after five minutes) must not be here: this scenario
  # runs a daemon of its own at the same paths. The setup refused to start beside one of the user's.
  local p
  for p in $(daemon_pids); do kill "$p" 2> /dev/null; done
  sleep 1
  CLOCK0=$(date +%s)
  CLOCK_SECONDS0=$SECONDS
  log "tmp_cleaner: $(date -u) on $(sw_vers -productVersion); the clock will move at most a day at a time and be put back"
  # Network time would fight the steps; best effort (it needs privileges the image may not grant).
  sudo -n systemsetup -setusingnetworktime off > /dev/null 2>&1 || true
  tmp_case refresh --every-ms 1000
  tmp_case control --every-ms 3600000 --no-refresh --no-dir-lock
  clock_restore
}

# The clock as it would be had it not been moved, plus DAYS days.
clock_target() { echo $((CLOCK0 + (SECONDS - CLOCK_SECONDS0) + $1 * 86400)); }

clock_set() {
  local want=$1 now
  CLOCK_MOVED=1
  sudo -n date -u "$(date -u -r "$want" +%m%d%H%M%Y.%S)" > /dev/null || fail "cannot set the clock"
  now=$(date +%s)
  [[ $((now - want)) -ge -10 && $((now - want)) -le 10 ]] || fail "the clock did not stay where it was set (wanted $want, it is $now): something else is setting it"
}

clock_restore() {
  [[ -n ${CLOCK_MOVED:-} ]] || return 0
  sudo -n date -u "$(date -u -r "$(clock_target 0)" +%m%d%H%M%Y.%S)" > /dev/null 2>&1
  sudo -n systemsetup -setusingnetworktime on > /dev/null 2>&1
  CLOCK_MOVED=
  log "the clock is back: $(date -u)"
  return 0
}

# ids: the daemon's directory, socket, lock and start file as device:inode, or gone.
tmp_ids() {
  local d=/tmp/fil-$(id -u) rev f out=""
  rev=$(fil --version | sed -n 's/.*wire \([0-9]*\)\.\([0-9]*\).*/\1-\2/p')
  for f in "$d" "$d/fil-$rev.sock" "$d/fil-$rev.lock" "$d/fil-$rev.start"; do
    out="$out $(stat -f '%d:%i' "$f" 2> /dev/null || echo gone)"
  done
  echo "${out# }"
}

# tmp_case MODE DAEMON_ARGS...: see scenario_tmp_cleaners.
tmp_case() {
  local mode=$1 d rev dpid sp before after day gone
  shift
  d=/tmp/fil-$(id -u)
  rev=$(fil --version | sed -n 's/.*wire \([0-9]*\)\.\([0-9]*\).*/\1-\2/p')
  [[ -z $(daemon_pids) ]] || fail "$mode: a daemon is running before the case starts: $(daemon_pids | tr '\n' ' ')"
  rm -rf "$d"
  tmpclean-daemon "$@" > "$OUT/run/$SCENARIO/daemon-$mode.log" 2>&1 &
  dpid=$!
  HELPERS+=("$dpid")
  local i
  for ((i = 0; i < 100; i++)); do [[ -S $d/fil-$rev.sock ]] && break; sleep 0.1; done
  [[ -S $d/fil-$rev.sock ]] || fail "$mode: the daemon did not come up: $(cat "$OUT/run/$SCENARIO/daemon-$mode.log")"
  # What a relay leaves when it starts a daemon (it did not start this one).
  : > "$d/fil-$rev.start"
  chmod 600 "$d/fil-$rev.start"
  attach a "mac-tmp-$mode"
  fc_show_pid a
  sp=$(shell_pid a)
  fc_detach a
  before=$(tmp_ids)
  log "$mode: before: $before (dir sock lock start); daemon pid $dpid, shell pid $sp"
  [[ $before != *gone* ]] || fail "$mode: a file the daemon should have is missing: $before"
  for day in 1 2 3 4 5; do
    clock_set "$(clock_target "$day")"
    sleep 4
    sudo -n /usr/libexec/tmp_cleaner > "$OUT/run/$SCENARIO/cleaner-$mode-day$day.log" 2>&1 || fail "$mode: tmp_cleaner failed on day $day: $(cat "$OUT/run/$SCENARIO/cleaner-$mode-day$day.log")"
    after=$(tmp_ids)
    log "$mode: day $day after the cleaner: $after ($(grep -c . "$OUT/run/$SCENARIO/cleaner-$mode-day$day.log") lines from the cleaner)"
    if [[ $mode == refresh ]]; then
      [[ $after == "$before" ]] || fail "refresh: day $day: the daemon's files changed (before: $before, after: $after)"
      kill -0 "$dpid" 2> /dev/null || fail "refresh: day $day: the daemon is gone"
      kill -0 "$sp" 2> /dev/null || fail "refresh: day $day: the session's shell is gone"
      fil --config "$CFG" ls local 2> /dev/null | grep -q "mac-tmp-$mode" || fail "refresh: day $day: fil ls does not list the session"
    else
      # Control: the cleaner takes the regular files once they are past 3 days (day 4), the socket never.
      local b_dir b_sock b_lock b_start a_dir a_sock a_lock a_start
      read -r b_dir b_sock b_lock b_start <<< "$before"
      read -r a_dir a_sock a_lock a_start <<< "$after"
      if [[ $day -ge 4 ]]; then
        [[ $a_lock == gone && $a_start == gone ]] || fail "control: day $day: the lock and start files should be gone and are not ($after): the cleaner removes nothing here, so the refresh case above proves nothing"
      else
        [[ $after == "$before" ]] || fail "control: day $day: files changed before the age limit ($before -> $after)"
      fi
      [[ $a_sock == "$b_sock" && $a_dir == "$b_dir" ]] || fail "control: day $day: the socket or the directory was touched ($before -> $after)"
      kill -0 "$dpid" 2> /dev/null || fail "control: day $day: the daemon is gone"
    fi
  done
  ok "$mode: five days of the real cleaner behaved as expected"
  note "tmp_cleaner_$mode=ok"
  # Back to the real time, then the daemon ends and its files go.
  clock_restore
  kill "$dpid" 2> /dev/null
  for ((i = 0; i < 100; i++)); do kill -0 "$dpid" 2> /dev/null || break; sleep 0.1; done
  fil --config "$CFG" kill local "mac-tmp-$mode" > /dev/null 2>&1 || true
  rm -rf "$d"
}

# ---------------------------------------------------------------------------------------------
# Setup, teardown, main

setup() {
  [[ $(uname -s) == Darwin ]] || { echo "macos-remote: this suite is for macOS (it is $(uname -s))." >&2; exit 2; }
  local bins=${FIL_BIN_DIR:-$REPO_ROOT/target/release} b
  for b in fil ptydrive fwdprobe; do
    [[ -x $bins/$b ]] || { echo "macos-remote: $bins/$b is missing; run it through 'just macos-remote' (it builds them)." >&2; exit 2; }
  done
  command -v python3 > /dev/null || { echo "macos-remote: python3 is needed." >&2; exit 2; }
  if [[ -z ${MACOS_REMOTE_SHARED:-} && -n $(daemon_pids) ]]; then
    echo "macos-remote: a fil daemon of yours is running (pid $(daemon_pids | tr '\n' ' ')). The suite uses /tmp/fil-$(id -u) like it; stop that daemon or set MACOS_REMOTE_SHARED=1." >&2
    exit 2
  fi
  rm -rf "$OUT"
  mkdir -p "$OUT"
  W=${MACOS_WORK:-$(mktemp -d /tmp/filmac.XXXXXX)}
  mkdir -p "$W/bin" "$W/tools" "$W/pd"
  # The installed fil (what the remote runs, and the client too), and the test tools.
  cp "$bins/fil" "$W/bin/fil"
  cp "$bins/ptydrive" "$bins/fwdprobe" "$W/tools/"
  PATH=$W/tools:$W/bin:$PATH
  export PATH
  PD=$W/pd
  # The daemon's pre-existing files are not ours to remove; remember what was there.
  sshd_up
  CFG=$W/config.toml
  cat > "$CFG" << EOF
[defaults]
transport = "pipe"

[host.macremote]
pipe = ["ssh", "-F", "$SSH_CONFIG", "-T", "--", "macremote", "$REMOTE_FIL", "server", "--stdio"]

[host.macremote.direct]
address = "127.0.0.1"
EOF
  {
    echo "W='$W'"
    echo "PD='$PD'"
    echo "REMOTE_FIL='$REMOTE_FIL'"
    echo "CFG='$CFG'"
    echo "SSH_PORT='$SSH_PORT'"
    echo "SSH_CONFIG='$SSH_CONFIG'"
    echo "SSH_KNOWN_HOSTS='$SSH_KNOWN_HOSTS'"
    echo "AUTH_FILE='$AUTH_FILE'"
    echo "MACOS_SSHD='${MACOS_SSHD:-user}'"
    echo "PATH='$PATH'"
    echo "export PATH"
  } > "$ENVFILE"
  note_setup
}

note_setup() {
  log "fil: $(fil --version)"
  log "macOS $(sw_vers -productVersion) $(uname -m), bash $BASH_VERSION"
}

teardown() {
  set +e
  local p
  # The daemon (and nothing else of ours) is the only one alive: the setup refused to start otherwise.
  for p in $(daemon_pids); do
    [[ -n ${MACOS_REMOTE_SHARED:-} ]] || kill "$p" 2> /dev/null
  done
  sshd_down
  if [[ -n ${W:-} && -z ${MACOS_KEEP_WORK:-} ]]; then rm -rf "$W"; fi
}

list() {
  printf '%s\n' "${SCENARIOS[@]}"
}

# Annotations for a GitHub run: a known finding is a warning, not a pass in silence.
annotate() {
  local f="$OUT/results-macos.tsv"
  [[ -n ${GITHUB_ACTIONS:-} && -f $f ]] || return 0
  awk -F'\t' '$2 == "PASS*" { printf "::warning title=macos-remote %s::a known finding was recorded (KNOWN-BUG in the logs and notes)\n", $1 }
    $4 ~ /SKIPPED/ { printf "::notice title=macos-remote %s::skipped, a placeholder for a package that has not landed\n", $1 }' "$f"
}

main() {
  case ${1:-} in
    --list) list; exit 0 ;;
    --one)
      SCENARIO=$2
      export SCENARIO
      set -euo pipefail
      trap scenario_cleanup EXIT
      "scenario_${2}"
      exit $?
      ;;
  esac
  local chosen=("$@") s
  [[ ${#chosen[@]} -gt 0 ]] || chosen=("${SCENARIOS[@]}")
  for s in ${chosen[@]+"${chosen[@]}"}; do
    declare -F "scenario_$s" > /dev/null || { echo "macos-remote: no scenario '$s' (try --list)" >&2; exit 2; }
  done
  trap teardown EXIT
  setup
  local rc=0
  run_scenarios macos "$HERE/remote.sh" "${chosen[@]}" || rc=$?
  print_summary macos
  annotate
  exit $rc
}

main "$@"
