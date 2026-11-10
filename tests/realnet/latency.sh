#!/usr/bin/env bash
# The latency lab (docs/m2/latency.md): the measurements behind OD-10 and the thresholds for H1.
# It runs in the udp part's topology (network namespaces, NAT, tc netem, the runner's own sshd in
# the server namespace; see udp.sh), so it needs root and the same tools, plus strace and python3.
# It is NOT part of `run.sh all`: `just realnet latency`, or the linux-check workflow with
# `-f suite=latency`. Run: latency.sh [SCENARIO...]
#
#   l_nodelay       1. does ssh (client and sshd) set TCP_NODELAY for `ssh -T` and for `ssh -tt`?
#   l_keys_idle     2. keystroke-to-echo p50/p99 at 5, 20 and 50 keys/s, 40 ms round trip, +-5 ms per direction,
#                      through `ssh -tt`, fil on the pipe and fil on QUIC (and with no netem)
#   l_keys_bulk     3. the same while a bulk transfer runs through `ssh -L`, up and down, with the
#                      link rate-limited so a queue forms: the baseline fil's forwards are held to
#   l_catchup       4. how often a 4 MB `cat` makes the daemon skip ahead (catch-up) with a fast
#                      discarding consumer, on the pipe and on QUIC (the Pi procedure is in the doc)
#   l_ctrlc         5. Ctrl-C during a flood on a 1 Mbit/s link: how long until the next command's
#                      output shows, through `ssh -tt`, and fil on the pipe and on QUIC with the
#                      terminal paced and not (FIL_TERMINAL_PACING=off; M3 K1, DESIGN.md §8.5)
#
# Measurements go to the results table (notes) and, per keystroke cell, to
# $OUT/results-latency-cells.tsv (also appended to the job summary). Knobs (environment):
#   LAT_DELAY_MS=20  LAT_JITTER_MS=5   one-way netem delay and jitter (40 ms round trip, +-5 ms per direction)
#   LAT_SECS=20      minimum seconds of typing per cell (at least 300 keys)
#   LAT_BULK_RATE=20mbit LAT_BULK_LIMIT=500   the bottleneck of l_keys_bulk (rate; queue, packets)
#   LAT_CATCH_RUNS=5 runs of the 4 MB cat per cell in l_catchup
#   LAT_SLOW_RATE=1mbit LAT_SLOW_LIMIT=100   the link of l_ctrlc (rate; queue, packets)
set -uo pipefail
# shellcheck source=udp.sh
source "$(dirname "${BASH_SOURCE[0]}")/udp.sh"

FIL_BINS+=(keyprobe)

LAT_DELAY_MS=${LAT_DELAY_MS:-20}
LAT_JITTER_MS=${LAT_JITTER_MS:-5}
LAT_SECS=${LAT_SECS:-20}
LAT_BULK_RATE=${LAT_BULK_RATE:-20mbit}
LAT_BULK_LIMIT=${LAT_BULK_LIMIT:-500}
LAT_CATCH_RUNS=${LAT_CATCH_RUNS:-5}
LAT_SLOW_RATE=${LAT_SLOW_RATE:-1mbit}
LAT_SLOW_LIMIT=${LAT_SLOW_LIMIT:-100}
LAT_CELLS=$OUT/results-latency-cells.tsv
LAT_RATES=(5 20 50)
SSHD_TRACE_PORT=2222

# ---------------------------------------------------------------------------------------------
# Setup, teardown, the network

lat_setup() {
  udp_setup
  # The scenario processes read the part's env file, and udp_fil_setup wrote udp.env.
  cp "$OUT/udp.env" "$OUT/latency.env"
  printf 'cell\ttransport\tload\trate\tsent\techoed\tlost\tmin_ms\tp50_ms\tp90_ms\tp99_ms\tmax_ms\n' > "$LAT_CELLS"
  client_exec_on x mkdir -p "$PD"
}

lat_teardown() {
  lat_cells_summary
  udp_teardown "$@"
}

