#!/usr/bin/env bash
# Tier 1, part 2: direct QUIC over the real Linux network stack, in network namespaces
# (docs/m1/realnet.md). Needs root (through the root helper in lib.sh), ip, tc, iptables, conntrack.
#
#   client --(c0 10.0.1.0/24, c1 10.0.2.0/24)-- router --(rs 198.51.100.0/24)-- server
#
# The router NATs the client (SNAT, a different public address per client link) and carries
# `tc netem` on its egress interfaces. Run: udp.sh [SCENARIO...]
set -uo pipefail
# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
# shellcheck source=fil.sh
source "$(dirname "${BASH_SOURCE[0]}")/fil.sh"

NS_C=rn-client
NS_R=rn-router
NS_S=rn-server
SRV_IP=198.51.100.10
PUB1=198.51.100.1 # the client's public address on link 1
PUB2=198.51.100.2 # ... on link 2
PUB3=198.51.100.3 # a third address, for NAT pool changes
QUIC=$BIN_DIR/quic-echo
QL=$BIN_DIR/quiclink-probe
MIB=$((1 << 20))
PROBE_PORT=62000

nsx() {
  local ns=$1
  shift
  as_root ip netns exec "$ns" "$@"
}

# ---------------------------------------------------------------------------------------------
# Topology

udp_setup() {
  local c
  for c in ip tc iptables conntrack ss; do
    command -v "$c" > /dev/null || { echo "realnet: '$c' is required (apt: iproute2 iptables conntrack)" >&2; exit 2; }
  done
  as_root true || { echo "realnet: root is required for network namespaces" >&2; exit 2; }
  udp_teardown quiet
  mkdir -p "$OUT/run" "$OUT/logs"
  "$REALNET_DIR/build.sh"

  local ns
  for ns in "$NS_C" "$NS_R" "$NS_S"; do
    as_root ip netns add "$ns"
    nsx "$ns" ip link set lo up
  done
  veth rn-c0 rn-rc0 "$NS_C" "$NS_R"
  veth rn-c1 rn-rc1 "$NS_C" "$NS_R"
  veth rn-rs rn-s0 "$NS_R" "$NS_S"
  # Short names inside the namespaces.
  nsx "$NS_C" ip link set rn-c0 name c0
  nsx "$NS_C" ip link set rn-c1 name c1
  nsx "$NS_R" ip link set rn-rc0 name rc0
  nsx "$NS_R" ip link set rn-rc1 name rc1
  nsx "$NS_R" ip link set rn-rs name rs
  nsx "$NS_S" ip link set rn-s0 name s0

  nsx "$NS_C" ip addr add 10.0.1.2/24 dev c0
  nsx "$NS_R" ip addr add 10.0.1.1/24 dev rc0
  nsx "$NS_R" ip addr add 10.0.2.1/24 dev rc1
  nsx "$NS_R" ip addr add "$PUB1/24" dev rs
  nsx "$NS_R" ip addr add "$PUB2/24" dev rs
  nsx "$NS_R" ip addr add "$PUB3/24" dev rs
  nsx "$NS_S" ip addr add "$SRV_IP/24" dev s0
  local pair
  for pair in "$NS_C:c0" "$NS_C:c1" "$NS_R:rc0" "$NS_R:rc1" "$NS_R:rs" "$NS_S:s0"; do
    nsx "${pair%%:*}" ip link set "${pair##*:}" up
    # No segmentation offload: netem must see (and drop, delay) single packets, not 64 KiB bursts.
    for feat in gso gro tso tx-udp-segmentation; do
      nsx "${pair%%:*}" ethtool -K "${pair##*:}" "$feat" off > /dev/null 2>&1 || true
    done
  done
  nsx "$NS_C" ip route add default via 10.0.1.1 dev c0
  nsx "$NS_R" sysctl -q -w net.ipv4.ip_forward=1
  reset_net
  log "namespaces up: $NS_C | $NS_R | $NS_S"
  udp_fil_setup
}

# veth A B NS_A NS_B: a veth pair with one end in each namespace.
veth() {
  as_root ip link add "$1" type veth peer name "$2"
  as_root ip link set "$1" netns "$3"
  as_root ip link set "$2" netns "$4"
}

# Back to the clean state every scenario starts from: no netem, no firewall rules, the NAT rules
# of the topology, the client on link 1 only.
reset_net() {
  netem_clear
  nsx "$NS_R" iptables -F FORWARD
  nsx "$NS_R" iptables -t nat -F POSTROUTING
  nsx "$NS_R" iptables -t nat -A POSTROUTING -o rs -s 10.0.1.0/24 -j SNAT --to-source "$PUB1"
  nsx "$NS_R" iptables -t nat -A POSTROUTING -o rs -s 10.0.2.0/24 -j SNAT --to-source "$PUB2"
  nsx "$NS_R" conntrack -F > /dev/null 2>&1 || true
  nsx "$NS_C" ip addr flush dev c1
  nsx "$NS_C" ip link set c0 up
  nsx "$NS_C" ip route replace default via 10.0.1.1 dev c0
}

udp_teardown() {
  local ns p
  if [[ ${1:-} != quiet ]]; then
    mkdir -p "$OUT/logs"
    # The real daemon's log and the client's, before the users are removed.
    as_root cat /home/rnfil/.local/state/fil/daemon.log > "$OUT/logs/fil-daemon-udp.log" 2> /dev/null || true
  fi
  for ns in "$NS_C" "$NS_R" "$NS_S"; do
    for p in $(as_root ip netns pids "$ns" 2> /dev/null); do
      as_root kill -9 "$p" 2> /dev/null || true
    done
    as_root ip netns del "$ns" 2> /dev/null || true
  done
  udp_fil_teardown
  as_root chmod -R a+rX "$OUT/logs" 2> /dev/null || true
}

# ---------------------------------------------------------------------------------------------
# Impairments (tc netem on the router's egress interfaces: rs carries client->server, rc0/rc1
# carry server->client, so every packet crosses exactly one netem instance)

NETEM_DEVS=(rc0 rc1 rs)

# netem_apply ARGS...: e.g. netem_apply delay 40ms loss 5%
netem_apply() {
  local d
  for d in "${NETEM_DEVS[@]}"; do
    nsx "$NS_R" tc qdisc replace dev "$d" root netem "$@" limit 100000
  done
}

# netem_chain "STAGE1 ARGS" "STAGE2 ARGS": two netem stages in series (base delay, then reordering).
netem_chain() {
  local d
  for d in "${NETEM_DEVS[@]}"; do
    # shellcheck disable=SC2086
    nsx "$NS_R" tc qdisc replace dev "$d" root handle 1: netem $1 limit 100000
    # shellcheck disable=SC2086
    nsx "$NS_R" tc qdisc add dev "$d" parent 1:1 handle 2: netem $2 limit 100000
  done
}

