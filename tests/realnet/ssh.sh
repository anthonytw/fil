#!/usr/bin/env bash
# Tier 1, part 1: the pipe over real OpenSSH in Docker (docs/m1/realnet.md).
#
#   client --rn-c-- jump1 --rn-1-- jump2 --rn-2-- target
#
# Three internal Docker networks, so the target is reachable only through the jumps. All four are
# the same image (OpenSSH server and client, the real fil, the stand-ins). Run: ssh.sh [SCENARIO...]
set -uo pipefail
# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
# shellcheck source=fil.sh
source "$(dirname "${BASH_SOURCE[0]}")/fil.sh"

IMAGE=fil-realnet
HOSTS=(client client2 jump1 jump2 target)
NETWORKS=(rn-c rn-1 rn-2)
TSSH=/home/tester/.ssh
MIB=$((1 << 20))

# ---------------------------------------------------------------------------------------------
# Setup and teardown

ssh_setup() {
  command -v docker > /dev/null || { echo "realnet: docker is required" >&2; exit 2; }
  docker info > /dev/null 2>&1 || { echo "realnet: the docker daemon is not reachable" >&2; exit 2; }
  ssh_teardown quiet
  mkdir -p "$OUT/run" "$OUT/logs"

  "$REALNET_DIR/build.sh"
  local ctx=$OUT/ctx
  rm -rf "$ctx"
  mkdir -p "$ctx/bin"
  cp "$REALNET_DIR"/docker/* "$ctx/"
  cp "$BIN_DIR/pipe-echo" "$BIN_DIR/pipe-probe" "$BIN_DIR/quic-echo" \
    "$BIN_DIR/fil" "$BIN_DIR/ptydrive" "$BIN_DIR/sockprobe" "$ctx/bin/"
  docker build -q -t "$IMAGE" "$ctx" > /dev/null

  make_keys
  local n
  for n in "${NETWORKS[@]}"; do docker network create --internal "$n" > /dev/null; done
  start_host client rn-c
  start_host client2 rn-c
  start_host jump1 rn-c rn-1
  start_host jump2 rn-1 rn-2
  start_host target rn-2

  local h
  for h in "${HOSTS[@]}"; do
    docker cp "$SSH_KEYS/authorized_keys" "rn-$h:/etc/fil-realnet/authorized_keys"
    docker cp "$SSH_KEYS/ca.pub" "rn-$h:/etc/fil-realnet/ca.pub"
  done
  # client2 is a second machine for the takeover scenarios: the same user and the same keys.
  local c
  for c in client client2; do
    for f in id_user id_cert cert-valid.pub cert-expired.pub cert-wrongprincipal.pub \
      config config-ka config-cert-valid config-cert-nocert config-cert-expired \
      config-cert-wrongprincipal; do
      docker cp "$SSH_KEYS/$f" "rn-$c:$TSSH/$f"
    done
    docker exec -u root "rn-$c" sh -c "chown -R tester:tester $TSSH && chmod 600 $TSSH/*"
  done

  printf 'SSH_KEYS=%q\n' "$SSH_KEYS" > "$OUT/ssh.env"

  local i
  for ((i = 0; i < 60; i++)); do
    if cx ssh -T viajumps true > /dev/null 2>&1; then break; fi
    sleep 0.5
  done
  cx ssh -T viajumps true || { echo "realnet: the topology did not come up" >&2; exit 1; }
  log "topology up: client -> jump1 -> jump2 -> target"
  log "client $(cx ssh -V 2>&1), target openssh-server $(docker exec rn-target dpkg-query -W -f='${Version}' openssh-server)"
}

ssh_teardown() {
  local h
  if [[ ${1:-} != quiet ]]; then
    local u
    for h in "${HOSTS[@]}"; do
      docker cp "rn-$h:/var/log/sshd.log" "$OUT/logs/sshd-$h.log" > /dev/null 2>&1 || true
      # The real daemons' logs (the users that ran fil on each host).
      for u in fil noisy mallory; do
        docker cp "rn-$h:/home/$u/.local/state/fil/daemon.log" "$OUT/logs/fil-daemon-$h-$u.log" > /dev/null 2>&1 || true
      done
    done
  fi
  for h in "${HOSTS[@]}"; do docker rm -f "rn-$h" > /dev/null 2>&1 || true; done
  for h in "${NETWORKS[@]}"; do docker network rm "$h" > /dev/null 2>&1 || true; done
  if [[ -n ${SSH_KEYS:-} ]]; then rm -rf "$SSH_KEYS"; fi
}

# start_host NAME NETWORK...: a container reachable by NAME on each network.
start_host() {
  local name=$1 first=$2
  shift 2
  docker run -d --init --name "rn-$name" --hostname "$name" --network "$first" --network-alias "$name" "$IMAGE" > /dev/null
  local net
  for net in "$@"; do docker network connect --alias "$name" "$net" "rn-$name"; done
}

# Keys live in a temp directory for the run and are never written to the repository.
make_keys() {
  SSH_KEYS=$(mktemp -d)
  local k=$SSH_KEYS
  ssh-keygen -q -t ed25519 -N '' -C realnet-user -f "$k/id_user"
  ssh-keygen -q -t ed25519 -N '' -C realnet-cert-user -f "$k/id_cert"
  ssh-keygen -q -t ed25519 -N '' -C realnet-ca -f "$k/ca"
  cp "$k/id_user.pub" "$k/authorized_keys"
  chmod 644 "$k/authorized_keys" "$k/ca.pub"
  # Certificates for the key that is NOT in authorized_keys. Principal "fil" is the login name.
  sign_cert valid fil -5m:+1h
  sign_cert expired fil -2d:-1d
  sign_cert wrongprincipal someone-else -5m:+1h

  write_config "$k/config" "id_user" "" ""
  write_config "$k/config-ka" "id_user" "" "ServerAliveInterval 2
  ServerAliveCountMax 3"
  write_config "$k/config-cert-nocert" "id_cert" "" ""
  write_config "$k/config-cert-valid" "id_cert" "cert-valid.pub" ""
  write_config "$k/config-cert-expired" "id_cert" "cert-expired.pub" ""
  write_config "$k/config-cert-wrongprincipal" "id_cert" "cert-wrongprincipal.pub" ""
}

# sign_cert NAME PRINCIPAL VALIDITY: writes $SSH_KEYS/cert-NAME.pub
sign_cert() {
  local k=$SSH_KEYS
  cp "$k/id_cert.pub" "$k/cert-$1-src.pub"
  ssh-keygen -q -s "$k/ca" -I "realnet-$1" -n "$2" -V "$3" "$k/cert-$1-src.pub"
  mv "$k/cert-$1-src-cert.pub" "$k/cert-$1.pub"
  rm -f "$k/cert-$1-src.pub"
}

# write_config FILE IDENTITY CERTFILE EXTRA: the client's ssh config. Hosts: direct (jump1, the
# one host reachable without a jump), viajumps (target through jump1 and jump2), and users with
# noisy shells. Logging stays at INFO so the pre-auth banner reaches stderr.
write_config() {
  local file=$1 identity=$2 cert=$3 extra=$4
  {
    cat << 'EOF'
Host jump1 jump2
  User fil
Host direct
  HostName jump1
Host direct-noisy
  HostName jump1
  User noisy
Host direct-loud
  HostName jump1
  User loud
Host direct-flood
  HostName jump1
  User flood
Host direct-other
  HostName jump1
  User mallory
Host viajumps
  HostName target
  ProxyJump jump1,jump2
Host viajumps-cm
  HostName target
  ProxyJump jump1,jump2
  ControlMaster auto
  ControlPath /tmp/rn-cm-%C
  ControlPersist 120
Host *
  User fil
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  IdentitiesOnly yes
  BatchMode yes
  ConnectTimeout 10
EOF
    echo "  IdentityFile ~/.ssh/$identity"
    [[ -z $cert ]] || echo "  CertificateFile ~/.ssh/$cert"
    [[ -z $extra ]] || echo "  $extra"
  } > "$file"
}

# ---------------------------------------------------------------------------------------------
# Helpers for scenarios

cx() { docker exec -u tester rn-client "$@"; }

# run_drv NAME DRIVER_OPTS... -- SSH_ARGS...: run the pipe-echo driver in the client container,
# wait for it. Output in $LASTOUT (stdout) and ${LASTOUT%.out}.err. Returns the driver's status.
run_drv() {
  LASTOUT=$OUT/run/$SCENARIO-$1.out
  shift
  local rc=0
  cx pipe-echo client "$@" > "$LASTOUT" 2> "${LASTOUT%.out}.err" || rc=$?
  cat "$LASTOUT" >&2
  return $rc
}

# start_stream NAME "DRIVER_OPTS" SSH_ARGS...: a long-running stream in the background. Sets
# STREAM_PID (the host-side docker exec), STREAM_OUT and CHILD_PID (the ssh pid in the container).
start_stream() {
  STREAM_OUT=$OUT/run/$SCENARIO-$1.out
  local dopts=$2
  shift 2
  # shellcheck disable=SC2086
  cx pipe-echo client --mode stream --duration 120 --interval-ms 50 --size 4096 $dopts \
    -- ssh "$@" > "$STREAM_OUT" 2> "${STREAM_OUT%.out}.err" &
  STREAM_PID=$!
  wait_for_line "$STREAM_OUT" STREAMING 20 || fail "the stream never started: $(cat "$STREAM_OUT" "${STREAM_OUT%.out}.err")"
  CHILD_PID=$(kv "$(last_line "$STREAM_OUT" CHILD-PID)" pid)
  sleep 1.5
}

# The FAIL line a driver printed, as separate values.
read_fail() {
  FAIL_LINE=$(last_line "$1" FAIL)
  FAIL_AT=$(kv "$FAIL_LINE" at_ms || true)
  FAIL_REASON=$(kv "$FAIL_LINE" reason || true)
}

# wait_gone CONTAINER PROCESS_NAME SECONDS: the process must disappear (no orphans).
wait_gone() {
  local i
  for ((i = 0; i < $3 * 5; i++)); do
    if ! docker exec "$1" pgrep -x "$2" > /dev/null; then return 0; fi
    sleep 0.2
  done
  return 1
}

# wait_pid_gone CONTAINER PID SECONDS: the process must disappear. Unlike wait_gone this names one
# process, so ssh processes left over from earlier scenarios (s_netdown_no_detection abandons one)
# cannot be mistaken for ours.
wait_pid_gone() {
  local i
  for ((i = 0; i < $3 * 5; i++)); do
    if ! docker exec "$1" kill -0 "$2" 2> /dev/null; then return 0; fi
    sleep 0.2
  done
  return 1
}

# ssh_count: how many ssh client processes the client container has.
ssh_count() { docker exec rn-client pgrep -x -c ssh || true; }

accepted_count() { docker exec "rn-$1" grep -c "Accepted publickey for ${2:-fil}" /var/log/sshd.log || true; }

# ---------------------------------------------------------------------------------------------
# The same questions through fil's own PipeLink (pipe-probe at both ends) instead of the stand-in
# framing code: the link, not the driver, finds the break.

# run_probe NAME PROBE_OPTS... -- SSH_ARGS...: run the pipe-probe client in the client container
# and wait for it. Output in $LASTOUT (stdout) and ${LASTOUT%.out}.err. Returns its status.
run_probe() {
  LASTOUT=$OUT/run/$SCENARIO-$1.out
  shift
  local rc=0
  cx pipe-probe client "$@" > "$LASTOUT" 2> "${LASTOUT%.out}.err" || rc=$?
  cat "$LASTOUT" >&2
  return $rc
}

# start_probe NAME "PROBE_OPTS" SSH_ARGS...: a long-running probe in the background. Sets
# STREAM_PID, STREAM_OUT and CHILD_PID (the ssh pid in the container), like start_stream.
start_probe() {
  STREAM_OUT=$OUT/run/$SCENARIO-$1.out
  local popts=$2
  shift 2
  # shellcheck disable=SC2086
  cx pipe-probe client --duration 120 --interval-ms 50 --size 4096 $popts \
    -- ssh "$@" > "$STREAM_OUT" 2> "${STREAM_OUT%.out}.err" &
  STREAM_PID=$!
  wait_for_line "$STREAM_OUT" STREAMING 20 || fail "the probe never connected: $(cat "$STREAM_OUT" "${STREAM_OUT%.out}.err")"
  CHILD_PID=$(kv "$(last_line "$STREAM_OUT" CHILD-PID)" pid)
  sleep 1.5
}

# netdown_probe NAME "PROBE_OPTS" SSH_ARGS... -- OBSERVE_S: netdown, for the probe. Sets HUNG,
# DETECT_MS and FAIL_REASON.
netdown_probe() {
  local name=$1 popts=$2 observe ssh_args=()
  shift 2
  while [[ $1 != -- ]]; do
    ssh_args+=("$1")
    shift
  done
  observe=$2
  start_probe "$name" "$popts" "${ssh_args[@]}"
  docker network disconnect rn-2 rn-jump2
  local t0
  t0=$(now_ms)
  wait_exit "$STREAM_PID" "$observe"
  if [[ $WAIT_RC -eq 124 ]]; then
    HUNG=1
    DETECT_MS=-1
    FAIL_REASON=none
    docker exec rn-client pkill -f 'pipe-probe client' || true
    wait "$STREAM_PID" || true
  else
    HUNG=0
    read_fail "$STREAM_OUT"
    DETECT_MS=$((FAIL_AT - t0))
  fi
  docker network connect --alias jump2 rn-2 rn-jump2
  docker exec rn-target pkill -9 -f 'sshd: fil@[n]otty' || true
  sleep 1
}

# ---------------------------------------------------------------------------------------------
# Scenarios

s_topology() {
  local rc=0
  cx ssh -o ConnectTimeout=4 target true 2> /dev/null || rc=$?
  assert "the target cannot be reached directly by name (ssh rc=$rc)" test "$rc" -ne 0
  local tip jip
  tip=$(docker inspect -f '{{(index .NetworkSettings.Networks "rn-2").IPAddress}}' rn-target)
  jip=$(docker inspect -f '{{(index .NetworkSettings.Networks "rn-2").IPAddress}}' rn-jump2)
  rc=0
  cx ssh -o ConnectTimeout=4 "fil@$tip" true 2> /dev/null || rc=$?
  assert "nor by address (ssh rc=$rc)" test "$rc" -ne 0
  local out
  out=$(cx ssh -T viajumps 'echo reached; hostname')
  assert "through the jumps it answers" grep -q reached <<< "$out"
  assert "the target saw the login coming from jump2" \
    docker exec rn-target grep -q "Accepted publickey for fil from $jip " /var/log/sshd.log
  assert "jump1 and jump2 both authenticated a hop" \
    test "$(accepted_count jump1)" -ge 1 -a "$(accepted_count jump2)" -ge 1
}

# Common checks for a clean byte-exact bulk echo.
check_bulk() { # check_bulk LABEL BYTES RC
  local line
  line=$(last_line "$LASTOUT" RESULT)
  assert "$1: the driver exited 0" test "$3" -eq 0
  assert "$1: all $2 bytes echoed byte-exact" test "$(kv "$line" bytes)" = "$2"
  assert "$1: ok=true" test "$(kv "$line" ok)" = true
}

s_direct() {
  local rc=0 bytes=$((8 * MIB))
  run_drv direct --bytes "$bytes" -- ssh -T direct pipe-echo || rc=$?
  check_bulk direct "$bytes" "$rc"
  local line pre
  line=$(last_line "$LASTOUT" RESULT)
  pre=$(last_line "$LASTOUT" PREAMBLE)
  assert "a clean login skips nothing" test "$(kv "$pre" skipped_len)" = 0
  assert "sshd's pre-auth banner went to stderr, not the stream" \
    grep -q 'realnet test banner' "${LASTOUT%.out}.err"
  note "mbps=$(kv "$line" mbps)" "preamble_after_ms=$(kv "$pre" after_ms)"
}

s_jumps() {
  local rc=0 bytes=$((8 * MIB))
  run_drv jumps --bytes "$bytes" -- ssh -T viajumps pipe-echo || rc=$?
  check_bulk jumps "$bytes" "$rc"
  local line pre
  line=$(last_line "$LASTOUT" RESULT)
  pre=$(last_line "$LASTOUT" PREAMBLE)
  note "mbps=$(kv "$line" mbps)" "preamble_after_ms=$(kv "$pre" after_ms)"
}

s_controlmaster() {
  cx ssh -O exit viajumps-cm 2> /dev/null || true
  local n0 n1 n2 rc=0
  n0=$(accepted_count target)
  run_drv cm1 --bytes $((2 * MIB)) -- ssh -T viajumps-cm pipe-echo || rc=$?
  check_bulk "first session (starts the master)" $((2 * MIB)) "$rc"
  local t1 t2
  t1=$(kv "$(last_line "$LASTOUT" PREAMBLE)" after_ms)
  n1=$(accepted_count target)
  rc=0
  run_drv cm2 --bytes $((2 * MIB)) -- ssh -T viajumps-cm pipe-echo || rc=$?
  check_bulk "second session (reuses it)" $((2 * MIB)) "$rc"
  t2=$(kv "$(last_line "$LASTOUT" PREAMBLE)" after_ms)
  n2=$(accepted_count target)
  assert "the first session made one login on the target ($n0 -> $n1)" test "$n1" -eq $((n0 + 1))
  assert "the second made none: it shared the master ($n1 -> $n2)" test "$n2" -eq "$n1"
  assert "the master answers 'ssh -O check'" cx ssh -O check viajumps-cm
  cx ssh -O exit viajumps-cm 2> /dev/null || true
  note "first_preamble_ms=$t1" "second_preamble_ms=$t2"
}

s_certs() {
  local rc=0 bytes=$((2 * MIB))
  run_drv cert-valid --bytes "$bytes" -- ssh -F "$TSSH/config-cert-valid" -T viajumps pipe-echo || rc=$?
  check_bulk "valid certificate" "$bytes" "$rc"
  assert "the target logged a certificate login" \
    docker exec rn-target grep -q 'Accepted publickey for fil .*ED25519-CERT' /var/log/sshd.log
  assert "the jump hosts trusted the same CA" \
    docker exec rn-jump2 grep -q 'Accepted publickey for fil .*ED25519-CERT' /var/log/sshd.log
  local name
  for name in nocert expired wrongprincipal; do
    rc=0
    run_drv "cert-$name" --bytes 4096 --dead-after 15 \
      -- ssh -F "$TSSH/config-cert-$name" -T viajumps pipe-echo || rc=$?
    assert "$name: refused, and the driver says so rather than hanging (rc=$rc)" test "$rc" -eq 4
    assert "$name: ssh itself exited 255" \
      grep -q 'child_status=255' "$LASTOUT"
  done
}

s_noisy() {
  local rc=0 line pre
  run_drv noisy --bytes $((2 * MIB)) -- ssh -T direct-noisy pipe-echo || rc=$?
  check_bulk "noisy .bashrc" $((2 * MIB)) "$rc"
  pre=$(last_line "$LASTOUT" PREAMBLE)
  assert "the scanner reported the skipped text" grep -q 'NOISY-BASHRC' <<< "$pre"
  assert "and it was non-empty" test "$(kv "$pre" skipped_len)" -gt 0
  assert "control characters were stripped for display" \
    bash -c '! grep -a -q "$(printf "\033")" "$1"' _ "$LASTOUT"
  note "noisy_skipped=$(kv "$pre" skipped_len)"

  rc=0
  run_drv loud --bytes $((1 * MIB)) -- ssh -T direct-loud pipe-echo || rc=$?
  check_bulk "60000 bytes of noise (under the bound)" $((1 * MIB)) "$rc"
  line=$(last_line "$LASTOUT" PREAMBLE)
  assert "all 60000 noise bytes were skipped" test "$(kv "$line" skipped_len)" -ge 60000
  note "loud_skipped=$(kv "$line" skipped_len)"
}

s_flood() {
  local rc=0 line
  run_drv flood --bytes $((1 * MIB)) -- ssh -T direct-flood pipe-echo || rc=$?
  assert "the driver gave up with 'no preamble' (rc=$rc)" test "$rc" -eq 3
  line=$(last_line "$LASTOUT" PREAMBLE-ERROR)
  assert "the bound is the 64 KiB limit" test "$(kv "$line" limit)" -eq 65536
  assert_num "memory stayed bounded (skipped bytes held)" "$(kv "$line" skipped_len)" -le 262144
  assert_num "it failed fast (ms)" "$(kv "$line" after_ms)" -lt 5000
  assert "the remote program was cleaned up (no orphan)" wait_gone rn-jump1 pipe-echo 5
  note "gave_up_after_ms=$(kv "$line" after_ms)" "held_bytes=$(kv "$line" skipped_len)"
}

s_hostile_names() {
  local rc=0 line
  run_drv names --mode names -- ssh -T viajumps pipe-echo || rc=$?
  line=$(last_line "$LASTOUT" RESULT)
  assert "every hostile name came back byte-exact (rc=$rc)" test "$rc" -eq 0
  assert "all of them" test "$(kv "$line" frames)" -ge 15
  local h
  for h in jump1 jump2 target; do
    assert "$h: nothing ran (no canary file)" bash -c '! docker exec "rn-$1" test -e /tmp/fil-canary' _ "$h"
  done
  # Positive control: the same text interpolated into the remote command line DOES run.
  cx ssh -T viajumps 'true; touch /tmp/fil-canary-naive'
  assert "control: naive interpolation would have run it" docker exec rn-target test -e /tmp/fil-canary-naive
  # What the remote actually ran: exactly the fixed command, no arguments.
  start_stream names "--duration 5" -T viajumps pipe-echo
  local args
  args=$(docker exec rn-target ps -eo args=)
  assert "the remote command line is exactly 'pipe-echo'" grep -qx 'pipe-echo' <<< "$args"
  assert "no session name appears in any remote process" bash -c '! grep -q canary <<< "$1"' _ "$args"
  wait_exit "$STREAM_PID" 15
}

s_kill_ssh_client() {
  start_stream k1 "" -T viajumps pipe-echo
  local ts
  ts=$(docker exec -u tester rn-client sh -c "date +%s%3N; kill -9 $CHILD_PID" | head -n 1)
  wait_exit "$STREAM_PID" 20
  assert "the driver exited with its link-failure status, not a hang (rc=$WAIT_RC)" test "$WAIT_RC" -eq 4
  read_fail "$STREAM_OUT"
  local ms=$((FAIL_AT - ts))
  assert "reason is a clean close (got $FAIL_REASON)" bash -c '[[ $1 == eof || $1 == read-error || $1 == write-error ]]' _ "$FAIL_REASON"
  assert_num "detected within 3 s (ms)" "$ms" -lt 3000
  assert "the remote pipe-echo exited too (no orphan)" wait_gone rn-target pipe-echo 10
  note "detect_ms=$ms" "reason=$FAIL_REASON"
}

s_kill_sshd_child() {
  start_stream k2 "" -T viajumps pipe-echo
  local ts
  ts=$(docker exec rn-target sh -c 'date +%s%3N; pkill -9 -f "sshd: fil@[n]otty"' | head -n 1)
  wait_exit "$STREAM_PID" 20
  assert "the driver exited with its link-failure status (rc=$WAIT_RC)" test "$WAIT_RC" -eq 4
  read_fail "$STREAM_OUT"
  local ms=$((FAIL_AT - ts))
  assert "reason is a clean close (got $FAIL_REASON)" bash -c '[[ $1 == eof || $1 == read-error || $1 == write-error ]]' _ "$FAIL_REASON"
  assert_num "detected within 3 s (ms)" "$ms" -lt 3000
  assert "the remote pipe-echo exited (no orphan)" wait_gone rn-target pipe-echo 10
  note "detect_ms=$ms" "reason=$FAIL_REASON"
}

# Cut jump2 off the network behind the target: a silent black hole, no reset, no FIN.
# netdown NAME "DRIVER_OPTS" SSH_ARGS... -- OBSERVE_S: sets HUNG (0/1), DETECT_MS, FAIL_REASON.
netdown() {
  local name=$1 dopts=$2 observe ssh_args=()
  shift 2
  while [[ $1 != -- ]]; do
    ssh_args+=("$1")
    shift
  done
  observe=$2
  start_stream "$name" "$dopts" "${ssh_args[@]}"
  docker network disconnect rn-2 rn-jump2
  local t0
  t0=$(now_ms)
  wait_exit "$STREAM_PID" "$observe"
  if [[ $WAIT_RC -eq 124 ]]; then
    HUNG=1
    DETECT_MS=-1
    FAIL_REASON=none
    docker exec rn-client pkill -f 'pipe-echo client' || true
    wait "$STREAM_PID" || true
  else
    HUNG=0
    read_fail "$STREAM_OUT"
    DETECT_MS=$((FAIL_AT - t0))
  fi
  docker network connect --alias jump2 rn-2 rn-jump2
  docker exec rn-target pkill -9 -f 'sshd: fil@[n]otty' || true
  sleep 1
}

s_netdown_app_heartbeat() {
  # No ssh keepalive. The driver's own 5 s silence rule (fil's DEAD_AFTER) is the only detector.
  netdown hb "--dead-after 5" -T viajumps pipe-echo -- 25
  assert "detected, not hung" test "$HUNG" -eq 0
  assert "as silence (got $FAIL_REASON)" test "$FAIL_REASON" = silence
  assert_num "after about DEAD_AFTER (ms)" "$DETECT_MS" -ge 3500
  assert_num "and not much later (ms)" "$DETECT_MS" -le 9000
  note "detect_ms=$DETECT_MS" "dead_after=5s"
}

s_netdown_ssh_keepalive() {
  # ssh's own keepalive only: ServerAliveInterval 2, ServerAliveCountMax 3 (config-ka).
  netdown ka "--dead-after 0" -F "$TSSH/config-ka" -T viajumps pipe-echo -- 40
  assert "detected, not hung" test "$HUNG" -eq 0
  assert "as the connection closing (got $FAIL_REASON)" test "$FAIL_REASON" = eof
  assert_num "after about interval x (count+1) = 8 s (ms)" "$DETECT_MS" -ge 5000
  assert_num "and well before a minute (ms)" "$DETECT_MS" -le 20000
  note "detect_ms=$DETECT_MS" "ServerAliveInterval=2" "CountMax=3"
}

s_netdown_no_detection() {
  # Control: neither keepalive nor a heartbeat. Informational: records how long we watched.
  netdown none "--dead-after 0" -T viajumps pipe-echo -- 25
  if [[ $HUNG -eq 1 ]]; then
    note "hung_for_over_s=25" "(no detector: the client waits forever)"
  else
    note "unexpectedly_detected_ms=$DETECT_MS" "reason=$FAIL_REASON"
  fi
}

s_pipelink_echo() {
  # PipeLink at both ends, through a clean login, a noisy one, and the two jump hosts.
  local rc=0 line
  run_probe clean --duration 4 -- ssh -T viajumps pipe-probe || rc=$?
  assert "clean login: the probe exited 0 (rc=$rc)" test "$rc" -eq 0
  line=$(last_line "$LASTOUT" RESULT)
  assert "clean login: ok=true" test "$(kv "$line" ok)" = true
  assert "clean login: nothing skipped" test "$(kv "$(last_line "$LASTOUT" BANNER)" len)" = 0
  assert "ssh was given its keepalive options" \
    grep -q 'ServerAliveInterval=2,-o,ServerAliveCountMax=3' "$LASTOUT"
  note "rtt_ms=$(kv "$line" rtt_ms)"
  rc=0
  run_probe noisy --duration 4 -- ssh -T direct-noisy pipe-probe || rc=$?
  assert "noisy login: the probe exited 0 (rc=$rc)" test "$rc" -eq 0
  line=$(last_line "$LASTOUT" BANNER)
  assert "noisy login: the banner was kept" grep -q 'NOISY-BASHRC' <<< "$line"
  assert "noisy login: control characters were stripped" \
    bash -c '! grep -a -q "$(printf "\033")" "$1"' _ "$LASTOUT"
  note "noisy_banner_len=$(kv "$line" len)"
}

s_pipelink_flood() {
  # 100000 bytes of shell noise: the preamble bound holds and the error says why.
  local rc=0 line before
  before=$(ssh_count)
  run_probe flood --duration 4 -- ssh -T direct-flood pipe-probe || rc=$?
  assert "the probe gave up with a connect error (rc=$rc)" test "$rc" -eq 3
  line=$(last_line "$LASTOUT" PREAMBLE-ERROR)
  assert "the bound is the 64 KiB limit" test "$(kv "$line" limit)" -eq 65536
  assert_num "memory stayed bounded (skipped bytes held)" "$(kv "$line" skipped_len)" -le 262144
  assert_num "it failed fast (ms)" "$(kv "$line" after_ms)" -lt 5000
  assert "the remote program was cleaned up (no orphan)" wait_gone rn-jump1 pipe-probe 5
  sleep 1
  assert "and so was the local ssh (count $before before, $(ssh_count) after)" \
    test "$(ssh_count)" -le "$before"
  note "gave_up_after_ms=$(kv "$line" after_ms)"
}

s_pipelink_kill_ssh() {
  start_probe kill "" -T viajumps pipe-probe
  local ts
  ts=$(docker exec -u tester rn-client sh -c "date +%s%3N; kill -9 $CHILD_PID" | head -n 1)
  wait_exit "$STREAM_PID" 20
  assert "the probe exited with its link-failure status (rc=$WAIT_RC)" test "$WAIT_RC" -eq 4
  read_fail "$STREAM_OUT"
  local ms=$((FAIL_AT - ts))
  assert "the link went down as an I/O failure (got $FAIL_REASON)" test "$FAIL_REASON" = io
  assert "and the reason names the signal" grep -q 'detail=.*signal' "$STREAM_OUT"
  assert_num "detected within 3 s (ms)" "$ms" -lt 3000
  assert "the remote pipe-probe exited too (no orphan)" wait_gone rn-target pipe-probe 10
  note "detect_ms=$ms"
}

s_pipelink_netdown_heartbeat() {
  # The black hole, found by fil alone: ssh keepalive is off for this run, so only PipeLink's
  # own heartbeat can notice. Compare s_netdown_no_detection, where nothing does.
  netdown_probe hb "--no-ssh-keepalive --dead-after 5" -T viajumps pipe-probe -- 25
  assert "detected, not hung" test "$HUNG" -eq 0
  assert "by fil's liveness clock (got $FAIL_REASON)" test "$FAIL_REASON" = dead
  assert "after it reported Degraded" grep -q 'STATE state=Degraded' "$STREAM_OUT"
  assert_num "after about dead_after (ms)" "$DETECT_MS" -ge 3500
  assert_num "and not much later (ms)" "$DETECT_MS" -le 9000
  assert "the dead link's ssh ($CHILD_PID) was killed, not left behind" \
    wait_pid_gone rn-client "$CHILD_PID" 5
  note "detect_ms=$DETECT_MS" "dead_after=5s" "ssh_keepalive=off"
}

s_pipelink_netdown_keepalive() {
  # The default command, with the ServerAlive options fil adds (2 s, so ssh gives up at 6 to 8 s): fil's 5 s wins,
  # kills ssh at once, and the backstop never has to fire.
  netdown_probe ka "--dead-after 5" -T viajumps pipe-probe -- 25
  assert "detected, not hung" test "$HUNG" -eq 0
  assert "by fil's liveness clock, before ssh's (got $FAIL_REASON)" test "$FAIL_REASON" = dead
  assert_num "after about dead_after (ms)" "$DETECT_MS" -ge 3500
  assert_num "and before ssh's earliest give-up, 6 s (ms)" "$DETECT_MS" -le 6000
  assert "the dead link's ssh ($CHILD_PID) was killed, not left behind" \
    wait_pid_gone rn-client "$CHILD_PID" 5
  note "detect_ms=$DETECT_MS" "dead_after=5s" "ssh_keepalive=2s"
}

# ---------------------------------------------------------------------------------------------
# The real `fil` (client, relay, daemon) over the same real OpenSSH. The client runs on a PTY
# under ptydrive (fil.sh); the daemon is started by the first relay on each host, as in life.
# Session names are unique per scenario, and each scenario kills its sessions.

# fil.sh wants these: run as the client user in container HOST (client or client2).
client_exec_on() {
  local h=$1
  shift
  docker exec -u tester "rn-$h" "$@"
}
client_bg_on() {
  local h=$1
  shift
  docker exec -d -u tester "rn-$h" "$@"
}

# fk HOST SESSION: kill a session, whatever state it is in.
fk() { cx fil kill "$1" "$2" > /dev/null 2>&1 || true; }

# Attach, run something, detach with ~. ; list; reattach and see the earlier output, in the same
# shell; the exit status and the messages of each step.
f_attach_detach() {
  local t0
  t0=$(now_ms)
  fc_start a -- direct ad
  fc_ready a
  note "attach_to_shell_ms=$(($(now_ms) - t0))"
  fc_line a 'echo FIL-$((40+2))'
  fc_wait a FIL-42
  fc_show_pid a
  local pid1
  pid1=$(shell_pid a)
  [[ -n $pid1 ]] || fail "no shell pid on the screen: $(fc_screen a)"
  fc_detach a

  # The daemon notices the closed link a moment after the client has gone; allow it a few seconds.
  local out i t0
  t0=$(now_ms)
  for ((i = 0; i < 50; i++)); do
    out=$(cx fil ls direct)
    awk '$1 == "ad" && $3 == "-" { f = 1 } END { exit !f }' <<< "$out" && break
    sleep 0.1
  done
  if awk '$1 == "ad" && $3 == "-" { f = 1 } END { exit !f }' <<< "$out"; then
    note "listed_as_detached_after_ms=$(($(now_ms) - t0))"
    ok "fil ls lists the session as detached"
  else
    log "fil ls, 5 s after ~. : $out"
    known_bug "after ~. over ssh, fil ls still lists the client as attached (the daemon logs LinkDown, not a detach)"
  fi

  fc_start b -- direct ad
  fc_wait b FIL-42 20
  fc_has b "PID-$pid1" || fail "the earlier output (the shell pid) is not on the second screen"
  fc_show_pid b
  [[ $(shell_pid b) == "$pid1" ]] || fail "the reattached shell is a different process: $(shell_pid b) vs $pid1"
  ok "reattached: earlier output visible, same shell (pid $pid1)"
  fc_detach b
  fk direct ad
}

# fil ls, fil detach (the attached client is told), fil kill (of an attached session; the client
# reports how it ended), and the errors for a session that is gone.
f_ls_kill_detach() {
  fc_start a -- direct lkd
  fc_ready a
  local out
  out=$(cx fil ls direct)
  awk '$1 == "lkd" && $3 == "client" { f = 1 } END { exit !f }' <<< "$out" || fail "fil ls: 'lkd' should show the attached client: $out"
  ok "fil ls shows who is attached"

  out=$(cx fil detach direct lkd)
  [[ $out == *'detached session "lkd"'* ]] || fail "fil detach said: $out"
  fc_wait a 'was detached from elsewhere' 20
  fc_type a n
  fc_expect_exit a 1
  out=$(cx fil ls direct)
  awk '$1 == "lkd" && $3 == "-" { f = 1 } END { exit !f }' <<< "$out" || fail "fil ls after detach: $out"
  ok "after fil detach the session is listed as detached, and it kept running"

  fc_start b -- direct lkd
  fc_ready b
  out=$(cx fil kill direct lkd)
  [[ $out == *'killed session "lkd"'* ]] || fail "fil kill said: $out"
  fc_wait_exit b 20
  note "exit_after_kill=$FC_CODE"
  [[ $FC_CODE -ge 128 ]] || fail "the client of a killed session exited $FC_CODE, wanted 128+signal: $(fc_screen b)"
  fc_has b 'lkd' || fail "no message about the session: $(fc_screen b)"
  out=$(cx fil ls direct)
  if grep -q '^lkd ' <<< "$out"; then fail "the killed session is still listed: $out"; fi
  local rc=0
  out=$(cx fil kill direct lkd 2>&1) || rc=$?
  [[ $rc -ne 0 && $out == *'no session "lkd"'* ]] || fail "killing it again: rc=$rc $out"
  ok "killing a session that is gone fails with a clear message"
}

# A second machine attaches: it gets the earlier output, and the first client is told, by whom.
f_takeover() {
  fc_on client
  fc_start a -- viajumps tk
  fc_ready a
  fc_line a 'echo TAKEN-$((6*7))'
  fc_wait a TAKEN-42
  fc_on client2
  fc_start b -- viajumps tk
  fc_wait b TAKEN-42 30
  fc_wait a 'was taken over' 20
  fc_wait a 'by client2' 5
  local out
  out=$(cx fil ls viajumps)
  awk '$1 == "tk" && $3 == "client2" { f = 1 } END { exit !f }' <<< "$out" || fail "fil ls should show client2 as attached: $out"
  fc_type a n
  fc_expect_exit a 1
  fc_has a 'taken over by client2' || fail "no final message on the first client: $(fc_screen a)"
  fc_line b 'echo OWNER-$((3+4))'
  fc_wait b OWNER-7
  fc_detach b
  fk viajumps tk
}

# ... and the first client takes it back: the second one is told in turn.
f_takeover_back() {
  fc_on client
  fc_start a -- viajumps tb
  fc_ready a
  fc_line a 'echo FIRST-$((6*7))'
  fc_wait a FIRST-42
  fc_on client2
  fc_start b -- viajumps tb
  fc_wait b FIRST-42 30
  fc_wait a 'Take it back' 20
  fc_type a y
  fc_wait b 'was taken over' 20
  fc_wait b 'by client' 5
  fc_line a 'echo BACK-$((2*8))'
  fc_wait a BACK-16 20
  fc_has a FIRST-42 || fail "the first client lost the earlier output after taking the session back: $(fc_screen a)"
  fc_type b n
  fc_expect_exit b 1
  fc_detach a
  fk viajumps tb
}

# kill -9 the client's ssh in the middle of output: the session resumes with nothing lost.
f_kill_ssh() {
  fc_start a -- viajumps ks
  fc_ready a
  fc_line a 'for i in $(seq 1 60); do echo TICK-$i; sleep 0.25; done'
  fc_wait a TICK-8 20
  local pid ssh1 ssh2 t0 i
  pid=$(fc_pid a)
  ssh1=$(cx pgrep -P "$pid" -x ssh | head -n 1 || true)
  [[ -n $ssh1 ]] || fail "fil ($pid) has no ssh child: $(cx ps -ef)"
  t0=$(now_ms)
  cx kill -9 "$ssh1"
  ssh2=
  for ((i = 0; i < 300; i++)); do
    ssh2=$(cx pgrep -P "$pid" -x ssh | head -n 1 || true)
    [[ -n $ssh2 && $ssh2 != "$ssh1" ]] && break
    ssh2=
    sleep 0.1
  done
  [[ -n $ssh2 ]] || fail "fil did not start a new ssh: $(fc_screen a)"
  note "new_ssh_ms=$(($(now_ms) - t0))"
  fc_wait a TICK-60 40
  fc_seq_ok a TICK 1 60 || fail "output was lost or repeated across the ssh kill: $(fc_screen a)"
  ok "TICK-1 to TICK-60 each once, in order"
  if fc_has a 'skipped'; then fail "a gap marker: the resume was a snapshot, not a resume: $(fc_screen a)"; fi
  ok "no gap marker"
  fc_detach a
  fk viajumps ks
}

# kill -9 the daemon: the next resume says the session was replaced, and a fresh attach works.
f_daemon_restart() {
  fc_start a -- viajumps dr1
  fc_ready a
  fc_show_pid a
  local old dpid t0
  old=$(shell_pid a)
  dpid=$(docker exec rn-target pgrep -u fil -f 'fil server --daemon' | head -n 1 || true)
  [[ -n $dpid ]] || fail "no daemon on the target"
  t0=$(now_ms)
  docker exec rn-target kill -9 "$dpid"
  fc_wait a 'is a different session now' 60
  note "replaced_message_ms=$(($(now_ms) - t0))"
  fc_type a n
  fc_expect_exit a 1
  fc_has a 'daemon restarted' || fail "no final message: $(fc_screen a)"

  # And the other answer: a fresh attach, a new shell.
  fc_start c -- viajumps dr2
  fc_ready c
  fc_show_pid c
  dpid=$(docker exec rn-target pgrep -u fil -f 'fil server --daemon' | head -n 1 || true)
  docker exec rn-target kill -9 "$dpid"
  fc_wait c 'Start a fresh attach' 60
  fc_type c y
  fc_show_pid c
  local new
  new=$(shell_pid c)
  [[ -n $new ]] || fail "no shell after the fresh attach: $(fc_screen c)"
  fc_line c 'echo FRESH-$((6*7))'
  fc_wait c FRESH-42
  ok "a fresh attach after the daemon restart gives a working shell (pid $new; the first session's was $old)"
  fc_detach c
  fk viajumps dr2
}

# A login shell that prints at startup: the banner warning is shown, and the session works.
f_noisy() {
  fc_start a -- direct-noisy nz
  # The warning is written before the session's screen is drawn, and the first snapshot pushes it
  # into the scrollback, so look at everything the client printed rather than at the screen.
  fc_wait_raw a 'your shell printed this before fil started' 30
  fc_wait_raw a 'fil:   | NOISY-BASHRC: welcome back' 5
  fc_ready a
  local err
  err=$(cx fil ls direct-noisy 2>&1 > /dev/null)
  [[ $err == *'the remote shell printed this before fil started'* && $err == *NOISY-BASHRC* ]] || fail "fil ls did not show the banner: $err"
  ok "fil ls shows the banner too"
  fc_detach a
  fk direct-noisy nz
}

# Through both jumps, for real: the shell is on the target, and the target's sshd saw jump2.
f_proxyjump() {
  local before
  before=$(accepted_count target)
  fc_start a -- viajumps pj
  fc_ready a
  fc_line a 'echo ON-$(hostname)'
  fc_wait a ON-target
  [[ $(accepted_count target) -gt $before ]] || fail "the target's sshd saw no new login"
  local ps
  ps=$(cx ps -eo args | grep -F 'fil server --stdio' | grep -F viajumps || true)
  [[ -n $ps ]] || fail "no ssh to viajumps running 'fil server --stdio': $(cx ps -ef)"
  ok "ssh viajumps ... fil server --stdio is the pipe command"
  fc_detach a
  fk viajumps pj
}

# Sessions end with their processes: no systemd here, so this is the process-tree fallback.
f_tree_scope() {
  fc_start a -- viajumps tr
  fc_ready a
  fc_line a 'sleep 777 & echo BG-$! DONE-$((1+1))'
  fc_wait a DONE-2
  local bg
  bg=$(fc_screen a | grep -o 'BG-[0-9][0-9]*' | tail -n 1 | cut -d- -f2)
  [[ -n $bg ]] || fail "no background pid: $(fc_screen a)"
  docker exec rn-target kill -0 "$bg" || fail "the background job is not running"
  if docker exec rn-target cat "/proc/$bg/cgroup" | grep -q 'fil-tr-'; then fail "a systemd scope was used where there is no systemd"; fi
  ok "no systemd scope (the process tree is used)"
  cx fil kill viajumps tr > /dev/null
  fc_wait_exit a 20
  wait_pid_gone rn-target "$bg" 10 || fail "the background job ($bg) survived fil kill"
  ok "fil kill ended the shell and its background job"
}

# Another Unix user cannot reach the first user's daemon: the directory refuses it, and when the
# directory is opened up by hand the daemon's own peer-UID check does. Each user has a daemon of
# its own. A directory somebody else prepared makes fil refuse to start.
f_other_user() {
  local out u
  scenario_cleanup() {
    [[ -n ${FUID:-} ]] || return 0
    docker exec rn-target sh -c "chmod 700 /tmp/fil-$FUID; chmod 600 /tmp/fil-$FUID/*.sock" > /dev/null 2>&1 || true
  }
  trap scenario_cleanup EXIT
  cx fil ls viajumps > /dev/null || fail "could not start fil's daemon on the target"
  FUID=$(docker exec rn-target id -u fil)
  local muid sock probe
  muid=$(docker exec rn-target id -u mallory)
  sock=$(docker exec rn-target sh -c "ls /tmp/fil-$FUID/*.sock" | head -n 1)
  [[ -n $sock ]] || fail "no socket in /tmp/fil-$FUID"
  [[ $(docker exec rn-target stat -c '%a %U' "/tmp/fil-$FUID") == '700 fil' ]] || fail "the socket directory is not 0700 fil"
  [[ $(docker exec rn-target stat -c '%a %U' "$sock") == '600 fil' ]] || fail "the socket is not 0600 fil"
  ok "the socket directory is 0700 and the socket 0600, both fil's"

  if docker exec -u mallory rn-target ls "/tmp/fil-$FUID" > /dev/null 2>&1; then fail "mallory can list fil's socket directory"; fi
  probe=$(docker exec -u mallory rn-target sockprobe "$sock")
  [[ $probe == 'REFUSED errno=13'* ]] || fail "mallory's connect: $probe"
  ok "mallory is stopped at the directory: $probe"

  # Open the directory and the socket by hand (as root) so only the daemon's own check is left.
  docker exec rn-target sh -c "chmod 711 /tmp/fil-$FUID; chmod 666 $sock"
  probe=$(docker exec -u fil rn-target sockprobe "$sock")
  [[ $probe == DATA* ]] || fail "control: fil's own connection got: $probe"
  probe=$(docker exec -u mallory rn-target sockprobe "$sock")
  [[ $probe == CLOSED* ]] || fail "mallory with the door open got: $probe"
  ok "with the directory and socket opened, the daemon hangs up on mallory (fil itself is let in and greeted)"
  local i
  for ((i = 0; i < 20; i++)); do
    docker exec rn-target grep -q "\"peer_uid\":$muid" /home/fil/.local/state/fil/daemon.log && break
    sleep 0.2
  done
  docker exec rn-target grep "\"peer_uid\":$muid" /home/fil/.local/state/fil/daemon.log | grep -q 'refused a connection from another user' \
    || fail "the daemon's log has no refusal for uid $muid"
  ok "the daemon logged the refusal (uid $muid)"
  scenario_cleanup
  cx fil ls viajumps > /dev/null || fail "the daemon stopped serving fil after the probes"

  # A directory somebody else made, with the wrong mode, or a symbolic link: fil refuses to start.
  local vuid v2uid v3uid err rc
  vuid=$(docker exec rn-target id -u victim)
  v2uid=$(docker exec rn-target id -u victim2)
  v3uid=$(docker exec rn-target id -u victim3)
  docker exec -u mallory rn-target mkdir -m 700 "/tmp/fil-$vuid"
  rc=0
  err=$(docker exec -u victim rn-target fil server --stdio 2>&1 > /dev/null) || rc=$?
  [[ $rc -ne 0 && $err == *"owned by uid $muid, not by uid $vuid"* ]] || fail "directory made by another user: rc=$rc $err"
  ok "a directory made by another user is refused: $err"
  docker exec rn-target install -d -o victim2 -g victim2 -m 755 "/tmp/fil-$v2uid"
  rc=0
  err=$(docker exec -u victim2 rn-target fil server --stdio 2>&1 > /dev/null) || rc=$?
  [[ $rc -ne 0 && $err == *'has mode 0755, not 0700'* ]] || fail "directory with the wrong mode: rc=$rc $err"
  ok "a directory with the wrong mode is refused: $err"
  docker exec rn-target ln -s /tmp "/tmp/fil-$v3uid"
  rc=0
  err=$(docker exec -u victim3 rn-target fil server --stdio 2>&1 > /dev/null) || rc=$?
  [[ $rc -ne 0 && $err == *'symbolic link'* ]] || fail "symbolic link in place of the directory: rc=$rc $err"
  ok "a symbolic link in place of the directory is refused: $err"
  for u in victim victim2 victim3; do
    if docker exec rn-target pgrep -u "$u" fil > /dev/null; then fail "a fil process runs as $u"; fi
  done

  # Two users on one host: two daemons, two directories, sessions not shared.
  fc_start a -- direct iso
  fc_ready a
  fc_detach a
  out=$(cx fil ls direct-other)
  [[ $out == 'no sessions' ]] || fail "mallory's fil ls on the host where fil has a session: $out"
  out=$(docker exec rn-jump1 sh -c 'stat -c "%U:%a" /tmp/fil-*' | tr '\n' ' ')
  [[ $out == *'fil:700'* && $out == *'mallory:700'* ]] || fail "socket directories on jump1: $out"
  ok "each user has a daemon and a directory of its own; mallory does not see fil's session"
  fk direct iso
}

# Retired from the default run (still runnable by name): the real fil now covers them.
#   s_pipelink_echo      -> f_proxyjump, f_noisy, f_attach_detach (PipeLink at both ends, through
#                           both jumps and the noisy login)
#   s_pipelink_kill_ssh  -> f_kill_ssh (the same kill, and the session resumes)
ALL_SCENARIOS=(
  s_topology s_direct s_jumps s_controlmaster s_certs s_noisy s_flood s_hostile_names
  s_kill_ssh_client s_kill_sshd_child
  s_netdown_app_heartbeat s_netdown_ssh_keepalive s_netdown_no_detection
  s_pipelink_flood s_pipelink_netdown_heartbeat s_pipelink_netdown_keepalive
  f_attach_detach f_ls_kill_detach f_takeover f_takeover_back f_kill_ssh f_daemon_restart
  f_noisy f_proxyjump f_tree_scope f_other_user
)

part_main ssh ssh_setup ssh_teardown "${ALL_SCENARIOS[@]}" -- "$@"