# The cells as a markdown table in the job summary.
lat_cells_summary() {
  [[ -n ${GITHUB_STEP_SUMMARY:-} && -s $LAT_CELLS ]] || return 0
  {
    echo '### latency lab: keystroke-to-echo (ms)'
    echo
    echo '| cell | transport | load | keys/s | sent | echoed | lost | min | p50 | p90 | p99 | max |'
    echo '|---|---|---|---|---|---|---|---|---|---|---|---|'
    tail -n +2 "$LAT_CELLS" | awk -F'\t' '{ printf "|"; for (i = 1; i <= NF; i++) printf " %s |", $i; printf "\n" }'
  } >> "$GITHUB_STEP_SUMMARY"
}

# lat_net rtt | clean | bottleneck: the network a cell runs on. `rate` is what makes netem's jitter
# keep packets in order (s_netem_reorder_probe); the bottleneck is the same with a real rate.
lat_net() {
  reset_net
  local d rate limit
  case $1 in
    clean) return 0 ;;
    rtt) rate=1gbit limit=100000 ;;
    bottleneck) rate=$LAT_BULK_RATE limit=$LAT_BULK_LIMIT ;;
    slow) rate=$LAT_SLOW_RATE limit=$LAT_SLOW_LIMIT ;;
    *) fail "lat_net: unknown network $1" ;;
  esac
  for d in "${NETEM_DEVS[@]}"; do
    nsx "$NS_R" tc qdisc replace dev "$d" root netem \
      delay "${LAT_DELAY_MS}ms" "${LAT_JITTER_MS}ms" rate "$rate" limit "$limit"
  done
  log "network: $1 (one-way ${LAT_DELAY_MS} ms +-${LAT_JITTER_MS} ms, rate $rate, queue $limit packets)"
}

lat_need() {
  local c
  for c in "$@"; do
    command -v "$c" > /dev/null || fail "'$c' is required for this scenario (apt: $c)"
  done
}

# lat_stop_listener NS PORT: stops the process listening on TCP PORT in namespace NS (by pid).
lat_stop_listener() {
  local pid
  pid=$(nsx "$1" ss -ltnpH "sport = :$2" 2> /dev/null | grep -o 'pid=[0-9]*' | head -n 1 | cut -d= -f2 || true)
  [[ -z $pid ]] || as_root kill "$pid" 2> /dev/null || true
}

# lat_wait_listen NS PORT: waits (10 s) until something listens on PORT in NS.
lat_wait_listen() {
  local i
  for ((i = 0; i < 100; i++)); do
    if [[ -n $(nsx "$1" ss -ltnH "sport = :$2" 2> /dev/null) ]]; then return 0; fi
    sleep 0.1
  done
  fail "nothing listens on $2 in $1"
}

# ---------------------------------------------------------------------------------------------
# 1. TCP_NODELAY