netem_clear() {
  local d
  for d in "${NETEM_DEVS[@]}"; do
    nsx "$NS_R" tc qdisc del dev "$d" root 2> /dev/null || true
  done
}

udp_block() { nsx "$NS_R" iptables -I FORWARD 1 -p udp -j DROP; }
udp_unblock() { nsx "$NS_R" iptables -D FORWARD -p udp -j DROP; }

# ---------------------------------------------------------------------------------------------
# Helpers for scenarios

# new_run NAME: a fresh directory (D) for one server and its clients.
new_run() {
  D=$OUT/run/$SCENARIO/$1
  rm -rf "$D"
  mkdir -p "$D/keys"
}

# shellcheck disable=SC2120 # callers may pass extra server arguments
# start_server [EXTRA_ARGS...]: sets SRV_PID, SRV_LOG, PORT. Uses D.
start_server() {
  SRV_LOG=$D/server.log
  rm -f "$D/port" "$D/fp"
  nsx "$NS_S" "$QUIC" server --keys-dir "$D/keys" --fp-out "$D/fp" --port-out "$D/port" "$@" \
    > "$SRV_LOG" 2>&1 &
  SRV_PID=$!
  local i
  for ((i = 0; i < 100; i++)); do
    [[ -s $D/port ]] && break
    sleep 0.1
  done
  [[ -s $D/port ]] || fail "the server did not start: $(cat "$SRV_LOG")"
  PORT=$(cat "$D/port")
}

stop_server() {
  as_root kill "$SRV_PID" 2> /dev/null || true
  wait "$SRV_PID" 2> /dev/null || true
}

# client_cmd NAME ARGS...: the command line of a client in the client namespace.
# (A bounded run: `timeout` inside the namespace, so a hang is a failure, not a stuck job.)
run_client() { # run_client NAME ARGS...: foreground; output in $D/NAME.out
  local name=$1
  shift
  CLIENT_OUT=$D/$name.out
  nsx "$NS_C" timeout 200 "$QUIC" client --server "$SRV_IP:$PORT" --pub-out "$D/keys/$name.pub" \
    --pin-file "$D/fp" "$@" > "$CLIENT_OUT" 2>&1
}

bg_client() { # bg_client NAME ARGS...: sets CLIENT_PID, CLIENT_OUT
  local name=$1
  shift
  CLIENT_OUT=$D/$name.out
  nsx "$NS_C" timeout 200 "$QUIC" client --server "$SRV_IP:$PORT" --pub-out "$D/keys/$name.pub" \
    --pin-file "$D/fp" "$@" > "$CLIENT_OUT" 2>&1 &
  CLIENT_PID=$!
}

handshakes() { count_lines "$SRV_LOG" CONNECTED; }

# ---------------------------------------------------------------------------------------------
# The same, for quiclink-probe: the real QuicLink and QuicListener (fil-transport::quic)

# shellcheck disable=SC2120 # callers may pass extra server arguments
# ql_start_server [EXTRA_ARGS...]: sets SRV_PID, SRV_LOG, PORT. Uses D.
ql_start_server() {
  SRV_LOG=$D/server.log
  rm -f "$D/port" "$D/fp"
  nsx "$NS_S" "$QL" server --keys-dir "$D/keys" --fp-out "$D/fp" --port-out "$D/port" "$@" \
    > "$SRV_LOG" 2>&1 &
  SRV_PID=$!
  local i
  for ((i = 0; i < 100; i++)); do
    [[ -s $D/port ]] && break
    sleep 0.1
  done
  [[ -s $D/port ]] || fail "the server did not start: $(cat "$SRV_LOG")"
  PORT=$(cat "$D/port")
}

ql_run_client() { # ql_run_client NAME ARGS...: foreground; output in $D/NAME.out
  local name=$1
  shift
  CLIENT_OUT=$D/$name.out
  nsx "$NS_C" timeout 200 "$QL" client --server "$SRV_IP:$PORT" --pub-out "$D/keys/$name.pub" \
    --pin-file "$D/fp" "$@" > "$CLIENT_OUT" 2>&1
}

ql_bg_client() { # ql_bg_client NAME ARGS...: sets CLIENT_PID, CLIENT_OUT
  local name=$1
  shift
  CLIENT_OUT=$D/$name.out
  nsx "$NS_C" timeout 200 "$QL" client --server "$SRV_IP:$PORT" --pub-out "$D/keys/$name.pub" \
    --pin-file "$D/fp" "$@" > "$CLIENT_OUT" 2>&1 &
  CLIENT_PID=$!
}

ql_accepts() { count_lines "$SRV_LOG" ACCEPTED; }

# ---------------------------------------------------------------------------------------------
# Scenarios

# Does netem reorder when it adds jitter? Measured with a numbered-datagram probe, not assumed.
s_netem_reorder_probe() {
  new_run probe
  local label args line
  for label in clean jitter-plain jitter-rate-limited; do
    reset_net
    case $label in
      clean) ;;
      jitter-plain) netem_apply delay 55ms 15ms ;;
      jitter-rate-limited) netem_apply delay 55ms 15ms rate 1gbit ;;
    esac
    nsx "$NS_S" "$QUIC" udp-recv --bind "0.0.0.0:$PROBE_PORT" --idle-ms 1500 > "$D/$label.recv" 2>&1 &
    local rpid=$!
    wait_for_line "$D/$label.recv" UDP-READY 5 || fail "probe receiver did not start"
    nsx "$NS_C" "$QUIC" udp-send --to "$SRV_IP:$PROBE_PORT" --count 3000 --interval-us 1000 > "$D/$label.send" 2>&1
    wait_exit "$rpid" 20
    line=$(last_line "$D/$label.recv" UDP-RECV)
    log "$label: $line"
    args=$(kv "$line" reordered)
    note "$label: reordered=$args of $(kv "$line" received) (max displacement $(kv "$line" max_displacement))"
    case $label in
      clean) assert "no reordering without netem" test "$args" -eq 0 ;;
      jitter-plain) assert "plain netem jitter DOES reorder" test "$args" -gt 0 ;;
      jitter-rate-limited) assert "'rate' makes netem jitter in-order" test "$args" -eq 0 ;;
    esac
  done
  reset_net
}

