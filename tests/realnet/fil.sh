#!/usr/bin/env bash
# Helpers for driving the REAL `fil` client on a pseudo-terminal (docs/m1/realnet.md). Sourced by
# the part scripts (ssh.sh, udp.sh, sysd.sh) after lib.sh. Never run.
#
# The client needs a terminal, so each client runs under `ptydrive serve` (crates/fil-testbins),
# which owns a PTY and a headless screen; the scenario types and reads through `ptydrive ctl`.
#
# The part script must define:
#   client_exec_on HOST CMD...   run CMD as the client user where the client lives; wait for it
#   client_bg_on   HOST CMD...   start CMD there, detached
# HOST names a container in the ssh part and is ignored by the parts with one client machine.
#
# Per scenario: fc_start NAME ... starts a client, fc_finish (an EXIT trap set by fc_start) saves
# every client's screen, raw output and log under $OUT/run and stops them.

PD=${PD:-/tmp/rn-pd}
FC_HOST=${FC_HOST:-client}
FC_NAMES=()
FC_N=0

# fc_on HOST: the clients started from now on run on HOST.
fc_on() { FC_HOST=$1; }

# known_bug DESCRIPTION: a finding about fil itself that the suite has caught. It is recorded in
# the results and the log, and does not fail the scenario (so one finding does not hide the
# results after it); REALNET_STRICT=1 makes it fail.
known_bug() {
  note "KNOWN-BUG: $1"
  log "KNOWN BUG: $1"
  [[ ${REALNET_STRICT:-0} != 1 ]] || fail "known bug (REALNET_STRICT=1): $1"
}

# fc_start NAME [-- FIL_ARGS...]: the client `fil FIL_ARGS...` in a PTY of 200 rows by 100
# columns, with debug logging into its own state directory (so its client.log is only its own).
# Extra environment for it: FC_ENV=(K=V ...). It runs on FC_HOST.
fc_start() {
  local name=$1 e
  shift
  [[ ${1:-} == -- ]] && shift
  printf -v "FC_WHERE_$name" '%s' "$FC_HOST"
  FC_NAMES+=("$name")
  trap fc_finish EXIT
  local envargs=(--env FIL_LOG=debug --env "XDG_STATE_HOME=$PD/state-${SCENARIO:-x}-$name")
  for e in ${FC_ENV[@]+"${FC_ENV[@]}"}; do envargs+=(--env "$e"); done
  client_bg_on "$FC_HOST" ptydrive serve --dir "$PD" "${envargs[@]}" "$name" -- fil "$@"
  # `ctl` retries the connection for 5 s by itself: a server that is not up by then never will be
  # (a missing binary, a bad directory).
  fc "$name" status > /dev/null 2>&1 || fail "ptydrive for $name did not start within 5 s"
}

# fc NAME VERB ARGS...: `ptydrive ctl`. The answer is on stdout; the status is its status.
fc() {
  local n=$1
  shift
  client_exec_on "$(fc_where "$n")" ptydrive ctl --dir "$PD" "$n" "$@"
}

# The host a client runs on (set by fc_start; the names are plain identifiers).
fc_where() {
  local v="FC_WHERE_$1"
  printf '%s' "${!v}"
}

# fc_wait NAME PATTERN [SECONDS]: the screen must show PATTERN (escapes allowed, see ptydrive).
fc_wait() {
  local out
  if out=$(fc "$1" wait --timeout "${3:-15}" "$2" 2>&1); then
    ok "$1: screen shows '$2'"
  else
    fail "$1: waiting for '$2': $out"
  fi
}

# fc_wait_raw NAME PATTERN [SECONDS]: like fc_wait, in everything received so far (escapes and all).
fc_wait_raw() {
  local out
  if out=$(fc "$1" wait --raw --timeout "${3:-15}" "$2" 2>&1); then
    ok "$1: output contains '$2'"
  else
    fail "$1: waiting for '$2' in the raw output: $out"
  fi
}

# fc_gone NAME PATTERN [SECONDS]: PATTERN must leave the screen.
fc_gone() {
  local out
  if out=$(fc "$1" gone --timeout "${3:-15}" "$2" 2>&1); then
    ok "$1: screen no longer shows '$2'"
  else
    fail "$1: '$2' did not go away: $out"
  fi
}

# fc_type NAME TEXT: keystrokes (C escapes: \r is Enter). fc_line NAME COMMAND: a line and Enter.
fc_type() { fc "$1" send "$2" > /dev/null || fail "$1: could not type"; }
fc_line() { fc_type "$1" "$2\\r"; }
# fc_answer NAME KEY: the answer to a yes/no question on the screen. The client ignores the
# keyboard until the typing has paused for 300 ms (a word typed ahead must not answer for the user).
# A prompt takes a lone key as its answer only after 300 ms with nothing else typed (DESIGN §4.2),
# so wait before the key and again after it, before typing anything more.
fc_answer() { sleep 0.5; fc_type "$1" "$2"; sleep 0.6; }