# l_nodelay: strace -f -e setsockopt on the ssh client and on a second sshd (port 2222 in the
# server namespace, same configuration), for `ssh -T` (no pty) and `ssh -tt` (a pty).
l_nodelay() {
  lat_need strace
  lat_net rtt
  local cfg=$OUT/run/lat-sshd-trace.conf sshd_trace=$OUT/run/lat-strace-sshd.txt
  sed -e "s/^Port 22\$/Port $SSHD_TRACE_PORT/" -e 's|^PidFile .*|PidFile none|' \
    /etc/fil-realnet-udp/sshd_config > "$cfg"
  as_root rm -f "$sshd_trace"
  nsx "$NS_S" strace -f -e trace=setsockopt -o "$sshd_trace" \
    /usr/sbin/sshd -D -E "$OUT/logs/sshd-trace.log" -f "$cfg" &
  # Global, not local: the EXIT trap runs after this function has returned.
  LAT_SSHD_PID=$!
  local i
  trap 'lat_stop_listener "$NS_S" "$SSHD_TRACE_PORT"; wait "$LAT_SSHD_PID" 2> /dev/null' EXIT
  for ((i = 0; i < 100; i++)); do
    client_exec_on x ssh -T -p "$SSHD_TRACE_PORT" srv true > /dev/null 2>&1 && break
    sleep 0.2
  done
  client_exec_on x ssh -T -p "$SSHD_TRACE_PORT" srv true || fail "the traced sshd does not answer on $SSHD_TRACE_PORT"

  local mode flag n0 cfile
  for mode in T tt; do
    flag=-$mode
    cfile=$PD/lat-strace-client-$mode.txt
    n0=$(as_root cat "$sshd_trace" | wc -l)
    client_exec_on x rm -f "$cfile"
    # A second of sleep keeps the session open long enough for every option to have been set.
    client_exec_on x strace -f -e trace=setsockopt -o "$cfile" \
      ssh "$flag" -p "$SSHD_TRACE_PORT" srv 'sleep 1' < /dev/null > /dev/null 2>&1 || true
    sleep 1
    client_exec_on x cat "$cfile" > "$OUT/run/lat-strace-client-$mode.txt"
    as_root cat "$sshd_trace" | tail -n "+$((n0 + 1))" > "$OUT/run/lat-strace-sshd-$mode.txt"
    local c s
    c=$(lat_count_nodelay "$OUT/run/lat-strace-client-$mode.txt")
    s=$(lat_count_nodelay "$OUT/run/lat-strace-sshd-$mode.txt")
    note "ssh_${mode}_client_nodelay=$c sshd_${mode}_nodelay=$s"
    log "ssh $flag: client TCP_NODELAY calls: $c; sshd: $s"
    log "client setsockopt calls ($mode): $(tr '\n' '|' < "$OUT/run/lat-strace-client-$mode.txt" | cut -c1-600)"
  done
  # The real pipe: the ssh that fil itself starts (`ssh -T -- HOST fil server --stdio`), client side.
  local fcfile=$PD/lat-strace-fil.txt
  client_exec_on x rm -f "$fcfile"
  client_exec_on x keyprobe keys --shell --rate 20 --count 20 --warmup 2 --drain-ms 3000 -- \
    strace -f -e trace=setsockopt -o "$fcfile" fil --predict off --transport pipe srv latnd > "$OUT/run/lat-strace-fil.keyprobe" 2>&1 \
    || log "the traced fil run failed: $(tail -n 3 "$OUT/run/lat-strace-fil.keyprobe")"
  cx_fk latnd
  client_exec_on x cat "$fcfile" > "$OUT/run/lat-strace-fil.txt" 2> /dev/null || true
  note "fil_pipe_client_nodelay=$(lat_count_nodelay "$OUT/run/lat-strace-fil.txt")"
  # Keystrokes travel client -> sshd (the client's socket decides) and the echo sshd -> client
  # (sshd's). Without TCP_NODELAY on a side, small writes in that direction can wait for an ACK.
  local mode verdict
  for mode in T tt; do
    verdict=no
    if [[ $(lat_count_nodelay "$OUT/run/lat-strace-client-$mode.txt") -eq 0 ]] \
      || [[ $(lat_count_nodelay "$OUT/run/lat-strace-sshd-$mode.txt") -eq 0 ]]; then
      verdict=yes
    fi
    note "ssh_${mode}_may_nagle=$verdict"
  done
  ok "the client and sshd setsockopt calls are recorded for both modes"
}

# lat_count_nodelay FILE: how many successful setsockopt(..., TCP_NODELAY, [1], ...) calls.
# (A call strace split into "<unfinished ...>" and "resumed" lines counts once, on its first line;
# one that failed does not count.)
lat_count_nodelay() { grep -a 'TCP_NODELAY, \[1\]' "$1" | grep -a -v -c '= -1' || true; }

# ---------------------------------------------------------------------------------------------
# 2 and 3. Keystroke latency

# lat_count RATE: keys to measure at RATE keys/s: LAT_SECS seconds of typing, at least 300 keys.
lat_count() {
  local n=$(($1 * LAT_SECS))
  if [[ $n -lt 300 ]]; then n=300; fi
  printf '%s' "$n"
}

# Every fil cell here (and in fwd.sh) runs with `--predict off`: these cells measure the server's
# echo, and since adaptive prediction became the default (M3 E1b) an idle cell's echo is otherwise
# drawn locally in well under a millisecond, which made H1's "2 x idle + 10 ms" line ~10 ms.
# Prediction's own latency is predict.sh's job.