s_clean() {
  reset_net
  new_run clean
  start_server
  local rc=0 line
  run_client c --bytes $((32 * MIB)) || rc=$?
  cat "$CLIENT_OUT"
  line=$(last_line "$CLIENT_OUT" RESULT)
  assert "the client exited 0" test "$rc" -eq 0
  assert "32 MiB echoed byte-exact" test "$(kv "$line" recvd)" -eq $((32 * MIB))
  assert "one handshake" test "$(handshakes)" -eq 1
  assert "the server saw the NATed address, not the client's own" \
    grep -q "^CONNECTED n=1 remote=$PUB1:" "$SRV_LOG"
  assert_num "the server bound inside 60000-61000 (port $PORT)" "$PORT" -ge 60000
  assert_num "..." "$PORT" -le 61000
  local sockets
  sockets=$(nsx "$NS_S" ss -uln)
  assert "and it is a UDP socket in the server namespace" grep -q ":$PORT " <<< "$sockets"
  note "mbps=$(kv "$line" mbps)" "handshake_ms=$(kv "$(last_line "$CLIENT_OUT" CONNECTED)" handshake_ms)"
  stop_server
}

s_port_range() {
  reset_net
  new_run range
  local i ports=() pids=() p
  for i in 1 2 3 4; do
    mkdir -p "$D/s$i/keys"
    nsx "$NS_S" "$QUIC" server --keys-dir "$D/s$i/keys" --fp-out "$D/s$i/fp" --port-out "$D/s$i/port" \
      --port-range 60000-60003 > "$D/s$i/server.log" 2>&1 &
    pids+=($!)
    for ((p = 0; p < 100; p++)); do
      [[ -s $D/s$i/port ]] && break
      sleep 0.1
    done
    ports+=("$(cat "$D/s$i/port")")
  done
  log "ports taken: ${ports[*]}"
  assert "four servers got four distinct ports" test "$(printf '%s\n' "${ports[@]}" | sort -u | wc -l)" -eq 4
  for p in "${ports[@]}"; do
    assert_num "port $p is in the range" "$p" -ge 60000
    assert_num "..." "$p" -le 60003
  done
  mkdir -p "$D/s5/keys"
  local rc=0
  nsx "$NS_S" "$QUIC" server --keys-dir "$D/s5/keys" --fp-out "$D/s5/fp" --port-out "$D/s5/port" \
    --port-range 60000-60003 > "$D/s5/server.log" 2>&1 || rc=$?
  assert "a fifth server finds no free port and exits with status 3 (rc=$rc)" test "$rc" -eq 3
  assert "and says so" grep -q NO-FREE-PORT "$D/s5/server.log"
  for p in "${pids[@]}"; do
    as_root kill "$p" 2> /dev/null || true
  done
  wait 2> /dev/null || true
  note "ports=${ports[*]}"
}

# One 2 MiB echo under one set of impairments. Sets the globals CASE_SECS, CASE_RC.
impaired_case() { # impaired_case LABEL SIM_REFERENCE -- netem_apply ARGS | chain STAGE1 STAGE2
  local label=$1 sim=$2
  shift 2
  reset_net
  if [[ $1 == chain ]]; then netem_chain "$2" "$3"; elif [[ $1 != none ]]; then netem_apply "$@"; fi
  new_run "$label"
  start_server
  local rc=0 line
  run_client c --bytes $((2 * MIB)) || rc=$?
  cat "$CLIENT_OUT"
  line=$(last_line "$CLIENT_OUT" RESULT)
  assert "$label: finished intact (rc=$rc)" test "$rc" -eq 0
  assert "$label: 2 MiB echoed" test "$(kv "$line" recvd)" -eq $((2 * MIB))
  assert "$label: one handshake" test "$(handshakes)" -eq 1
  note "$label=$(kv "$line" secs)s (sim: $sim)"
  stop_server
}

# 40 ms one way, as in the simulator's table (docs/spikes/netsim.md). Netem's "delay D J" is
# uniform in D-J..D+J; the simulator adds uniform 0..J, so jitter J is "delay (40+J/2) (J/2)".
s_impaired() {
  impaired_case clean40 "0.8s" delay 40ms
  impaired_case jitter30-inorder "1.2s" delay 55ms 15ms rate 1gbit
  impaired_case loss5 "n/a, 8% loss was 34s" delay 40ms loss 5%
  impaired_case loss8 "34s" delay 40ms loss 8%
  impaired_case jitter30-inorder-loss5 "63s" delay 55ms 15ms loss 5% rate 1gbit
  impaired_case jitter30-reordering "53s" delay 55ms 15ms
  impaired_case reorder1pct-80ms "12s" chain "delay 40ms" "delay 80ms reorder 99%"
  impaired_case reorder10pct-10ms "26s" chain "delay 40ms" "delay 10ms reorder 90%"
  reset_net
}

# Move the client's NAT mapping three times while a stream is running. Each move flushes the
# router's conntrack table and re-points the SNAT rule, so the next packet gets a new mapping
# and the old public port/address stops working, as when a home router reboots or recycles.
s_nat_rebind() {
  reset_net
  netem_apply delay 20ms
  new_run rebind
  start_server
  bg_client c --mode stream --duration 16 --rate 524288
  wait_for_line "$CLIENT_OUT" CONNECTED 10 || fail "client did not connect: $(cat "$CLIENT_OUT")"
  local spec ev=() t
  for spec in "$PUB1:31000-31999" "$PUB1:32000-32999" "$PUB3:33000-33999"; do
    sleep 3.5
    t=$(now_ms)
    nsx "$NS_R" iptables -t nat -R POSTROUTING 1 -o rs -p udp -s 10.0.1.0/24 -j SNAT --to-source "$spec" --random
    nsx "$NS_R" conntrack -F > /dev/null 2>&1 || true
    ev+=("$t")
    log "NAT mapping moved to $spec"
  done
  wait_exit "$CLIENT_PID" 40
  cat "$CLIENT_OUT"
  local line changes
  line=$(last_line "$CLIENT_OUT" RESULT)
  assert "the client finished (rc=$WAIT_RC)" test "$WAIT_RC" -eq 0
  assert "echo intact" test "$(kv "$line" sent)" = "$(kv "$line" recvd)"
  assert "the connection was never lost" test "$(kv "$line" conn_lost)" = false
  assert "exactly one handshake" test "$(handshakes)" -eq 1
  changes=$(count_lines "$SRV_LOG" PATH-CHANGE)
  assert_num "the server followed the client's address (path changes)" "$changes" -ge 3
  assert "the last move was to the third public address" \
    grep -q "^PATH-CHANGE .* to=$PUB3:3" "$SRV_LOG"
  local i=0 pc detect=()
  while read -r pc; do
    if [[ $i -lt 3 ]]; then detect+=($(($(kv "$pc" at_ms) - ${ev[$i]}))); fi
    i=$((i + 1))
  done < <(grep -a '^PATH-CHANGE' "$SRV_LOG")
  note "path_changes=$changes" "server_learned_after_ms=${detect[*]}" \
    "max_stall_ms=$(kv "$line" max_stall_ms)" "lost_packets=$(kv "$line" lost_packets)" \
    "mbps=$(kv "$line" mbps)"
  stop_server
}