# fc_ready NAME [SECONDS]: the session's shell answers. (The typed text never contains the
# answer, so the echo of the keystrokes cannot satisfy it.)
fc_ready() {
  FC_N=$((FC_N + 1))
  fc_line "$1" "echo READY-\$((1000+$FC_N))"
  fc_wait "$1" "READY-$((1000 + FC_N))" "${2:-30}"
}

fc_screen() { fc "$1" screen; }

# fc_has NAME TEXT: the screen contains TEXT (fixed string).
fc_has() { fc_screen "$1" | grep -qF -- "$2"; }

# fc_seq_ok NAME PREFIX FROM TO: the screen has the lines PREFIX-FROM ... PREFIX-TO, each once,
# in order, and no others of that shape: nothing lost, nothing repeated.
fc_seq_ok() {
  fc_screen "$1" | awk -v p="$2" -v from="$3" -v to="$4" '
    { sub(/\r$/, "") }
    index($0, p "-") == 1 && substr($0, length(p) + 2) ~ /^[0-9]+$/ { got[++c] = substr($0, length(p) + 2) + 0 }
    END {
      ok = (c == to - from + 1)
      for (i = 1; i <= c && ok; i++) if (got[i] != from + i - 1) ok = 0
      if (!ok) { printf "seen:"; for (i = 1; i <= c; i++) printf " %d", got[i]; printf "\n" > "/dev/stderr" }
      exit !ok
    }'
}

# The session's shell pid, from the last "PID-<n>" the screen shows.
shell_pid() { fc_screen "$1" | grep -o 'PID-[0-9][0-9]*' | tail -n 1 | cut -d- -f2; }

# fc_show_pid NAME: types a line that prints the shell's pid, waits for it.
fc_show_pid() {
  FC_N=$((FC_N + 1))
  fc_line "$1" "echo PID-\$\$ DONE-\$((2000+$FC_N))"
  fc_wait "$1" "DONE-$((2000 + FC_N))"
}

# fc_pid NAME: the client's own pid (where it runs).
fc_pid() { fc "$1" status | sed -n 's/^running pid=\([0-9]*\).*/\1/p'; }

# fc_wait_exit NAME [SECONDS]: sets FC_CODE to the client's exit status; fails if it is still running.
fc_wait_exit() {
  local out
  out=$(fc "$1" wait-exit --timeout "${2:-15}" 2>&1) || fail "$1: the client did not exit: $out"
  FC_CODE=$(sed -n 's/^exited code=\([0-9]*\).*/\1/p' <<< "$out")
  ok "$1: exited ($out)"
}

# fc_expect_exit NAME CODE [SECONDS]
fc_expect_exit() {
  fc_wait_exit "$1" "${3:-15}"
  [[ $FC_CODE == "$2" ]] || fail "$1: exit status $FC_CODE, wanted $2: $(fc_screen "$1")"
}

# fc_detach NAME: Enter, then ~. ; the client must say so and exit 0.
fc_detach() {
  fc_type "$1" '\r~.'
  fc_expect_exit "$1" 0 15
  fc_has "$1" 'detached from session' || fail "$1: no 'detached from session' on the screen: $(fc_screen "$1")"
  ok "$1: detached"
}

# fc_log NAME: the client's log (JSON lines).
fc_log() { client_exec_on "$(fc_where "$1")" cat "$PD/state-${SCENARIO:-x}-$1/fil/client.log" 2> /dev/null || true; }

# fc_count_log NAME TEXT: how many log lines contain TEXT.
fc_count_log() { fc_log "$1" | grep -cF -- "$2" || true; }

# fc_log_ms NAME TEXT [N]: the time of the Nth (default first) log line containing TEXT, as
# milliseconds since the epoch; empty if none.
fc_log_ms() {
  local ts
  ts=$(fc_log "$1" | { grep -F -- "$2" || true; } | sed -n "${3:-1}p" | sed -n 's/.*"timestamp":"\([^"]*\)".*/\1/p')
  [[ -n $ts ]] || return 0
  # 2026-11-03T02:31:32.331204Z -> epoch milliseconds
  date -u -d "${ts%.*}Z" +%s | awk -v f="${ts#*.}" '{ printf "%d\n", $1 * 1000 + substr(f, 1, 3) }'
}

# The end of a scenario (an EXIT trap): keep what the clients showed, stop them.
fc_finish() {
  set +e
  local n
  mkdir -p "$OUT/run"
  for n in ${FC_NAMES[@]+"${FC_NAMES[@]}"}; do
    fc "$n" status > "$OUT/run/${SCENARIO:-x}-$n.status" 2>&1
    fc "$n" screen > "$OUT/run/${SCENARIO:-x}-$n.screen" 2>&1
    fc "$n" raw > "$OUT/run/${SCENARIO:-x}-$n.raw" 2>&1
    fc_log "$n" > "$OUT/run/${SCENARIO:-x}-$n.clientlog" 2>&1
    fc "$n" quit > /dev/null 2>&1
  done
  if declare -F scenario_cleanup > /dev/null; then scenario_cleanup; fi
}