# lat_kp TAG RATE SETTLE_MS -- COMMAND...: keyprobe in the client namespace, typing into COMMAND on
# a pty. Output in $OUT/run/SCENARIO-TAG.keyprobe (and .err). Returns keyprobe's status. The fil
# client logs at debug into a state directory of its own (checked afterwards).
lat_kp() {
  local tag=$1 rate=$2 settle=$3 count
  shift 4
  count=$(lat_count "$rate")
  client_exec_on x timeout $((count / rate + 200)) env FIL_LOG=debug "XDG_STATE_HOME=$PD/state-$SCENARIO-$tag" \
    keyprobe keys --shell --rate "$rate" --count "$count" --warmup 10 --settle-ms "$settle" -- "$@" \
    > "$OUT/run/$SCENARIO-$tag.keyprobe" 2> "$OUT/run/$SCENARIO-$tag.err"
}

# lat_finish TRANSPORT TAG LOAD RATE: reads the keyprobe line, notes it, records the cell. Sets
# LAT_P50, LAT_P99.
lat_finish() {
  local transport=$1 tag=$2 load=$3 rate=$4 line
  line=$(last_line "$OUT/run/$SCENARIO-$tag.keyprobe" KEYPROBE)
  [[ -n $line ]] || fail "$tag: keyprobe printed no result: $(cat "$OUT/run/$SCENARIO-$tag.err" 2> /dev/null)"
  local sent echoed lost
  sent=$(kv "$line" sent)
  echoed=$(kv "$line" echoed)
  lost=$(kv "$line" lost)
  LAT_P50=$(kv "$line" p50_ms)
  LAT_P99=$(kv "$line" p99_ms)
  note "$tag.p50_ms=$LAT_P50 $tag.p99_ms=$LAT_P99 $tag.lost=$lost"
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$tag" "$transport" "$load" "$rate" "$sent" "$echoed" "$lost" \
    "$(kv "$line" min_ms)" "$LAT_P50" "$(kv "$line" p90_ms)" "$LAT_P99" "$(kv "$line" max_ms)" >> "$LAT_CELLS"
  if [[ $lost != 0 ]]; then known_bug "$tag: $lost of $sent keystrokes were never echoed"; fi
}

# lat_check_path TAG KIND FIRST_KEY_MS: the fil client's log must show the path the cell claims:
# pipe: never on QUIC; quic: on QUIC before the first measured key.
lat_check_path() {
  local tag=$1 kind=$2 first=$3 sw n back
  printf -v "FC_WHERE_$tag" x
  case $kind in
    pipe)
      [[ $(fc_count_log "$tag" 'Switched(Direct)') -eq 0 ]] || fail "$tag: the pipe cell moved to QUIC"
      ;;
    quic)
      sw=$(fc_log_ms "$tag" 'Switched(Direct)')
      [[ $sw =~ ^[0-9]+$ ]] || fail "$tag: the client never moved to QUIC: $(fc_log "$tag" | tail -n 3)"
      [[ $sw -lt $first ]] || fail "$tag: the move to QUIC ($sw) came after the first key ($first)"
      # And no fall back to the pipe after it.
      n=$(fc_count_log "$tag" 'Switched(Pipe)')
      if [[ $n -gt 0 ]]; then
        back=$(fc_log_ms "$tag" 'Switched(Pipe)' "$n")
        [[ ! $back =~ ^[0-9]+$ || $back -lt $sw ]] || fail "$tag: the path fell back to the pipe during the cell"
      fi
      ;;
  esac
  ok "$tag: on the $kind path for the whole cell"
}

# lat_save_log TAG: the fil client's debug log of the cell, kept in the artifact (the test users
# and their state directories are removed at the end of the run).
lat_save_log() {
  printf -v "FC_WHERE_$1" x
  fc_log "$1" > "$OUT/run/$SCENARIO-$1.clientlog"
}