# Switch the client from link 1 to link 2: a new local address and route, the old link taken
# down. The router NATs link 2 to a different public address, so the server sees a new IP.
s_ip_change() {
  reset_net
  netem_apply delay 20ms
  new_run ipchange
  start_server
  bg_client c --mode stream --duration 12 --rate 524288
  wait_for_line "$CLIENT_OUT" CONNECTED 10 || fail "client did not connect: $(cat "$CLIENT_OUT")"
  sleep 4
  local t
  t=$(now_ms)
  nsx "$NS_C" ip addr add 10.0.2.2/24 dev c1
  nsx "$NS_C" ip route replace default via 10.0.2.1 dev c1
  nsx "$NS_C" ip link set c0 down
  log "client switched to link 2"
  wait_exit "$CLIENT_PID" 40
  cat "$CLIENT_OUT"
  local line pc
  line=$(last_line "$CLIENT_OUT" RESULT)
  assert "the client finished (rc=$WAIT_RC)" test "$WAIT_RC" -eq 0
  assert "echo intact" test "$(kv "$line" sent)" = "$(kv "$line" recvd)"
  assert "the connection was never lost" test "$(kv "$line" conn_lost)" = false
  assert "exactly one handshake" test "$(handshakes)" -eq 1
  assert "the server saw the new public address" grep -q "^PATH-CHANGE .* to=$PUB2:" "$SRV_LOG"
  pc=$(last_line "$SRV_LOG" PATH-CHANGE)
  note "server_learned_after_ms=$(($(kv "$pc" at_ms) - t))" "max_stall_ms=$(kv "$line" max_stall_ms)" \
    "path_changes=$(count_lines "$SRV_LOG" PATH-CHANGE)"
  stop_server
}

# A UDP blackout of DURATION seconds in the middle of a stream, with idle timeout 5 s and
# keep-alive 1 s (LivenessParams::default). Shorter than the idle timeout the connection should
# survive; longer, it ends with a timeout and the client reconnects with a new handshake.
blackout_case() { # blackout_case SECONDS EXPECT(survive|lost|either) [quiet]
  local dur=$1 expect=$2 traffic=${3:-active} rate=262144 label="blackout-$1s" poke=()
  # "quiet": nothing but keep-alives on the link. "poke": quiet, but one keystroke-sized write the
  # moment the link returns, which is what the simulator's blackout table did (docs/spikes/netsim.md).
  case $traffic in
    quiet) rate=0 label="blackout-quiet-$1s" ;;
    poke)
      rate=0
      label="blackout-poke-$1s"
      poke=(--poke-file "$OUT/run/$SCENARIO/$label/poke")
      ;;
  esac
  reset_net
  netem_apply delay 20ms
  new_run "$label"
  start_server
  bg_client c --mode stream --duration $((3 + ${dur%.*} + 8)) --rate "$rate" ${poke[@]+"${poke[@]}"} --reconnect --reconnect-for 40
  wait_for_line "$CLIENT_OUT" CONNECTED 10 || fail "client did not connect: $(cat "$CLIENT_OUT")"
  sleep 3
  local t_block t_heal
  t_block=$(now_ms)
  udp_block
  sleep "$dur"
  udp_unblock
  t_heal=$(now_ms)
  [[ $traffic != poke ]] || : > "$D/poke"
  wait_exit "$CLIENT_PID" 60
  cat "$CLIENT_OUT"
  local line lost=false lost_after="" lost_reason="" recov=""
  line=$(last_line "$CLIENT_OUT" RESULT)
  assert "$label: the client finished (rc=$WAIT_RC)" test "$WAIT_RC" -eq 0
  if [[ $(kv "$line" conn_lost) == true ]]; then
    lost=true
    lost_after=$(($(kv "$(last_line "$CLIENT_OUT" CONN-LOST)" at_ms) - t_block))
    assert "$label: it reconnected" grep -q '^RECONNECTED' "$CLIENT_OUT"
    assert "$label: the server saw a second handshake" test "$(handshakes)" -eq 2
    # Either end may give up first. If the client's idle timer fires, the reason is TimedOut. If the
    # server's fires first, it forgets the connection and answers the client's next packet (after
    # the link returns) with a stateless reset, so the reason is Reset. Both are a lost connection.
    lost_reason=$(kv "$(last_line "$CLIENT_OUT" CONN-LOST)" reason | tr -d '"')
    assert "$label: lost by an idle timeout or the server's stateless reset (reason=$lost_reason)" \
      test "$lost_reason" = TimedOut -o "$lost_reason" = Reset
    note "$label=LOST after ${lost_after}ms, reconnected $(kv "$(last_line "$CLIENT_OUT" RECONNECTED)" after_loss_ms)ms later"
  else
    assert "$label: echo intact" test "$(kv "$line" sent)" = "$(kv "$line" recvd)"
    assert "$label: no new handshake" test "$(handshakes)" -eq 1
    local stall_end
    stall_end=$(kv "$(grep -a '^STALL' "$CLIENT_OUT" | tail -n 1)" end_at_ms || true)
    [[ -z $stall_end ]] || recov=$((stall_end - t_heal))
    note "$label=survived, first data ${recov:-?}ms after the link returned, max_stall=$(kv "$line" max_stall_ms)ms"
  fi
  case $expect in
    survive) assert "$label: survived, as expected" test "$lost" = false ;;
    lost) assert "$label: lost, as expected" test "$lost" = true ;;
  esac
  if [[ $lost == true && $lost_reason == TimedOut ]]; then
    assert_num "$label: noticed about one idle timeout after the blackout began (ms)" "$lost_after" -ge 4000
    assert_num "$label: ... and not much later (ms)" "$lost_after" -le 7500
  elif [[ $lost == true ]]; then
    # A reset can only arrive once the link is back: after the blackout, and soon after it.
    assert_num "$label: the reset came after the blackout (ms)" "$lost_after" -ge "$(awk -v d="$dur" 'BEGIN{print int(d*1000)}')"
    assert_num "$label: ... and soon after it (ms)" "$lost_after" -le "$(awk -v d="$dur" 'BEGIN{print int(d*1000)+5000}')"
  fi
  stop_server
}

s_blackouts() {
  blackout_case 3.0 survive
  blackout_case 4.5 either
  blackout_case 5.5 either
  blackout_case 8.0 lost
  # The simulator's table (docs/spikes/netsim.md) used a quiet link: same sweep around the cutoff.
  blackout_case 4.0 either quiet
  blackout_case 4.5 either quiet
  blackout_case 4.9 either quiet
  blackout_case 5.5 either quiet
  blackout_case 4.5 survive poke
  blackout_case 4.9 either poke
  blackout_case 5.5 either poke
}

