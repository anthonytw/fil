#!/usr/bin/env bash
# Tier 1, part 1: the pipe over real OpenSSH in Docker (docs/m1/realnet.md).
#
#   client --rn-c-- jump1 --rn-1-- jump2 --rn-2-- target
#
# Three internal Docker networks, so the target is reachable only through the jumps. All four are
# the same image (OpenSSH server and client, pipe-echo, quic-echo). Run: ssh.sh [SCENARIO...]
set -uo pipefail
# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

IMAGE=fil-realnet
HOSTS=(client jump1 jump2 target)
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
  cp "$BIN_DIR/pipe-echo" "$BIN_DIR/quic-echo" "$ctx/bin/"
  docker build -q -t "$IMAGE" "$ctx" > /dev/null

  make_keys
  local n
  for n in "${NETWORKS[@]}"; do docker network create --internal "$n" > /dev/null; done
  start_host client rn-c
  start_host jump1 rn-c rn-1
  start_host jump2 rn-1 rn-2
  start_host target rn-2

  local h
  for h in "${HOSTS[@]}"; do
    docker cp "$SSH_KEYS/authorized_keys" "rn-$h:/etc/fil-realnet/authorized_keys"
    docker cp "$SSH_KEYS/ca.pub" "rn-$h:/etc/fil-realnet/ca.pub"
  done
  for f in id_user id_cert cert-valid.pub cert-expired.pub cert-wrongprincipal.pub \
    config config-ka config-cert-valid config-cert-nocert config-cert-expired \
    config-cert-wrongprincipal; do
    docker cp "$SSH_KEYS/$f" "rn-client:$TSSH/$f"
  done
  docker exec -u root rn-client sh -c "chown -R tester:tester $TSSH && chmod 600 $TSSH/*"

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
    for h in "${HOSTS[@]}"; do
      docker cp "rn-$h:/var/log/sshd.log" "$OUT/logs/sshd-$h.log" > /dev/null 2>&1 || true
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
  docker run -d --name "rn-$name" --network "$first" --network-alias "$name" "$IMAGE" > /dev/null
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

accepted_count() { docker exec "rn-$1" grep -c "Accepted publickey for ${2:-fil}" /var/log/sshd.log || true; }

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

ALL_SCENARIOS=(
  s_topology s_direct s_jumps s_controlmaster s_certs s_noisy s_flood s_hostile_names
  s_kill_ssh_client s_kill_sshd_child
  s_netdown_app_heartbeat s_netdown_ssh_keepalive s_netdown_no_detection
)

part_main ssh ssh_setup ssh_teardown "${ALL_SCENARIOS[@]}" -- "$@"