# lat_cell KIND RATE LOAD: one cell. KIND is ssh (`ssh -tt`), pipe or quic (fil with
# --transport pipe|direct).
lat_cell() {
  local kind=$1 rate=$2 load=$3 tag settle=500 cmd
  case $kind in
    ssh) tag=s$load$rate cmd=(ssh -tt srv) ;;
    sshno) tag=n$load$rate cmd=(ssh -tt -o ObscureKeystrokeTiming=no srv) ;;
    pipe) tag=p$load$rate cmd=(fil --predict off --transport pipe srv "latp$load$rate") ;;
    quic) tag=q$load$rate cmd=(fil --predict off --transport direct srv "latq$load$rate") settle=3000 ;;
  esac
  log "=== cell $tag: $kind at $rate keys/s ($load)"
  lat_kp "$tag" "$rate" "$settle" -- "${cmd[@]}" || fail "$tag: keyprobe failed: $(cat "$OUT/run/$SCENARIO-$tag.err" "$OUT/run/$SCENARIO-$tag.keyprobe" 2> /dev/null | tail -n 5)"
  lat_finish "$kind" "$tag" "$load" "$rate"
  if [[ $kind == pipe || $kind == quic ]]; then
    lat_save_log "$tag"
    lat_check_path "$tag" "$kind" "$(kv "$(last_line "$OUT/run/$SCENARIO-$tag.keyprobe" KEYPROBE)" first_key_ms)"
    cx_fk "${cmd[-1]}"
  fi
}

# l_keys_idle: the nine cells (three transports, three rates) on the 40 ms network, and one cell
# per transport with no netem at all (the cost of fil itself).
l_keys_idle() {
  lat_net rtt
  client_exec_on x mkdir -p "$PD"
  local kind rate
  for kind in ssh pipe quic; do
    for rate in "${LAT_RATES[@]}"; do lat_cell "$kind" "$rate" rtt; done
  done
  # ssh 9.5 and later send keystrokes of an interactive session at 20 ms intervals
  # (ObscureKeystrokeTiming). One more path with that off, to see what it costs.
  lat_cell sshno 20 rtt
  lat_net clean
  for kind in ssh sshno pipe quic; do lat_cell "$kind" 20 local; done
  lat_compare 20
}

# lat_compare RATE: fil's p50 and p99 minus ssh -tt's at RATE keys/s on the 40 ms network.
lat_compare() {
  local r=$1 kind f
  note "ssh_minus_sshno.p50_ms_at_$r=$(awk -v a="$(lat_cell_field "$(lat_tag_of ssh rtt "$r")" p50_ms)" \
    -v b="$(lat_cell_field "$(lat_tag_of sshno rtt "$r")" p50_ms)" 'BEGIN { printf "%+.1f", a - b }')"
  for kind in pipe quic; do
    for f in p50_ms p99_ms; do
      note "${kind}_minus_ssh.${f}_at_$r=$(awk -v a="$(lat_cell_field "$(lat_tag_of "$kind" rtt "$r")" "$f")" \
        -v b="$(lat_cell_field "$(lat_tag_of ssh rtt "$r")" "$f")" 'BEGIN { printf "%+.1f", a - b }')"
    done
  done
}

lat_tag_of() {
  case $1 in
    ssh) printf 's%s%s' "$2" "$3" ;;
    sshno) printf 'n%s%s' "$2" "$3" ;;
    pipe) printf 'p%s%s' "$2" "$3" ;;
    quic) printf 'q%s%s' "$2" "$3" ;;
  esac
}

# lat_cell_field TAG FIELD: from the keyprobe line.
lat_cell_field() { kv "$(last_line "$OUT/run/$SCENARIO-$1.keyprobe" KEYPROBE)" "$2"; }

# The bulk programs. A sink and a source on loopback ports in the server namespace, reached by
# `ssh -L`. Run as the server-side user, in python3 (always on a runner).
LAT_PY=$(
  cat << 'EOF'
import socket, sys, threading

def serve(port, handler):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))
    s.listen(8)
    while True:
        c, _ = s.accept()
        threading.Thread(target=handler, args=(c,), daemon=True).start()

def sink(c):
    try:
        while c.recv(1 << 20):
            pass
    except OSError:
        pass
    c.close()