# UDP dropped by the router: how long until the client KNOWS? (The input for the pipe fallback.)
s_udp_blocked() {
  reset_net
  netem_apply delay 20ms
  local idle line
  # (a) blocked from the start: the connect attempt.
  for idle in 5000 15000; do
    new_run "blocked-start-idle$idle"
    start_server
    udp_block
    local rc=0
    run_client c --bytes 1024 --idle-ms "$idle" --connect-timeout 40 || rc=$?
    udp_unblock
    cat "$CLIENT_OUT"
    line=$(last_line "$CLIENT_OUT" CONNECT-FAILED)
    assert "idle $idle: the connect attempt failed (rc=$rc)" test "$rc" -eq 4
    assert "idle $idle: the server never saw a handshake" test "$(count_lines "$SRV_LOG" ACCEPT)" -eq 0
    note "blocked_at_start(idle=${idle}ms)=gave up after $(kv "$line" after_ms)ms"
    stop_server
  done
  # (b) blocked in the middle of a session, and never lifted.
  new_run blocked-mid
  start_server
  bg_client c --mode stream --duration 30 --rate 262144
  wait_for_line "$CLIENT_OUT" CONNECTED 10 || fail "client did not connect"
  sleep 3
  local t
  t=$(now_ms)
  udp_block
  wait_exit "$CLIENT_PID" 40
  udp_unblock
  cat "$CLIENT_OUT"
  assert "mid-session: the client ended with an error status (rc=$WAIT_RC)" test "$WAIT_RC" -eq 4
  line=$(last_line "$CLIENT_OUT" CONN-LOST)
  local after=$(($(kv "$line" at_ms) - t))
  assert_num "mid-session: it knew after about the idle timeout (ms)" "$after" -ge 4000
  assert_num "... and not much later (ms)" "$after" -le 7500
  note "blocked_mid_session=noticed after ${after}ms (idle 5000ms)"
  stop_server
}

# ---------------------------------------------------------------------------------------------
# QuicLink scenarios (P2): the same faults, through the real link and listener. The link must
# stay Up through NAT changes, report Degraded then Down(Dead) on a black hole, and refuse in
# connect_timeout (2 s) rather than the idle timeout when UDP never works.

s_quiclink_rebind() {
  reset_net
  netem_apply delay 20ms
  new_run qlrebind
  ql_start_server
  ql_bg_client c --duration 16 --rate 524288
  wait_for_line "$CLIENT_OUT" CONNECTED 10 || fail "client did not connect: $(cat "$CLIENT_OUT")"
  local spec
  for spec in "$PUB1:31000-31999" "$PUB1:32000-32999" "$PUB3:33000-33999"; do
    sleep 3.5
    nsx "$NS_R" iptables -t nat -R POSTROUTING 1 -o rs -p udp -s 10.0.1.0/24 -j SNAT --to-source "$spec" --random
    nsx "$NS_R" conntrack -F > /dev/null 2>&1 || true
    log "NAT mapping moved to $spec"
  done
  wait_exit "$CLIENT_PID" 40
  cat "$CLIENT_OUT"
  local line gens
  line=$(last_line "$CLIENT_OUT" RESULT)
  assert "the client finished (rc=$WAIT_RC)" test "$WAIT_RC" -eq 0
  assert "echo intact" test "$(kv "$line" sent)" = "$(kv "$line" recvd)"
  assert "the link was never lost" test "$(kv "$line" conn_lost)" = false
  assert "exactly one handshake (one accepted link)" test "$(ql_accepts)" -eq 1
  assert "the listener answered the first packet with a Retry" grep -q '^ACCEPTED n=1 .* retried=1 ' "$SRV_LOG"
  gens=$(count_lines "$SRV_LOG" PATH-GEN)
  assert_num "the daemon's link counted the path changes (path_generation bumps)" "$gens" -ge 3
  assert "the last change was to the third public address" grep -q "^PATH-GEN .* remote=$PUB3:3" "$SRV_LOG"
  assert "the daemon's link never degraded" test "$(count_lines "$SRV_LOG" 'LINK-STATE n=1 state=Degraded')" -eq 0
  note "daemon_path_gens=$gens" "client_path_gen=$(kv "$line" path_gen) (the client cannot see a NAT rebind)"
  stop_server
}

s_quiclink_udp_blocked() {
  reset_net
  netem_apply delay 20ms
  local line rc
  # (a) blocked from the start: Refused at connect_timeout, not at the 5 s idle timeout.
  new_run qlblocked-start
  ql_start_server
  udp_block
  rc=0
  ql_run_client c --duration 1 || rc=$?
  udp_unblock
  cat "$CLIENT_OUT"
  line=$(last_line "$CLIENT_OUT" CONNECT-FAILED)
  assert "blocked at start: the connect failed (rc=$rc)" test "$rc" -eq 4
  assert "the daemon never accepted anything" test "$(ql_accepts)" -eq 0
  assert_num "refused after about connect_timeout (ms)" "$(kv "$line" after_ms)" -ge 1900
  assert_num "... and well before the idle timeout (ms)" "$(kv "$line" after_ms)" -le 2600
  note "blocked_at_start=refused after $(kv "$line" after_ms)ms (connect_timeout 2000ms)"
  stop_server

  # (b) blocked in the middle of a session and never lifted: Degraded, then Down(Dead).
  new_run qlblocked-mid
  ql_start_server
  ql_bg_client c --duration 30 --rate 262144
  wait_for_line "$CLIENT_OUT" CONNECTED 10 || fail "client did not connect"
  sleep 3
  local t
  t=$(now_ms)
  udp_block
  wait_exit "$CLIENT_PID" 40
  udp_unblock
  cat "$CLIENT_OUT"
  assert "mid-session: the client ended with an error status (rc=$WAIT_RC)" test "$WAIT_RC" -eq 4
  local degraded dead
  degraded=$(($(kv "$(grep -a '^LINK-STATE state=Degraded' "$CLIENT_OUT" | head -n 1)" at_ms) - t))
  dead=$(($(kv "$(grep -a '^LINK-STATE state=Down:Dead' "$CLIENT_OUT" | head -n 1)" at_ms) - t))
  assert_num "Degraded about degraded_after (2 s) after the block (ms)" "$degraded" -ge 1500
  assert_num "... and not much later (ms)" "$degraded" -le 3500
  assert_num "Down(Dead) about the idle timeout (5 s) after the block (ms)" "$dead" -ge 4000
  assert_num "... and not much later (ms)" "$dead" -le 7000
  note "blocked_mid_session: Degraded after ${degraded}ms, Down(Dead) after ${dead}ms (dead_after 5000ms)"
  stop_server
}

# A blackout of DURATION seconds in the middle of a session. The client reconnects (a fresh
# handshake with the same key) if the link is lost, as the path manager does.
quiclink_blackout_case() { # quiclink_blackout_case SECONDS EXPECT(survive|lost|either)
  local dur=$1 expect=$2 label="qlblackout-$1s"
  reset_net
  netem_apply delay 20ms
  new_run "$label"
  ql_start_server
  ql_bg_client c --duration $((3 + ${dur%.*} + 8)) --rate 262144 --reconnect --reconnect-for 40
  wait_for_line "$CLIENT_OUT" CONNECTED 10 || fail "client did not connect: $(cat "$CLIENT_OUT")"
  sleep 3
  local t_block t_heal
  t_block=$(now_ms)
  udp_block
  sleep "$dur"
  udp_unblock
  t_heal=$(now_ms)
  wait_exit "$CLIENT_PID" 60
  cat "$CLIENT_OUT"
  local line lost=false
  line=$(last_line "$CLIENT_OUT" RESULT)
  assert "$label: the client finished (rc=$WAIT_RC)" test "$WAIT_RC" -eq 0
  if [[ $(kv "$line" conn_lost) == true ]]; then
    lost=true
    assert "$label: it reconnected" grep -q '^RECONNECTED' "$CLIENT_OUT"
    assert "$label: the daemon accepted a second link" test "$(ql_accepts)" -eq 2
    local back
    back=$(($(kv "$(last_line "$CLIENT_OUT" RECONNECTED)" at_ms) - t_heal))
    note "$label=LOST (Down:Dead after $(($(kv "$(last_line "$CLIENT_OUT" CONN-LOST)" at_ms) - t_block))ms), fresh handshake ${back}ms after the link returned"
  else
    assert "$label: echo intact" test "$(kv "$line" sent)" = "$(kv "$line" recvd)"
    assert "$label: one link" test "$(ql_accepts)" -eq 1
    assert "$label: it reported Degraded" grep -q '^LINK-STATE state=Degraded' "$CLIENT_OUT"
    assert "$label: and then Up again" awk '/^LINK-STATE state=Degraded/ { d = 1 } d && /^LINK-STATE state=Up/ { f = 1 } END { exit !f }' "$CLIENT_OUT"
    note "$label=survived, Degraded then Up"
  fi
  case $expect in
    survive) assert "$label: survived, as expected" test "$lost" = false ;;
    lost) assert "$label: lost, as expected" test "$lost" = true ;;
  esac
  stop_server
}

s_quiclink_blackouts() {
  quiclink_blackout_case 3.0 survive
  quiclink_blackout_case 4.5 either
  quiclink_blackout_case 8.0 lost
}

# ---------------------------------------------------------------------------------------------
# The real `fil`: client in the client namespace, sshd and the daemon in the server namespace
#
# The pipe needs an ssh path to the server namespace, so the harness runs the host's own sshd
# inside it (on 198.51.100.10:22), with a throwaway host key, a throwaway client key and two
# throwaway users made for the run: rnclient (runs the client, on a PTY) and rnfil (the account on
# the "server"). Everything is removed again at the end. The ssh path crosses the same router, NAT
# and netem as the UDP path; only UDP is ever blocked, so the pipe keeps working while QUIC does not.
# `fil srv` finds the UDP address the way a user's setup would: `ssh -G srv` says HostName.

UC_HOME=/home/rnclient
FIL_BINS=(fil ptydrive sockprobe)
PD=/tmp/rn-pd-udp
# The programs live in a directory of their own, put first on the PATH of the client and (through
# sshd's SetEnv) of the sessions sshd starts. Nothing is installed into /usr/local/bin. The
# directory is made fresh by `mktemp -d` in udp_fil_setup, so nobody can plant one at a fixed path
# first on a shared machine.
UDP_BIN=""
UDP_PATH=""

client_exec_on() {
  shift
  nsx "$NS_C" runuser -u rnclient -- env -i "HOME=$UC_HOME" USER=rnclient LOGNAME=rnclient \
    "PATH=$UDP_PATH" "$@"
}
client_bg_on() {
  shift
  nsx "$NS_C" runuser -u rnclient -- env -i "HOME=$UC_HOME" USER=rnclient LOGNAME=rnclient \
    "PATH=$UDP_PATH" "$@" > /dev/null 2>&1 &
}

udp_fil_setup() {
  local c k=$OUT/run/udp-keys
  for c in sshd runuser useradd userdel ssh-keygen; do
    command -v "$c" > /dev/null || [[ -x /usr/sbin/$c ]] || { echo "realnet: '$c' is required (apt: openssh-server)" >&2; exit 2; }
  done
  rm -rf "$k"
  mkdir -p "$k"
  UDP_BIN=$(as_root mktemp -d /tmp/rn-udp-bin.XXXXXX)
  as_root chmod 755 "$UDP_BIN"
  UDP_PATH=$UDP_BIN:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
  # Each scenario runs in a fresh process that sources this file (part_main --one), so the
  # per-run directory must be written down here, as ssh.sh and sysd.sh do with their state.
  printf 'UDP_BIN=%q\nUDP_PATH=%q\n' "$UDP_BIN" "$UDP_PATH" > "$OUT/udp.env"
  ssh-keygen -q -t ed25519 -N '' -C realnet-udp-user -f "$k/id"
  ssh-keygen -q -t ed25519 -N '' -C realnet-udp-host -f "$k/host"
  cat > "$k/sshd_config" << EOF
Port 22
ListenAddress 198.51.100.10
HostKey /etc/fil-realnet-udp/host_ed25519
PidFile /etc/fil-realnet-udp/sshd.pid
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
UsePAM no
AuthorizedKeysFile /etc/fil-realnet-udp/authorized_keys
StrictModes no
AllowUsers rnfil
PermitTTY yes
LogLevel VERBOSE
# ssh srv fil server --stdio must find the fil under test.
SetEnv PATH=$UDP_PATH
EOF
  cat > "$k/ssh_config" << 'EOF'
Host srv
  HostName 198.51.100.10
  User rnfil
  IdentityFile ~/.ssh/id_ed25519
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  BatchMode yes
  ConnectTimeout 10
EOF
  as_root install -d -m 755 /etc/fil-realnet-udp /run/sshd
  as_root install -m 644 "$k/id.pub" /etc/fil-realnet-udp/authorized_keys
  as_root install -m 600 "$k/host" /etc/fil-realnet-udp/host_ed25519
  as_root install -m 644 "$k/sshd_config" /etc/fil-realnet-udp/sshd_config
  for c in rnclient rnfil; do
    as_root useradd -m -s /bin/bash -p '*' "$c"
  done
  as_root install -d -o rnclient -g rnclient -m 700 "$UC_HOME/.ssh"
  as_root install -m 600 -o rnclient -g rnclient "$k/id" "$UC_HOME/.ssh/id_ed25519"
  as_root install -m 600 -o rnclient -g rnclient "$k/ssh_config" "$UC_HOME/.ssh/config"
  # The daemon never retires on its own, so scenarios cannot race it.
  as_root install -d -o rnfil -g rnfil -m 700 /home/rnfil/.config /home/rnfil/.config/fil
  printf 'idle_exit_secs = 0\n' | as_root tee /home/rnfil/.config/fil/daemon.toml > /dev/null
  as_root chown rnfil:rnfil /home/rnfil/.config/fil/daemon.toml
  for c in "${FIL_BINS[@]}"; do as_root install -m 755 "$BIN_DIR/$c" "$UDP_BIN/$c"; done
  nsx "$NS_S" /usr/sbin/sshd -D -E "$OUT/logs/sshd-udp.log" -f /etc/fil-realnet-udp/sshd_config &
  local i
  for ((i = 0; i < 100; i++)); do
    client_exec_on x ssh -T srv true > /dev/null 2>&1 && break
    sleep 0.2
  done
  client_exec_on x ssh -T srv true || { echo "realnet: ssh from the client namespace to the server namespace does not work" >&2; exit 1; }
  # The fil that a login finds is the one under test (sshd's SetEnv PATH took effect).
  [[ $(client_exec_on x ssh -T srv command -v fil) == "$UDP_BIN/fil" ]] \
    || { echo "realnet: 'ssh srv fil' does not find $UDP_BIN/fil: $(client_exec_on x ssh -T srv 'echo $PATH; command -v fil')" >&2; exit 1; }
  log "ssh path up: client namespace -> sshd in the server namespace ($(client_exec_on x ssh -V 2>&1))"
}