def source(c):
    buf = bytes(1 << 16)
    try:
        while True:
            c.sendall(buf)
    except OSError:
        pass
    c.close()

threading.Thread(target=serve, args=(int(sys.argv[1]), sink), daemon=True).start()
serve(int(sys.argv[2]), source)
EOF
)
LAT_SINK_PORT=19100   # server side: bulk in (client -> server)
LAT_SOURCE_PORT=19101 # server side: bulk out (server -> client)
LAT_FWD_UP=19000      # client side: -L to the sink
LAT_FWD_DOWN=19001    # client side: -L to the source

# lat_tx_bytes DEV: bytes the router's netem on DEV has sent.
lat_tx_bytes() { nsx "$NS_R" tc -s qdisc show dev "$1" | awk '/ Sent /{ print $2; exit }'; }

lat_bulk_cleanup() {
  lat_stop_listener "$NS_S" "$LAT_SINK_PORT"
  lat_stop_listener "$NS_S" "$LAT_SOURCE_PORT"
}

# l_keys_bulk: keystrokes through `ssh -tt -L ...` (the typing and the forward share one ssh
# connection, one TCP connection) at 20 keys/s: idle, with a bulk upload through the forward, and
# with a bulk download, on a link of LAT_BULK_RATE with a short queue. The baseline for H1.
l_keys_bulk() {
  lat_need python3
  lat_net bottleneck
  client_exec_on x mkdir -p "$PD"
  trap lat_bulk_cleanup EXIT
  nsx "$NS_S" runuser -u rnfil -- python3 -c "$LAT_PY" "$LAT_SINK_PORT" "$LAT_SOURCE_PORT" > "$OUT/run/$SCENARIO-bulk-server.log" 2>&1 &
  lat_wait_listen "$NS_S" "$LAT_SOURCE_PORT"
  lat_wait_listen "$NS_S" "$LAT_SINK_PORT"

  local load rate=20 tag kp bulk b0 b1 t0 t1 dev mbit
  for load in idle up down; do
    tag=sL$load
    log "=== cell $tag: ssh -tt -L, bulk $load, $LAT_BULK_RATE"
    lat_kp "$tag" "$rate" 500 -- ssh -tt \
      -L "127.0.0.1:$LAT_FWD_UP:127.0.0.1:$LAT_SINK_PORT" -L "127.0.0.1:$LAT_FWD_DOWN:127.0.0.1:$LAT_SOURCE_PORT" srv &
    kp=$!
    lat_wait_listen "$NS_C" "$LAT_FWD_UP"
    bulk=
    case $load in
      up)
        dev=rs
        client_exec_on x timeout 200 bash -c "exec 3<>/dev/tcp/127.0.0.1/$LAT_FWD_UP; head -c 16G /dev/zero >&3" > /dev/null 2>&1 &
        bulk=$!
        ;;
      down)
        dev=rc0
        client_exec_on x timeout 200 bash -c "exec 3<>/dev/tcp/127.0.0.1/$LAT_FWD_DOWN; cat <&3 > /dev/null" > /dev/null 2>&1 &
        bulk=$!
        ;;
    esac
    # Let the queue fill before reading the byte counter (keyprobe's own setup takes a while too).
    if [[ -n $bulk ]]; then
      sleep 6
      b0=$(lat_tx_bytes "$dev")
      t0=$(now_ms)
    fi
    local rc=0
    wait "$kp" || rc=$?
    if [[ -n $bulk ]]; then
      b1=$(lat_tx_bytes "$dev")
      t1=$(now_ms)
      mbit=$(awk -v b0="$b0" -v b1="$b1" -v t0="$t0" -v t1="$t1" 'BEGIN { printf "%.1f", (b1 - b0) * 8 / ((t1 - t0) / 1000) / 1e6 }')
      note "$tag.bulk_mbit=$mbit"
      # The forward died with the ssh that keyprobe hung up, which ends the transfer.
      wait "$bulk" 2> /dev/null || true
      [[ $mbit == *[0-9]* ]] && awk -v m="$mbit" 'BEGIN { exit !(m >= 2) }' \
        || fail "$tag: the bulk transfer moved $mbit Mbit/s: the link was not loaded"
    fi
    [[ $rc -eq 0 ]] || fail "$tag: keyprobe failed: $(tail -n 3 "$OUT/run/$SCENARIO-$tag.err" 2> /dev/null)"
    lat_finish "ssh-L" "$tag" "$load" "$rate"
  done

  # H1's rule (docs/m2/realnet.md, "H1 trigger (2x idle + 10)"): p50 under a bulk forward above 2x idle plus 10 ms.
  local idle p50
  idle=$(lat_cell_field sLidle p50_ms)
  for load in up down; do
    p50=$(lat_cell_field "sL$load" p50_ms)
    note "ssh_L_$load.p50_vs_idle=$(awk -v p="$p50" -v i="$idle" 'BEGIN { printf "%.1fx", p / i }') h1_rule_exceeded=$(awk -v p="$p50" -v i="$idle" 'BEGIN { print (p > 2 * i + 10) ? "yes" : "no" }')"
  done
}

# ---------------------------------------------------------------------------------------------
# 4. Catch-up

# lat_drain_field FILE KEY: KEY of each DRAIN run line, one per line.
lat_drain_field() {
  local l
  grep -a '^DRAIN run=' "$1" | while read -r l; do
    kv "$l" "$2"
    echo
  done
}

# l_catchup: a 4 MB `cat` (55000 lines of 73 bytes, 4.0 MB) read by a consumer that parses nothing and throws
# everything away (keyprobe drain), LAT_CATCH_RUNS times per cell: how many runs make the daemon
# jump to a snapshot (the `fil: skipped` marker). `ssh -tt` is the control: it must deliver
# every line. The clean network and the 40 ms one; the pipe and QUIC.
l_catchup() {
  client_exec_on x mkdir -p "$PD"
  local net kind tag cmd line sum
  # Global, not local: the EXIT trap runs after this function has returned.
  big=/tmp/rn-lat-big.txt
  # The file is made by the server-side user in a sticky /tmp.
  trap 'as_root rm -f "$big" || true' EXIT
  for net in clean rtt; do
    lat_net "$net"
    for kind in ssh pipe quic; do
      tag=c$kind$net
      case $kind in
        ssh) cmd=(ssh -tt srv) ;;
        pipe) cmd=(fil --predict off --transport pipe srv "lat$tag") ;;
        quic) cmd=(fil --predict off --transport direct srv "lat$tag") ;;
      esac
      log "=== catch-up cell $tag"
      local settle=500
      [[ $kind != quic ]] || settle=3000
      client_exec_on x timeout $((LAT_CATCH_RUNS * 150 + 200)) env FIL_LOG=debug "XDG_STATE_HOME=$PD/state-$SCENARIO-$tag" \
        keyprobe drain --settle-ms "$settle" \
        --setup "seq -f 'LINE-%06g 0123456789abcdefghij0123456789abcdefghij0123456789abcdefghij' 1 55000 > $big; echo KP-READY-\$((6*7))" \
        --ready KP-READY-42 \
        --run "cat $big; echo CATEND-\$((6*7))" --end CATEND-42 --tally LINE- \
        --repeat "$LAT_CATCH_RUNS" --timeout 150 -- "${cmd[@]}" \
        > "$OUT/run/$SCENARIO-$tag.drain" 2> "$OUT/run/$SCENARIO-$tag.err" \
        || fail "$tag: keyprobe drain failed: $(tail -n 3 "$OUT/run/$SCENARIO-$tag.err" 2> /dev/null) $(cat "$OUT/run/$SCENARIO-$tag.drain")"
      cat "$OUT/run/$SCENARIO-$tag.drain" >&2
      sum=$(last_line "$OUT/run/$SCENARIO-$tag.drain" DRAIN-SUMMARY)
      printf -v "FC_WHERE_$tag" x
      note "catchup.$kind.$net=$(kv "$sum" runs_with_marks)/$(kv "$sum" runs) marks=$(kv "$sum" marks) ms=$(lat_drain_field "$OUT/run/$SCENARIO-$tag.drain" ms | paste -sd, -)"
      if [[ $kind == ssh ]]; then
        # The control: every one of the 55000 lines reached the consumer, in every run.
        for line in $(lat_drain_field "$OUT/run/$SCENARIO-$tag.drain" tally); do
          [[ $line -eq 55000 ]] || fail "$tag: the consumer saw $line of 55000 lines through ssh: it is not a fast consumer"
        done
      else
        lat_save_log "$tag"
        cx_fk "${cmd[-1]}"
      fi
      if [[ $kind == quic ]]; then lat_check_path "$tag" quic 9999999999999 > /dev/null; fi
    done
  done
}