udp_fil_teardown() {
  local c
  for c in rnclient rnfil; do
    as_root userdel -r -f "$c" > /dev/null 2>&1 || true
  done
  [[ -z $UDP_BIN ]] || as_root rm -rf "$UDP_BIN"
  as_root rm -rf /etc/fil-realnet-udp "$PD" "$OUT/run/udp-keys"
}

# wait_log NAME TEXT SECONDS [MIN]: the client's log must have at least MIN (default 1) lines
# containing TEXT. Prints nothing; fails the scenario.
wait_log() {
  local i
  for ((i = 0; i < $3 * 10; i++)); do
    if [[ $(fc_count_log "$1" "$2") -ge ${4:-1} ]]; then return 0; fi
    sleep 0.1
  done
  fail "$1: the client log never showed '$2' (at least ${4:-1}) within $3 s: $(fc_log "$1" | tail -n 5)"
}

# The client's ssh process (the pipe), if it has one: its pid.
pipe_ssh() { client_exec_on x pgrep -P "$1" -x ssh | head -n 1 || true; }

# nat_move SPEC: from now on the client's UDP leaves the router as SPEC (ADDR:PORTRANGE), and the
# old mapping is gone. TCP (the ssh pipe) keeps the rule it had.
NAT_UDP=
nat_move() {
  if [[ -n $NAT_UDP ]]; then
    nsx "$NS_R" iptables -t nat -D POSTROUTING -o rs -p udp -s 10.0.1.0/24 -j SNAT --to-source "$NAT_UDP" --random
  fi
  NAT_UDP=$1
  nsx "$NS_R" iptables -t nat -I POSTROUTING 1 -o rs -p udp -s 10.0.1.0/24 -j SNAT --to-source "$NAT_UDP" --random
  nsx "$NS_R" conntrack -F > /dev/null
  log "NAT mapping for UDP moved to $NAT_UDP"
}