# lat_term_pacing on|off: the server's daemon (one per user) restarted with
# FIL_TERMINAL_PACING=$1, so the sessions of the next attaches pace their terminal or not.
lat_term_pacing() {
  local i pid
  lat_daemon_pid() { nsx "$NS_S" pgrep -u rnfil -f 'fil server --daemon' | head -n 1 || true; }
  pid=$(lat_daemon_pid)
  [[ -z $pid ]] || as_root kill "$pid" 2> /dev/null || true
  for ((i = 0; i < 100; i++)); do
    [[ -z $(lat_daemon_pid) ]] && break
    sleep 0.1
  done
  nsx "$NS_S" runuser -u rnfil -- env -i HOME=/home/rnfil USER=rnfil LOGNAME=rnfil "PATH=$UDP_PATH" \
    "FIL_TERMINAL_PACING=$1" setsid "$UDP_BIN/fil" server --daemon < /dev/null > /dev/null 2>&1 &
  for ((i = 0; i < 100; i++)); do
    [[ -n $(lat_daemon_pid) ]] && break
    sleep 0.1
  done
  [[ -n $(lat_daemon_pid) ]] || fail "the daemon did not start (FIL_TERMINAL_PACING=$1)"
}

# l_ctrlc: a flood of numbered lines on a 1 Mbit/s link (LAT_SLOW_RATE, one-way LAT_DELAY_MS),
# Ctrl-C three seconds in, then `echo ALIVE-42`: the time from the Ctrl-C to ALIVE-42 on the
# screen. `ssh -tt` is the reference (it delivers every byte: its time is the backlog in ssh's
# window and the router's queue); fil paced is K1's cell (at most 3 s on QUIC), fil unpaced the
# "before" (the jump waits for everything the transport holds).
l_ctrlc() {
  local pace kind tag t0 t1
  lat_net slow
  for pace in on off; do
    lat_term_pacing "$pace"
    for kind in ssh pipe quic; do
      [[ $kind == ssh && $pace == off ]] && continue
      tag=k$kind$pace
      log "=== Ctrl-C cell $tag"
      case $kind in
        ssh) FC_PROG=ssh fc_start "$tag" -- -tt srv ;;
        pipe) fc_start "$tag" -- --transport pipe srv "lat$tag" ;;
        quic) fc_start "$tag" -- --transport direct srv "lat$tag" ;;
      esac
      fc_ready "$tag" 60
      fc_line "$tag" "stty noflsh; seq -f 'LINE-%09g abcdefghij0123456789abcdefghij0123456789' 1 999999999"
      sleep 3
      t0=$(now_ms)
      fc_type "$tag" '\x03'
      fc_line "$tag" 'echo ALIVE-$((6*7))'
      fc_wait "$tag" 'ALIVE-42' 180
      t1=$(now_ms)
      note "ctrlc.$kind.pacing_$pace.ms=$((t1 - t0)) marks=$(fc "$tag" raw | grep -a -c 'fil: skipped' || true)"
      if [[ $kind == quic ]]; then lat_check_path "$tag" quic 9999999999999 > /dev/null; fi
      [[ $kind == ssh ]] || cx_fk "lat$tag"
    done
  done
  # The default again for whatever runs next.
  lat_term_pacing on
}

LAT_SCENARIOS=(l_nodelay l_keys_idle l_keys_bulk l_catchup l_ctrlc)

# Sourced by fwd.sh for the keystroke cells and the bulk programs; run directly, it is the latency part.
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  part_main latency lat_setup lat_teardown "${LAT_SCENARIOS[@]}" -- "$@"
fi