# nat_expect SPEC: the router's conntrack table must show the client's UDP flow to the daemon
# mapped as SPEC says (ADDR:LOW-HIGH): the reply tuple's address is ADDR and its port in the range.
# This is what proves a move really happened to the flow under test, not merely that a rule was added.
nat_expect() {
  local addr=${1%%:*} prange=${1##*:} lo hi i line dst dport
  lo=${prange%-*}
  hi=${prange#*-}
  for ((i = 0; i < 40; i++)); do
    line=$(nsx "$NS_R" conntrack -L -p udp 2> /dev/null | grep -E 'src=10\.0\.1\.2 .*dport=6[0-9]{4} ' | tail -n 1 || true)
    if [[ -n $line ]]; then
      # The second dst= and the second dport= are those of the reply tuple: the NATed side.
      dst=$(awk '{ n = 0; for (i = 1; i <= NF; i++) if ($i ~ /^dst=/ && ++n == 2) print substr($i, 5) }' <<< "$line")
      dport=$(awk '{ n = 0; for (i = 1; i <= NF; i++) if ($i ~ /^dport=/ && ++n == 2) print substr($i, 7) }' <<< "$line")
      if [[ $dst == "$addr" && -n $dport && $dport -ge $lo && $dport -le $hi ]]; then
        ok "conntrack: the client's UDP flow is now $dst:$dport (wanted $1)"
        return 0
      fi
    fi
    sleep 0.25
  done
  fail "the NAT mapping did not move to $1: conntrack has: $(nsx "$NS_R" conntrack -L -p udp 2>&1)"
}

# fil_attach_direct NAME SESSION: attach over `auto`, wait for the shell and for the move to direct.
# Sets UP_MS (from the attach reply, the log's DirectAvailable, to Switched(Direct)), START_MS (from
# starting the process) and FIL_PID. Fails if the log does not give the times.
fil_attach_direct() {
  local name=$1 session=$2 t0 t_reply t_up
  t0=$(now_ms)
  fc_start "$name" -- srv "$session"
  fc_ready "$name"
  note "attach_to_shell_ms=$(($(now_ms) - t0))"
  wait_log "$name" 'Switched(Direct)' 15
  t_reply=$(fc_log_ms "$name" 'DirectAvailable')
  t_up=$(fc_log_ms "$name" 'Switched(Direct)')
  [[ $t_reply =~ ^[0-9]+$ && $t_up =~ ^[0-9]+$ ]] \
    || fail "$name: cannot read the times from the client log (DirectAvailable '$t_reply', Switched(Direct) '$t_up')"
  UP_MS=$((t_up - t_reply))
  START_MS=$((t_up - t0))
  FIL_PID=$(fc_pid "$name")
  note "direct_after_attach_reply_ms=$UP_MS" "direct_after_start_ms=$START_MS"
  local i
  for ((i = 0; i < 50; i++)); do
    [[ -z $(pipe_ssh "$FIL_PID") ]] && break
    sleep 0.1
  done
  [[ -z $(pipe_ssh "$FIL_PID") ]] || fail "the pipe's ssh is still running after the switch to direct"
  ok "on direct: the pipe's ssh is closed ($UP_MS ms after the attach reply, $START_MS ms after the start)"
}

# Output that keeps coming: TICK-1 .. TICK-N, a quarter second apart.
fc_ticks() { fc_line "$1" "for i in \$(seq 1 $2); do echo TICK-\$i; sleep 0.25; done"; }

# Upgrade to QUIC through NAT and 40 ms of round trip, and the session works over it.
f_upgrade() {
  reset_net
  netem_apply delay 20ms
  fil_attach_direct a up
  # DESIGN: "moves to QUIC within a second". The runner measures 0.17 s after the reply (0.9 s after
  # the process start, most of it the ssh login); 1.5 s is the margin for a noisy VM.
  assert_num "moved to direct within 1.5 s of the attach reply (ms)" "$UP_MS" -lt 1500
  fc_line a 'echo UP-$((6*7))'
  fc_wait a UP-42
  fc_ticks a 40
  fc_wait a TICK-40 30
  fc_seq_ok a TICK 1 40 || fail "ticks over direct: $(fc_screen a)"
  [[ $(fc_count_log a 'Switched(Pipe)') -eq 1 && $(fc_count_log a 'Switched(Direct)') -eq 1 ]] \
    || fail "unexpected switches: $(fc_log a | grep -F 'Switched(')"
  # The router's conntrack table has the UDP flow to the daemon's port.
  nsx "$NS_R" conntrack -L -p udp 2> /dev/null | grep -E "dport=6[0-9]{4}" || fail "no UDP flow to the daemon's port in conntrack"
  ok "the router carries a UDP flow to the daemon's port"
  fc_detach a
  cx_fk up
}

cx_fk() { client_exec_on x fil kill srv "$1" > /dev/null 2>&1 || true; }

# NAT mappings change under a live session (new port, another new port, a new address): it stays on
# direct, and no output is lost.
f_nat_rebind() {
  reset_net
  netem_apply delay 20ms
  NAT_UDP=
  fil_attach_direct a nr
  fc_ticks a 90
  fc_wait a TICK-6 20
  local spec
  for spec in "$PUB1:31000-31999" "$PUB1:32000-32999" "$PUB3:33000-33999"; do
    sleep 4
    nat_move "$spec"
    nat_expect "$spec"
  done
  fc_wait a TICK-90 40
  fc_seq_ok a TICK 1 90 || fail "output lost or repeated across the NAT moves: $(fc_screen a)"
  ok "TICK-1 to TICK-90 each once, in order, across three NAT moves"
  [[ $(fc_count_log a 'Switched(') -eq 2 ]] || fail "the client switched transports again: $(fc_log a | grep -F 'Switched(')"
  [[ -z $(pipe_ssh "$FIL_PID") ]] || fail "the client fell back to an ssh pipe"
  ok "still on direct: no switch after the upgrade, no ssh"
  if fc_has a 'skipped'; then fail "a gap marker on the screen"; fi
  fc_detach a
  cx_fk nr
}

# UDP stops for good in the middle of a session: the client falls back to the pipe and the output
# carries on without loss. (Then UDP returns; whether and when the client goes back is recorded.)
f_udp_blocked_mid() {
  reset_net
  netem_apply delay 20ms
  fil_attach_direct a bm
  fc_ticks a 120
  fc_wait a TICK-8 20
  local t0 pipes0
  pipes0=$(fc_count_log a 'Switched(Pipe)')
  t0=$(now_ms)
  udp_block
  local i ssh_pid=
  for ((i = 0; i < 300; i++)); do
    ssh_pid=$(pipe_ssh "$FIL_PID")
    [[ -n $ssh_pid ]] && break
    sleep 0.1
  done
  [[ -n $ssh_pid ]] || fail "no fallback to the pipe after 30 s of blocked UDP: $(fc_screen a | tail -n 8)"
  note "pipe_back_ms=$(($(now_ms) - t0))"
  # A Switched(Pipe) after the block (the attach itself logged the first one).
  wait_log a 'Switched(Pipe)' 10 $((pipes0 + 1))
  fc_wait a TICK-120 60
  fc_seq_ok a TICK 1 120 || fail "output lost or repeated across the fallback: $(fc_screen a)"
  ok "TICK-1 to TICK-120 each once, in order, across the fallback"
  [[ $(fc_count_log a 'Switched(Pipe)') -ge 2 ]] || fail "no switch back to the pipe in the log"
  udp_unblock
  # Does it return to direct when UDP does? Recorded, not required.
  local t1 back=no
  t1=$(now_ms)
  for ((i = 0; i < 450; i++)); do
    if [[ $(fc_count_log a 'Switched(Direct)') -ge 2 ]]; then
      back=$(($(now_ms) - t1))
      break
    fi
    sleep 0.1
  done
  note "back_to_direct_after_unblock_ms=$back"
  fc_line a 'echo AFTER-$((6*7))'
  fc_wait a AFTER-42
  fc_detach a
  cx_fk bm
}

# UDP never works: the session stays on the pipe, and comes up as fast as ssh does.
f_udp_blocked_start() {
  reset_net
  netem_apply delay 20ms
  udp_block
  local t0
  t0=$(now_ms)
  fc_start a -- srv bs
  fc_ready a
  note "attach_to_shell_ms=$(($(now_ms) - t0))"
  local pid
  pid=$(fc_pid a)
  fc_ticks a 60
  fc_wait a TICK-60 40
  fc_seq_ok a TICK 1 60 || fail "ticks over the pipe: $(fc_screen a)"
  [[ $(fc_count_log a 'Switched(Pipe)') -ge 1 ]] || fail "the client log is empty or missing (no Switched(Pipe)): $(fc_log a | tail -n 3)"
  [[ $(fc_count_log a 'Switched(Direct)') -eq 0 ]] || fail "it switched to direct through a UDP block"
  [[ -n $(pipe_ssh "$pid") ]] || fail "the pipe's ssh is not running"
  note "direct_dial_failures=$(fc_count_log a 'direct dial failed')"
  ok "15 s on the pipe, never on direct; the ssh pipe is running"
  udp_unblock
  fc_detach a
  cx_fk bs
}

# Retired from the default run (still runnable by name): s_quiclink_rebind is covered by
# f_nat_rebind, which moves the NAT mapping the same way under the real client and daemon.
ALL_SCENARIOS=(
  s_netem_reorder_probe s_clean s_port_range s_impaired
  s_nat_rebind s_ip_change s_blackouts s_udp_blocked
  s_quiclink_udp_blocked s_quiclink_blackouts
  f_upgrade f_nat_rebind f_udp_blocked_mid f_udp_blocked_start
)

part_main udp udp_setup udp_teardown "${ALL_SCENARIOS[@]}" -- "$@"
