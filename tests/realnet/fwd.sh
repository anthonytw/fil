#!/usr/bin/env bash
# The M2 forward scenarios (docs/m2/realnet.md, plan §5.3): port forwards of the real `fil` binary
# on the real network. They run in the udp part's topology (network namespaces, NAT, tc netem, the
# runner's own sshd in the server namespace; see udp.sh), so they need root and the same tools,
# plus python3 for f_fwd_vs_ssh. Run: fwd.sh [SCENARIO...], `just realnet fwd`.
#
#   f_fwd_cable_pull     THE "done when": an -L download and an -R upload, byte-exact (sha256) with one
#                        accept each, across a pulled cable (`ip link set c0 down`) of 30 s and then of
#                        5 min, on auto (QUIC) and on the pipe; ssh -L alongside, as the comparison
#   f_fwd_reconnect      the same pair across a reconnect (auto: UDP cut 6 s; pipe: the ssh killed)
#   f_fwd_swap           the same pair across QUIC -> pipe -> QUIC (UDP cut 10 s, then back)
#   f_fwd_sleep          the same pair across a client sleep (12 s on auto, 45 s on the pipe)
#   f_fwd_roam           the pair across three NAT moves: no reconnect; the largest stall is measured
#   f_fwd_many_short     10,000 sequential and 200 concurrent short connections on one QUIC link, -L and -R
#   f_fwd_ending_stress  bursts of short connections over lossy and slow links: does the QUIC link's
#                        limit on unanswered stream ends (MAX_ENDING, 2,048) end an honest link?
#   f_fwd_other_user     another user on the host reaches a loopback -R port (documented), not the
#                        daemon's socket
#   f_fwd_vs_ssh         keystroke latency and throughput under a bulk forward, fil (pipe, QUIC) against
#                        ssh -L, under the latency lab's conditions (docs/m2/latency.md): H1's data
#
# Measurements go to the results table (notes); f_fwd_vs_ssh's keystroke cells also go to
# $OUT/results-fwd-cells.tsv and the job summary. Knobs (environment):
#   FWD_PULL_SHORT=30 FWD_PULL_LONG=300   the two cable pulls of f_fwd_cable_pull, in seconds
#   FWD_MANY=10000                        f_fwd_many_short's sequential count
#   FWD_STRESS_COUNT=20000 FWD_STRESS_PAR=200   each f_fwd_ending_stress burst
set -uo pipefail
# shellcheck source=latency.sh
source "$(dirname "${BASH_SOURCE[0]}")/latency.sh"

FIL_BINS+=(fwdprobe)

FWD_PULL_SHORT=${FWD_PULL_SHORT:-30}
FWD_PULL_LONG=${FWD_PULL_LONG:-300}
FWD_MANY=${FWD_MANY:-10000}
FWD_STRESS_COUNT=${FWD_STRESS_COUNT:-20000}
FWD_STRESS_PAR=${FWD_STRESS_PAR:-200}
LAT_CELLS=$OUT/results-fwd-cells.tsv

# The pair: -L FWD_L_PORT (client namespace) -> 127.0.0.1:FWD_L_TARGET (server namespace), and
# -R FWD_R_PORT (server namespace, the daemon) -> 127.0.0.1:FWD_R_TARGET (client namespace).
FWD_L_PORT=18080
FWD_L_TARGET=18000
FWD_R_PORT=19200
FWD_R_TARGET=19500
# ssh -L alongside the cable pull: client 18090 -> server 18001.
FWD_SSH_PORT=18090
FWD_SSH_TARGET=18001
FWD_MALLORY=rnmallory

# ---------------------------------------------------------------------------------------------
# Misses and findings
#
# A miss is a failure of what the done-when or a security property requires (plan §1): a wrong
# sha256, a second accept, a reader that did not end on EOF, a changed daemon, ResumeGone, a refused
# forward that listens, another user reaching a socket. Each is recorded at once (a note and the
# log) and collected, so every miss of a scenario is visible, and fwd_verdict fails the scenario at
# its end if there was any. known_bug (fil.sh) is only for a named finding, which says which change
# fixes it; none is open here.
FWD_MISSES=()
fwd_miss() {
  FWD_MISSES+=("$1")
  note "MISS: $1"
  log "MISS: $1"
}
fwd_verdict() {
  [[ ${#FWD_MISSES[@]} -eq 0 ]] || fail "${#FWD_MISSES[@]} miss(es): $(printf '%s; ' "${FWD_MISSES[@]}")"
  ok "no misses"
}

# The pipe's return after a long outage was a named finding (no network watcher on the pipe, so
# the client waited for its next retry, up to 30 s); PR #77 runs the watcher for every transport,
# so it is a miss like any other now. No named finding is open in this part.
# How soon after the cable is back the forwards must move again, on either transport. auto measured
# 0.6 to 2.3 s before PR #77 (its watcher sees the route return); 5 s is that and margin.
FWD_RECOVERY_MS=5000

# ---------------------------------------------------------------------------------------------
# Setup and teardown

fwd_setup() {
  udp_setup
  cp "$OUT/udp.env" "$OUT/fwd.env"
  printf 'cell\ttransport\tload\trate\tsent\techoed\tlost\tmin_ms\tp50_ms\tp90_ms\tp99_ms\tmax_ms\n' > "$LAT_CELLS"
  client_exec_on x mkdir -p "$PD"
  # Another user on the "server", for f_fwd_other_user (a leftover of a killed run goes first).
  as_root userdel -r -f "$FWD_MALLORY" > /dev/null 2>&1 || true
  as_root useradd -m -s /bin/bash -p '*' "$FWD_MALLORY"
}

fwd_teardown() {
  lat_cells_summary
  as_root userdel -r -f "$FWD_MALLORY" > /dev/null 2>&1 || true
  udp_teardown "$@"
}

# ---------------------------------------------------------------------------------------------
# fwdprobe on both ends

# srv_fp ARGS...: fwdprobe as the server's user in the server namespace (the session's side).
srv_fp() { nsx "$NS_S" runuser -u rnfil -- "$UDP_BIN/fwdprobe" "$@"; }

# fpv FILE KEY: the last value of KEY in a fwdprobe output (`KEY=value` lines).
fpv() { grep -a "^$2=" "$1" 2> /dev/null | tail -n 1 | cut -d= -f2- || true; }

# fp_accepts FILE: how many connections a fwdprobe server accepted (its `ACCEPT=` lines).
fp_accepts() { grep -a -c '^ACCEPT=' "$1" 2> /dev/null || true; }

# fwd_digest N SEED: the sha256 a seeded stream of N bytes has.
fwd_digest() { "$BIN_DIR/fwdprobe" digest "$1" --seed "$2" | sed -n 's/^SHA256=//p'; }

# The daemon's pid (the server's user has one daemon).
fwd_daemon_pid() { nsx "$NS_S" pgrep -u rnfil -f 'fil server --daemon' | head -n 1 || true; }

# fwd_rx_bytes FILTER: bytes received by the first established TCP socket in the client namespace
# that matches the ss FILTER (empty if there is none).
fwd_rx_bytes() {
  nsx "$NS_C" ss -tniH state established "( $1 )" 2> /dev/null | grep -o 'bytes_received:[0-9]*' | head -n 1 | cut -d: -f2 || true
}

# ---------------------------------------------------------------------------------------------
# A client with the pair's forwards

# fwd_attach NAME SESSION TRANSPORT: `fil [--transport pipe] -L .. -R .. srv SESSION` on a pty.
# auto waits for the move to QUIC. Sets FIL_PID and DAEMON_PID.
fwd_attach() {
  local name=$1 session=$2 transport=$3 targs=()
  [[ $transport == auto ]] || targs=(--transport "$transport")
  fc_start "$name" -- ${targs[@]+"${targs[@]}"} \
    -L "$FWD_L_PORT:127.0.0.1:$FWD_L_TARGET" -R "$FWD_R_PORT:127.0.0.1:$FWD_R_TARGET" srv "$session"
  fc_ready "$name"
  if [[ $transport == auto ]]; then wait_log "$name" 'Switched(Direct)' 15; fi
  lat_wait_listen "$NS_C" "$FWD_L_PORT"
  lat_wait_listen "$NS_S" "$FWD_R_PORT"
  FIL_PID=$(fc_pid "$name")
  DAEMON_PID=$(fwd_daemon_pid)
  [[ -n $FIL_PID && -n $DAEMON_PID ]] || fail "no client pid ($FIL_PID) or daemon pid ($DAEMON_PID)"
  ok "$name: attached ($transport) with -L $FWD_L_PORT and -R $FWD_R_PORT; the daemon is pid $DAEMON_PID"
}

# ---------------------------------------------------------------------------------------------
# The pair: an -L download and an -R upload, paced, checked whole

# fwd_pair_start TAG BYTES RATE: the -L source (server namespace) and the -R sink (client namespace),
# then the two transfers. Sets FWD_DIR, FWD_TAG, FWD_N, FWD_LCLI, FWD_RCLI and starts the sampler.
fwd_pair_start() {
  FWD_TAG=$1 FWD_N=$2
  local rate=$3
  FWD_DIR=$OUT/run/$SCENARIO
  mkdir -p "$FWD_DIR"
  srv_fp serve "127.0.0.1:$FWD_L_TARGET" source "$FWD_N" --seed 7 --rate "$rate" > "$FWD_DIR/$FWD_TAG-lsrv.out" 2>&1 &
  client_exec_on x fwdprobe serve "127.0.0.1:$FWD_R_TARGET" sink --reply > "$FWD_DIR/$FWD_TAG-rsrv.out" 2>&1 &
  lat_wait_listen "$NS_S" "$FWD_L_TARGET"
  lat_wait_listen "$NS_C" "$FWD_R_TARGET"
  FWD_T0=$(now_ms)
  client_exec_on x timeout 1500 fwdprobe connect "127.0.0.1:$FWD_L_PORT" recv > "$FWD_DIR/$FWD_TAG-lcli.out" 2>&1 &
  FWD_LCLI=$!
  nsx "$NS_S" runuser -u rnfil -- timeout 1500 "$UDP_BIN/fwdprobe" connect "127.0.0.1:$FWD_R_PORT" \
    send "$FWD_N" --seed 9 --rate "$rate" --show-reply > "$FWD_DIR/$FWD_TAG-rcli.out" 2>&1 &
  FWD_RCLI=$!
  fwd_sampler_start
  log "pair $FWD_TAG: $FWD_N bytes each way at $rate B/s (-L down, -R up)"
}

# The sampler: every 100 ms, the bytes the client namespace's two ends of the pair have received
# (the -L reader's socket, the -R sink's), as "MS L R" lines in $FWD_DIR/$FWD_TAG.progress.
fwd_sampler_start() {
  local f=$FWD_DIR/$FWD_TAG.progress
  : > "$f"
  (
    local t l r
    while :; do
      t=$(now_ms)
      l=$(fwd_rx_bytes "dport = :$FWD_L_PORT")
      r=$(fwd_rx_bytes "sport = :$FWD_R_TARGET")
      echo "$t ${l:-0} ${r:-0}" >> "$f"
      sleep 0.1
    done
  ) &
  FWD_SAMPLER=$!
}

fwd_sampler_stop() {
  [[ -z ${FWD_SAMPLER:-} ]] || kill "$FWD_SAMPLER" 2> /dev/null || true
  [[ -z ${FWD_SAMPLER:-} ]] || wait "$FWD_SAMPLER" 2> /dev/null || true
  FWD_SAMPLER=
}

# fwd_progress COL: the latest bytes received by one end (2 = -L, 3 = -R).
fwd_progress() { tail -n 1 "$FWD_DIR/$FWD_TAG.progress" 2> /dev/null | awk -v c="$1" '{ print $c + 0 }'; }

# fwd_wait_progress BYTES SECONDS: both ends have received at least BYTES.
fwd_wait_progress() {
  local i
  for ((i = 0; i < $2 * 10; i++)); do
    if [[ $(fwd_progress 2) -ge $1 && $(fwd_progress 3) -ge $1 ]]; then return 0; fi
    sleep 0.1
  done
  fail "the pair did not reach $1 bytes each way within $2 s (-L $(fwd_progress 2), -R $(fwd_progress 3))"
}

# fwd_resumed_after COL SINCE_MS: ms from SINCE_MS to the first sample after it that shows more bytes
# than the last one at or before it; empty if none.
fwd_resumed_after() {
  awk -v c="$1" -v since="$2" '
    $1 <= since { base = $c; next }
    $c > base && $c > 0 { print $1 - since; exit }' "$FWD_DIR/$FWD_TAG.progress"
}

# fwd_wait_resumed SINCE_MS SECONDS: both ends moved again after SINCE_MS. Notes the delays.
fwd_wait_resumed() {
  local since=$1 i l r
  for ((i = 0; i < $2 * 10; i++)); do
    l=$(fwd_resumed_after 2 "$since")
    r=$(fwd_resumed_after 3 "$since")
    if [[ -n $l && -n $r ]]; then
      FWD_RESUMED_L=$l FWD_RESUMED_R=$r
      return 0
    fi
    sleep 0.1
  done
  fail "the pair did not move again within $2 s of $since (-L ${l:-never}, -R ${r:-never})"
}

# fwd_max_stall COL: the longest time between two samples that showed progress, while the transfer
# ran (from its first byte to its connection's end), and when it began: "MS AT".
fwd_max_stall() {
  awk -v c="$1" '
    !started && $c > 0 { started = 1; lt = $1; lv = $c; next }
    !started || gone { next }
    $c == 0 { gone = 1; next }
    $c > lv { if ($1 - lt > max) { max = $1 - lt; at = lt } lt = $1; lv = $c }
    END { printf "%d %d\n", max, at }' "$FWD_DIR/$FWD_TAG.progress"
}

# fwd_pair_running WHEN: both transfers are still running (an event that lands after the end tests nothing).
fwd_pair_running() {
  kill -0 "$FWD_LCLI" 2> /dev/null || fail "the -L transfer had already ended $1: the event would not land mid-transfer"
  kill -0 "$FWD_RCLI" 2> /dev/null || fail "the -R transfer had already ended $1: the event would not land mid-transfer"
}

# fwd_pair_check TIMEOUT_S: waits for both transfers, then the done-when checks: byte-exact (sha256
# against the seeded stream), one accept at each end's server, a clean end. A miss is a finding in
# fil (KNOWN-BUG), not a harness failure. Notes the time and the largest stall of each.
fwd_pair_check() {
  local limit=$1 lrc rrc want_l want_r got_l got_r acc_l acc_r stall
  wait_exit "$FWD_LCLI" "$limit"
  lrc=$WAIT_RC
  wait_exit "$FWD_RCLI" "$limit"
  rrc=$WAIT_RC
  fwd_sampler_stop
  local took=$(($(now_ms) - FWD_T0))
  # The -R sink prints its block when the connection ends; give it a moment after the sender.
  local i
  for ((i = 0; i < 50; i++)); do
    grep -aq '^CONN_END=' "$FWD_DIR/$FWD_TAG-rsrv.out" 2> /dev/null && break
    sleep 0.1
  done
  want_l=$(fwd_digest "$FWD_N" 7)
  want_r=$(fwd_digest "$FWD_N" 9)
  got_l=$(fpv "$FWD_DIR/$FWD_TAG-lcli.out" IN_SHA256)
  got_r=$(grep -a '^REPLY=SINK_SHA256=' "$FWD_DIR/$FWD_TAG-rcli.out" | tail -n 1 | cut -d= -f3 || true)
  acc_l=$(fp_accepts "$FWD_DIR/$FWD_TAG-lsrv.out")
  acc_r=$(fp_accepts "$FWD_DIR/$FWD_TAG-rsrv.out")
  stall=$(fwd_max_stall 2)
  note "$FWD_TAG.l_ms=$(fpv "$FWD_DIR/$FWD_TAG-lcli.out" MS) $FWD_TAG.l_bytes=$(fpv "$FWD_DIR/$FWD_TAG-lcli.out" IN_BYTES) $FWD_TAG.l_end=$(fpv "$FWD_DIR/$FWD_TAG-lcli.out" IN_END) $FWD_TAG.l_accepts=$acc_l $FWD_TAG.l_max_stall_ms=${stall% *}"
  stall=$(fwd_max_stall 3)
  note "$FWD_TAG.r_ms=$(fpv "$FWD_DIR/$FWD_TAG-rcli.out" MS) $FWD_TAG.r_bytes=$(fpv "$FWD_DIR/$FWD_TAG-rsrv.out" IN_BYTES) $FWD_TAG.r_end=$(fpv "$FWD_DIR/$FWD_TAG-rsrv.out" IN_END) $FWD_TAG.r_accepts=$acc_r $FWD_TAG.r_max_stall_ms=${stall% *} $FWD_TAG.wall_ms=$took"
  local bad=()
  [[ $lrc -eq 0 ]] || bad+=("the -L reader exited $lrc ($(tr '\n' ' ' < "$FWD_DIR/$FWD_TAG-lcli.out" | cut -c1-200))")
  [[ $rrc -eq 0 ]] || bad+=("the -R sender exited $rrc ($(tr '\n' ' ' < "$FWD_DIR/$FWD_TAG-rcli.out" | cut -c1-200))")
  [[ $got_l == "$want_l" ]] || bad+=("-L: sha256 ${got_l:-none} of $(fpv "$FWD_DIR/$FWD_TAG-lcli.out" IN_BYTES) bytes, wanted $want_l of $FWD_N")
  [[ $got_r == "$want_r" ]] || bad+=("-R: the sink got sha256 ${got_r:-none}, wanted $want_r of $FWD_N")
  [[ $acc_l -eq 1 ]] || bad+=("-L: the server in the session accepted $acc_l connections, not 1")
  [[ $acc_r -eq 1 ]] || bad+=("-R: the server on the client machine accepted $acc_r connections, not 1")
  [[ $(fpv "$FWD_DIR/$FWD_TAG-lcli.out" IN_END) == eof ]] || bad+=("-L: the reader's end was $(fpv "$FWD_DIR/$FWD_TAG-lcli.out" IN_END), not eof")
  if [[ ${#bad[@]} -eq 0 ]]; then
    ok "$FWD_TAG: -L and -R byte-exact ($FWD_N bytes each, sha256), one accept at each server, ended with EOF"
  else
    local b
    for b in "${bad[@]}"; do fwd_miss "$FWD_TAG: $b"; done
  fi
  lat_stop_listener "$NS_S" "$FWD_L_TARGET"
  lat_stop_listener "$NS_C" "$FWD_R_TARGET"
}

# fwd_after_checks NAME TAG: the session survived as the done-when asks: the daemon untouched (the
# same pid), the client still running and its shell answering, no channel ended by a lost resume.
fwd_after_checks() {
  local name=$1 tag=$2 dpid
  dpid=$(fwd_daemon_pid)
  [[ $dpid == "$DAEMON_PID" ]] || fwd_miss "$tag: the daemon changed (pid $DAEMON_PID, now ${dpid:-none})"
  if ! fc "$name" status | grep -q '^running'; then
    fwd_miss "$tag: the client exited: $(fc "$name" status)"
    return 0
  fi
  fc_ready "$name" 60
  if fc_log "$name" | grep -q 'ResumeGone'; then fwd_miss "$tag: a forward channel ended with ResumeGone"; fi
  if fc_has "$name" 'different session'; then fwd_miss "$tag: the session was replaced"; fi
  # How the client's forward channels ended (Done is a clean close).
  note "$tag.channel_closes=$(fc_log "$name" | grep -F 'forward channel closed' | grep -o '"why":"[A-Za-z]*"' | sort | uniq -c | tr -s ' ' | tr '\n' ',' || true)"
}

link_downs() { fc_count_log "$1" '"message":"link down"'; }

# ---------------------------------------------------------------------------------------------
# Disruptions

cable_pull() {
  nsx "$NS_C" ip link set c0 down
  log "cable pulled (client c0 down)"
}

cable_plug() {
  nsx "$NS_C" ip link set c0 up
  # Taking the link down removed the default route through it; a real reconnect gets it back from
  # DHCP. Wait for the link to be up before putting the route back.
  local i
  for ((i = 0; i < 50; i++)); do
    nsx "$NS_C" ip route replace default via 10.0.1.1 dev c0 2> /dev/null && break
    sleep 0.1
  done
  log "cable plugged back in"
}

# A laptop sleep: the client's process group and the group of each child it started (a reconnect's
# pipe ssh runs in a group of its own, connect.rs), as dropin's Stopped::all does.
FWD_STOPPED=()
client_stop() {
  local c g
  FWD_STOPPED=("$FIL_PID")
  for c in $(as_root pgrep -P "$FIL_PID" || true); do
    g=$(ps -o pgid= -p "$c" | tr -d ' ')
    [[ -z $g || " ${FWD_STOPPED[*]} " == *" $g "* ]] || FWD_STOPPED+=("$g")
  done
  for g in "${FWD_STOPPED[@]}"; do as_root kill -STOP -- "-$g"; done
  note "${SCENARIO}.stopped_groups=${#FWD_STOPPED[@]}"
}
client_cont() {
  local g
  for g in ${FWD_STOPPED[@]+"${FWD_STOPPED[@]}"}; do as_root kill -CONT -- "-$g" 2> /dev/null || true; done
  FWD_STOPPED=()
}

# The end of every scenario (fc_finish calls it): nothing stays stopped, unplugged or blocked.
scenario_cleanup() {
  fwd_door_restore
  client_cont
  fwd_sampler_stop
  cable_plug > /dev/null 2>&1
  reset_net
  local p
  for p in "$FWD_L_TARGET" "$FWD_SSH_TARGET"; do lat_stop_listener "$NS_S" "$p"; done
  for p in "$FWD_R_TARGET" "$FWD_SSH_PORT"; do lat_stop_listener "$NS_C" "$p"; done
  cx_fk "${FWD_SESSION:-none}"
}

# ---------------------------------------------------------------------------------------------
# The done-when: a pulled cable

# fwd_cable_case TRANSPORT: the pair across two cable pulls (FWD_PULL_SHORT, then FWD_PULL_LONG).
# On auto, ssh -L carries a third transfer through the same pulls, as the comparison.
fwd_cable_case() {
  local tr=$1 name=c$1 n=200000000 rate=8000000 t downs0 i
  FWD_SESSION=fwdcable$tr
  reset_net
  netem_apply delay 20ms
  fwd_attach "$name" "$FWD_SESSION" "$tr"
  mkdir -p "$OUT/run/$SCENARIO"
  local with_ssh=
  if [[ $tr == auto ]]; then
    with_ssh=1
    srv_fp serve "127.0.0.1:$FWD_SSH_TARGET" source "$n" --seed 11 --rate "$rate" > "$OUT/run/$SCENARIO/ssh-srv.out" 2>&1 &
    lat_wait_listen "$NS_S" "$FWD_SSH_TARGET"
    # Plain ssh -L, as a user runs it (no ServerAlive options in the test's ssh config).
    client_exec_on x ssh -N -L "127.0.0.1:$FWD_SSH_PORT:127.0.0.1:$FWD_SSH_TARGET" srv > "$OUT/run/$SCENARIO/ssh-L.out" 2>&1 &
    lat_wait_listen "$NS_C" "$FWD_SSH_PORT"
  fi
  fwd_pair_start "$tr" "$n" "$rate"
  local sshcli=
  if [[ -n $with_ssh ]]; then
    client_exec_on x timeout 1500 fwdprobe connect "127.0.0.1:$FWD_SSH_PORT" recv > "$OUT/run/$SCENARIO/ssh-cli.out" 2>&1 &
    sshcli=$!
  fi
  fwd_wait_progress 20000000 30
  downs0=$(link_downs "$name")
  local pull secs k=0
  for secs in "$FWD_PULL_SHORT" "$FWD_PULL_LONG"; do
    k=$((k + 1))
    fwd_pair_running "before pull $k"
    pull=$(now_ms)
    cable_pull
    sleep "$secs"
    t=$(now_ms)
    cable_plug
    note "$tr.pull$k.down_ms=$((t - pull))"
    fwd_wait_resumed "$t" 120
    note "$tr.pull$k.l_resumed_after_plug_ms=$FWD_RESUMED_L $tr.pull$k.r_resumed_after_plug_ms=$FWD_RESUMED_R"
    # The network is back: the client must be too, within FWD_RECOVERY_MS (recorded above on every
    # run), on both transports: the network watcher sees the route return (PR #77 for the pipe).
    local slow=$FWD_RESUMED_L
    [[ $FWD_RESUMED_R -le $slow ]] || slow=$FWD_RESUMED_R
    if [[ $slow -gt $FWD_RECOVERY_MS ]]; then
      fwd_miss "$tr: after a ${secs} s cable pull the forwards moved again only $slow ms after the cable was back (bound ${FWD_RECOVERY_MS} ms)"
    fi
    [[ $(link_downs "$name") -gt $downs0 ]] || fail "$tr: the client never logged its link down during pull $k: it was not a disconnect"
    downs0=$(link_downs "$name")
    # Some data between the two pulls, so the second lands on a running transfer too.
    if [[ $k -eq 1 ]]; then
      local l0
      l0=$(fwd_progress 2)
      fwd_wait_progress $((l0 + 20000000)) 60
    fi
  done
  fwd_pair_check 600
  note "$tr.link_downs=$(link_downs "$name") $tr.to_pipe=$(fc_count_log "$name" 'Switched(Pipe)') $tr.to_direct=$(fc_count_log "$name" 'Switched(Direct)')"
  fwd_after_checks "$name" "$tr"
  if [[ -n $with_ssh ]]; then
    # ssh -L, the comparison: recorded, not asserted. It may still be waiting on TCP's backoff.
    wait_exit "$sshcli" 240
    local o=$OUT/run/$SCENARIO/ssh-cli.out want verdict
    want=$(fwd_digest "$n" 11)
    if [[ $WAIT_RC -eq 124 ]]; then
      verdict="still-stalled-240s-after-plug(bytes=$(fpv "$o" IN_BYTES))"
    elif [[ $(fpv "$o" IN_SHA256) == "$want" ]]; then
      verdict="byte-exact"
    else
      verdict="broken(end=$(fpv "$o" IN_END),bytes=$(fpv "$o" IN_BYTES))"
    fi
    note "ssh_L.result=$verdict ssh_L.ms=$(fpv "$o" MS) ssh_L.accepts=$(fp_accepts "$OUT/run/$SCENARIO/ssh-srv.out") ssh_L.ssh_alive=$(nsx "$NS_C" ss -ltnH "sport = :$FWD_SSH_PORT" | grep -q . && echo yes || echo no)"
    lat_stop_listener "$NS_C" "$FWD_SSH_PORT"
    lat_stop_listener "$NS_S" "$FWD_SSH_TARGET"
  fi
  fc_detach "$name"
  cx_fk "$FWD_SESSION"
}

f_fwd_cable_pull() {
  fwd_cable_case auto
  fwd_cable_case pipe
  fwd_verdict
}

# ---------------------------------------------------------------------------------------------
# Reconnect, swap, sleep

# fwd_event_case TRANSPORT EVENT: the pair (60 MB each way at 2 MB/s, 30 s of data; 120 MB for the
# swap, whose two moves take about 25 s with the pipe carrying data meanwhile) across EVENT.
fwd_event_case() {
  local tr=$1 ev=$2 name=e$1 tag=$1-$2 downs0 t pipes0 directs0 n=60000000
  FWD_SESSION=fwd$ev$tr
  [[ $ev != swap ]] || n=120000000
  reset_net
  netem_apply delay 20ms
  fwd_attach "$name" "$FWD_SESSION" "$tr"
  fwd_pair_start "$tag" "$n" 2000000
  fwd_wait_progress 8000000 30
  fwd_pair_running "before the $ev"
  downs0=$(link_downs "$name")
  pipes0=$(fc_count_log "$name" 'Switched(Pipe)')
  directs0=$(fc_count_log "$name" 'Switched(Direct)')
  t=$(now_ms)
  case $tr-$ev in
    auto-reconnect)
      # Longer than dead_after (5 s): the link is declared dead and redialled (directly or over
      # the pipe; either is a reconnect).
      udp_block
      sleep 6
      udp_unblock
      ;;
    pipe-reconnect)
      local ssh
      ssh=$(pipe_ssh "$FIL_PID")
      [[ -n $ssh ]] || fail "no ssh under the client"
      as_root kill -9 "$ssh"
      ;;
    auto-swap)
      udp_block
      wait_log "$name" 'Switched(Pipe)' 30 $((pipes0 + 1))
      note "$tag.to_pipe_after_ms=$(($(now_ms) - t))"
      sleep 2
      udp_unblock
      wait_log "$name" 'Switched(Direct)' 60 $((directs0 + 1))
      note "$tag.back_to_direct_after_ms=$(($(now_ms) - t))"
      ;;
    auto-sleep | pipe-sleep)
      local s=12
      [[ $tr == auto ]] || s=45
      client_stop
      sleep "$s"
      client_cont
      note "$tag.stopped_s=$s"
      ;;
    *) fail "no event $ev on $tr" ;;
  esac
  fwd_wait_resumed "$(now_ms)" 120
  note "$tag.l_resumed_ms=$FWD_RESUMED_L $tag.r_resumed_ms=$FWD_RESUMED_R"
  fwd_pair_running "after the $ev"
  [[ $(link_downs "$name") -gt $downs0 ]] || fail "$tag: the client never logged its link down: the $ev was not a disconnect"
  fwd_pair_check 300
  note "$tag.link_downs=$(link_downs "$name") $tag.to_pipe=$(fc_count_log "$name" 'Switched(Pipe)') $tag.to_direct=$(fc_count_log "$name" 'Switched(Direct)')"
  fwd_after_checks "$name" "$tag"
  fc_detach "$name"
  cx_fk "$FWD_SESSION"
}

f_fwd_reconnect() {
  fwd_event_case auto reconnect
  fwd_event_case pipe reconnect
  fwd_verdict
}

f_fwd_swap() {
  fwd_event_case auto swap
  fwd_verdict
}

f_fwd_sleep() {
  fwd_event_case auto sleep
  fwd_event_case pipe sleep
  fwd_verdict
}

# Three NAT moves during the pair: no reconnect, the same connections, byte-exact.
f_fwd_roam() {
  local name=r tag=roam spec
  FWD_SESSION=fwdroam
  reset_net
  netem_apply delay 20ms
  # shellcheck disable=SC2034 # read by nat_move (udp.sh)
  NAT_UDP=
  fwd_attach "$name" "$FWD_SESSION" auto
  fwd_pair_start "$tag" 60000000 2000000
  fwd_wait_progress 4000000 30
  local downs0
  downs0=$(link_downs "$name")
  for spec in "$PUB1:31000-31999" "$PUB1:32000-32999" "$PUB3:33000-33999"; do
    sleep 4
    fwd_pair_running "before the move to $spec"
    nat_move "$spec"
    nat_expect "$spec"
  done
  fwd_pair_check 300
  [[ $(link_downs "$name") -eq $downs0 ]] || fwd_miss "roam: the link went down across the NAT moves ($(link_downs "$name") downs)"
  [[ $(fc_count_log "$name" 'Switched(') -eq 2 ]] || fwd_miss "roam: the client switched transports: $(fc_log "$name" | grep -F 'Switched(' | tr '\n' ' ')"
  [[ -z $(pipe_ssh "$FIL_PID") ]] || fwd_miss "roam: the client fell back to an ssh pipe"
  fwd_after_checks "$name" "$tag"
  fc_detach "$name"
  cx_fk "$FWD_SESSION"
  fwd_verdict
}

# ---------------------------------------------------------------------------------------------
# Many connections

# fwd_burst TAG DIR COUNT PARALLEL: fwdprobe burst through the -L (DIR=L, from the client namespace)
# or the -R (DIR=R, from the server namespace) forward to an echo server at the other end. Leaves
# the output in $OUT/run/$SCENARIO/TAG.burst and sets BURST_RC.
fwd_burst() {
  local tag=$1 dir=$2 count=$3 par=$4 o
  o=$OUT/run/$SCENARIO/$tag.burst
  mkdir -p "$OUT/run/$SCENARIO"
  BURST_RC=0
  case $dir in
    L)
      srv_fp serve "127.0.0.1:$FWD_L_TARGET" echo > "$OUT/run/$SCENARIO/$tag.echo" 2>&1 &
      lat_wait_listen "$NS_S" "$FWD_L_TARGET"
      client_exec_on x timeout 1200 fwdprobe connect "127.0.0.1:$FWD_L_PORT" burst --count "$count" --parallel "$par" > "$o" 2>&1 || BURST_RC=$?
      lat_stop_listener "$NS_S" "$FWD_L_TARGET"
      ;;
    R)
      client_exec_on x fwdprobe serve "127.0.0.1:$FWD_R_TARGET" echo > "$OUT/run/$SCENARIO/$tag.echo" 2>&1 &
      lat_wait_listen "$NS_C" "$FWD_R_TARGET"
      nsx "$NS_S" runuser -u rnfil -- timeout 1200 "$UDP_BIN/fwdprobe" connect "127.0.0.1:$FWD_R_PORT" \
        burst --count "$count" --parallel "$par" > "$o" 2>&1 || BURST_RC=$?
      lat_stop_listener "$NS_C" "$FWD_R_TARGET"
      ;;
  esac
  BURST_MS=$(fpv "$o" MS)
  BURST_FAILED=$(fpv "$o" FAILED)
  BURST_OK=$(fpv "$o" OK)
}

# 10,000 sequential and 200 concurrent connections each way on one QUIC link (M1 died at 1,024).
f_fwd_many_short() {
  local name=m dir downs0 failed=0
  FWD_SESSION=fwdmany
  # 20,000 channels write about 4 MiB of per-channel debug lines, the client log's rotation size:
  # keep those at info, so the link's own lines (link down, path moves, a violation) stay in it.
  # shellcheck disable=SC2034 # read by fc_start (fil.sh)
  FC_ENV=("FIL_LOG=fil=debug,fil_forward=info")
  reset_net
  fwd_attach "$name" "$FWD_SESSION" auto
  downs0=$(link_downs "$name")
  for dir in L R; do
    fwd_burst "seq$dir" "$dir" "$FWD_MANY" 1
    note "seq$dir: ok=$BURST_OK failed=$BURST_FAILED ms=$BURST_MS rc=$BURST_RC"
    [[ $BURST_RC -eq 0 && $BURST_FAILED == 0 && $BURST_OK == "$FWD_MANY" ]] \
      || { failed=1; fwd_miss "$FWD_MANY sequential -$dir connections: ok=$BURST_OK failed=$BURST_FAILED ($(fpv "$OUT/run/$SCENARIO/seq$dir.burst" FIRST_ERROR))"; }
    fwd_burst "par$dir" "$dir" 200 200
    note "par$dir: ok=$BURST_OK failed=$BURST_FAILED ms=$BURST_MS rc=$BURST_RC"
    [[ $BURST_RC -eq 0 && $BURST_FAILED == 0 && $BURST_OK == 200 ]] \
      || { failed=1; fwd_miss "200 concurrent -$dir connections: ok=$BURST_OK failed=$BURST_FAILED ($(fpv "$OUT/run/$SCENARIO/par$dir.burst" FIRST_ERROR))"; }
  done
  [[ $(link_downs "$name") -eq $downs0 ]] || fwd_miss "many_short: the link went down ($(link_downs "$name") downs)"
  [[ $(fc_count_log "$name" 'Switched(') -eq 2 ]] || fwd_miss "many_short: the client switched transports: $(fc_log "$name" | grep -F 'Switched(' | tr '\n' ' ')"
  [[ $failed -eq 1 ]] || ok "$FWD_MANY sequential and 200 concurrent connections each way, all echoed, on one QUIC link with no reconnect"
  fwd_after_checks "$name" many
  fc_detach "$name"
  cx_fk "$FWD_SESSION"
  fwd_verdict
}

# How often the daemon's log says a link ended on unanswered stream ends (grep -c prints 0 on none).
FWD_DAEMON_LOG=/home/rnfil/.local/state/fil/daemon.log
fwd_daemon_trips() {
  # A missing or empty log would read as no trip: that is an error, not a zero.
  as_root test -s "$FWD_DAEMON_LOG" || fail "the daemon's log $FWD_DAEMON_LOG is missing or empty: trips cannot be counted"
  local n
  n=$(as_root grep -c 'does not answer stream ends' "$FWD_DAEMON_LOG" || true)
  printf '%s' "${n:-0}"
}

# The QUIC link ends itself when more than MAX_ENDING (2,048) finished channels wait for the peer's
# answer to their end marker (crates/fil-transport/src/quic.rs). The answers travel on the control
# channel while stream credit comes back in QUIC frames, so an honest burst of short connections on
# a lossy or slow path might pile up enough unanswered ends to trip it, and the link would end as a
# "protocol violation". This measures it, each way, on three paths. A trip is a finding, with numbers.
f_fwd_ending_stress() {
  local name=s cell net dir v0c v0d vc vd downs0 to_pipe0 trips=0
  FWD_SESSION=fwdstress
  # 20,000 channels write about 4 MiB of per-channel debug lines, the client log's rotation size:
  # keep those at info, so the link's own lines (link down, path moves, a violation) stay in it.
  # shellcheck disable=SC2034 # read by fc_start (fil.sh)
  FC_ENV=("FIL_LOG=fil=debug,fil_forward=info")
  reset_net
  fwd_attach "$name" "$FWD_SESSION" auto
  local count
  for cell in lossy slow lossyslow harsh; do
    count=$FWD_STRESS_COUNT
    case $cell in
      lossy) net="delay 20ms 5ms loss 3% rate 1gbit" ;;
      slow) net="delay 20ms 5ms rate 5mbit" ;;
      lossyslow) net="delay 20ms 5ms loss 3% rate 5mbit" ;;
      # Past what a user would call working, to look for the edge (fewer connections: it is slow).
      harsh)
        net="delay 40ms 10ms loss 8% rate 2mbit"
        count=$((FWD_STRESS_COUNT / 4))
        ;;
    esac
    netem_clear
    local d
    for d in "${NETEM_DEVS[@]}"; do
      # shellcheck disable=SC2086 # the netem arguments are words
      nsx "$NS_R" tc qdisc replace dev "$d" root netem $net limit 500
    done
    log "network: $net"
    # The cell should start on QUIC (a lossy cell before it may have moved the session to the
    # pipe): wait up to 60 s for the way back. Where it is noted.
    local w
    for ((w = 0; w < 600; w++)); do
      [[ -z $(pipe_ssh "$FIL_PID") ]] && break
      sleep 0.1
    done
    note "$cell.starts_on=$([[ -z $(pipe_ssh "$FIL_PID") ]] && echo quic || echo pipe)"
    for dir in L R; do
      v0c=$(fc_count_log "$name" 'does not answer stream ends')
      v0d=$(fwd_daemon_trips)
      downs0=$(link_downs "$name")
      to_pipe0=$(fc_count_log "$name" 'Switched(Pipe)')
      fwd_burst "$cell$dir" "$dir" "$count" "$FWD_STRESS_PAR"
      vc=$(($(fc_count_log "$name" 'does not answer stream ends') - v0c))
      vd=$(($(fwd_daemon_trips) - v0d))
      local rate=n/a
      [[ ${BURST_MS:-0} -gt 0 ]] && rate=$(awk -v n="${BURST_OK:-0}" -v ms="$BURST_MS" 'BEGIN { printf "%.0f", n * 1000 / ms }')
      note "$cell$dir: net='$net' conns=$count parallel=$FWD_STRESS_PAR ok=$BURST_OK failed=$BURST_FAILED ms=$BURST_MS conns_per_s=$rate ending_trips_client=$vc ending_trips_daemon=$vd link_downs=$(($(link_downs "$name") - downs0)) to_pipe=$(($(fc_count_log "$name" 'Switched(Pipe)') - to_pipe0))"
      # A trip ends the link and the channels resume on the next one, so the burst itself still
      # succeeds: the link's own record is the check (the log lines, and any link down or move).
      local downs=$(($(link_downs "$name") - downs0)) moves=$(($(fc_count_log "$name" 'Switched(Pipe)') - to_pipe0))
      [[ $downs -eq 0 && $moves -eq 0 ]] || fwd_miss "$cell$dir: the link went down $downs time(s) and moved to the pipe $moves time(s) during the burst over '$net'"
      if [[ $vc -gt 0 || $vd -gt 0 ]]; then
        trips=$((trips + 1))
        fwd_miss "MAX_ENDING tripped on an honest link: $count -$dir connections, $FWD_STRESS_PAR at a time, over '$net': the link ended as 'the peer does not answer stream ends' $vc time(s) on the client and $vd on the daemon ($BURST_OK of $count connections echoed, $BURST_FAILED failed, at $rate per second)"
      fi
      [[ ${BURST_FAILED:-x} == 0 ]] || fwd_miss "$cell$dir: $BURST_FAILED of $count short -$dir connections failed over '$net' ($(fpv "$OUT/run/$SCENARIO/$cell$dir.burst" FIRST_ERROR))"
    done
  done
  note "max_ending_trips=$trips"
  # For scale: the same burst through ssh -L on the lossy path (one TCP connection, so a lost
  # packet holds every connection up, as fil's MUX channel does).
  netem_clear
  for d in "${NETEM_DEVS[@]}"; do
    nsx "$NS_R" tc qdisc replace dev "$d" root netem delay 20ms 5ms loss 3% rate 1gbit limit 500
  done
  srv_fp serve "127.0.0.1:$FWD_SSH_TARGET" echo > "$OUT/run/$SCENARIO/ssh-echo.out" 2>&1 &
  lat_wait_listen "$NS_S" "$FWD_SSH_TARGET"
  client_exec_on x ssh -N -L "127.0.0.1:$FWD_SSH_PORT:127.0.0.1:$FWD_SSH_TARGET" srv > /dev/null 2>&1 &
  lat_wait_listen "$NS_C" "$FWD_SSH_PORT"
  local so=$OUT/run/$SCENARIO/sshL.burst
  client_exec_on x timeout 1200 fwdprobe connect "127.0.0.1:$FWD_SSH_PORT" burst --count "$FWD_STRESS_COUNT" --parallel "$FWD_STRESS_PAR" > "$so" 2>&1 || true
  note "ssh_L_lossy_reference: conns=$FWD_STRESS_COUNT parallel=$FWD_STRESS_PAR ok=$(fpv "$so" OK) failed=$(fpv "$so" FAILED) ms=$(fpv "$so" MS) conns_per_s=$(awk -v n="$(fpv "$so" OK)" -v ms="$(fpv "$so" MS)" 'BEGIN { if (ms > 0) printf "%.0f", n * 1000 / ms; else print "n/a" }')"
  lat_stop_listener "$NS_C" "$FWD_SSH_PORT"
  lat_stop_listener "$NS_S" "$FWD_SSH_TARGET"
  netem_clear
  fwd_after_checks "$name" stress
  fc_detach "$name"
  cx_fk "$FWD_SESSION"
  fwd_verdict
}

# fwd_door_check WHAT SOCK CONTROL DIR...: another user cannot reach the Unix socket SOCK. First the
# directories stop it (EACCES). Then, with the directories and the socket opened by hand (as root,
# restored at once), the owner's own peer-UID check must: the server hangs up on the stranger
# (CLOSED) while the owner's connection is let in (CONTROL: DATA for the daemon, which greets its
# user; WAITING for the agent socket, which waits for a request). A stranger let in is a miss.
# The modes fwd_door_check opened, as "MODE PATH" lines, until it restores them; scenario_cleanup
# restores whatever is left if the scenario stops in between.
FWD_DOOR_MODES=()
fwd_door_restore() {
  local e
  for e in ${FWD_DOOR_MODES[@]+"${FWD_DOOR_MODES[@]}"}; do as_root chmod "${e%% *}" "${e#* }" 2> /dev/null || true; done
  FWD_DOOR_MODES=()
}

fwd_door_check() {
  local what=$1 sock=$2 control=$3 p probe x
  shift 3
  probe=$(nsx "$NS_S" runuser -u "$FWD_MALLORY" -- "$UDP_BIN/sockprobe" "$sock" 2>&1 || true)
  note "$what.closed_door=$probe"
  [[ $probe == 'REFUSED errno=13'* ]] || fwd_miss "another user's connect to the $what socket, with the directories as fil left them: $probe"
  # Paths have no spaces here (/tmp/fil-UID/...), so "MODE PATH" splits on the first space.
  for x in "$@" "$sock"; do FWD_DOOR_MODES+=("$(as_root stat -c %a "$x") $x"); done
  for x in "$@"; do as_root chmod 711 "$x"; done
  as_root chmod 666 "$sock"
  p=$(nsx "$NS_S" runuser -u rnfil -- "$UDP_BIN/sockprobe" "$sock" 2>&1 || true)
  probe=$(nsx "$NS_S" runuser -u "$FWD_MALLORY" -- "$UDP_BIN/sockprobe" "$sock" 2>&1 || true)
  fwd_door_restore
  note "$what.open_door: owner=$p stranger=$probe"
  [[ $p == "$control"* ]] || fail "control: the owner's own connection to the $what socket got '$p', wanted $control"
  [[ $probe == CLOSED* ]] || fwd_miss "another user's connect to the $what socket with the door opened was not hung up on (the peer-UID check): $probe"
  ok "$what socket: another user is stopped by the directories, and with them open by the peer-UID check"
}

# Another user of the host can reach a loopback -R port (as with ssh: documented in the plan, §5.3)
# but not the daemon's socket: neither through the 0700 directory nor, with it opened, past the
# daemon's peer-UID check. (The plan's `same-user` -R policy is not built yet, OD-9: not covered.)
f_fwd_other_user() {
  local name=o o muid fuid sock
  FWD_SESSION=fwdother
  reset_net
  fwd_attach "$name" "$FWD_SESSION" auto
  client_exec_on x fwdprobe serve "127.0.0.1:$FWD_R_TARGET" sink --reply > "$OUT/run/$SCENARIO-sink.out" 2>&1 &
  lat_wait_listen "$NS_C" "$FWD_R_TARGET"
  o=$OUT/run/$SCENARIO-mallory.out
  nsx "$NS_S" runuser -u "$FWD_MALLORY" -- timeout 30 "$UDP_BIN/fwdprobe" connect "127.0.0.1:$FWD_R_PORT" send 100000 --show-reply > "$o" 2>&1 || true
  if [[ $(grep -a '^REPLY=SINK_SHA256=' "$o" | cut -d= -f3) == "$(fwd_digest 100000 1)" ]]; then
    note "mallory_reaches_loopback_R=yes (documented: a loopback -R port is open to every local user, as with ssh)"
  else
    note "mallory_reaches_loopback_R=no ($(tr '\n' ' ' < "$o" | cut -c1-160))"
  fi
  lat_stop_listener "$NS_C" "$FWD_R_TARGET"
  fuid=$(id -u rnfil)
  muid=$(id -u "$FWD_MALLORY")
  [[ $fuid != "$muid" ]] || fail "mallory has fil's uid"
  # The daemon's socket is the one that greets its own user: the directory can hold others (a
  # stale socket left by a daemon of another run or revision answers ECONNREFUSED).
  local s all
  all=$(as_root find "/tmp/fil-$fuid" -maxdepth 1 -name '*.sock' | sort)
  note "daemon_dir_sockets=$(for s in $all; do printf '%s:%s,' "$(basename "$s")" "$(nsx "$NS_S" runuser -u rnfil -- "$UDP_BIN/sockprobe" "$s" 2>&1 | cut -d' ' -f1-2)"; done)"
  sock=
  for s in $all; do
    if [[ $(nsx "$NS_S" runuser -u rnfil -- "$UDP_BIN/sockprobe" "$s" 2>&1) == DATA* ]]; then
      sock=$s
      break
    fi
  done
  [[ -n $sock ]] || fail "no live daemon socket in /tmp/fil-$fuid (sockets: $(tr '\n' ' ' <<< "$all"))"
  fwd_door_check daemon "$sock" DATA "/tmp/fil-$fuid"
  fc_detach "$name"
  cx_fk "$FWD_SESSION"
  fwd_verdict
}

# ---------------------------------------------------------------------------------------------
# f_fwd_vs_ssh: H1's data

# The bulk forwards, as in l_keys_bulk: client 19000 -> the sink (19100), client 19001 -> the
# source (19101), both on the server's loopback.
fwd_bulk_cmd() {
  local kind=$1 name=$2 l=(-L "127.0.0.1:$LAT_FWD_UP:127.0.0.1:$LAT_SINK_PORT" -L "127.0.0.1:$LAT_FWD_DOWN:127.0.0.1:$LAT_SOURCE_PORT")
  case $kind in
    ssh) FWD_CMD=(ssh -tt "${l[@]}" srv) ;;
    # The same without OpenSSH's keystroke-timing obfuscation (20 ms batches on a pty session,
    # docs/m2/latency.md): the like-for-like baseline, as fil's pipe never pays it.
    sshno) FWD_CMD=(ssh -tt -o ObscureKeystrokeTiming=no "${l[@]}" srv) ;;
    pipe) FWD_CMD=(fil --transport pipe "${l[@]}" srv "$name") ;;
    quic) FWD_CMD=(fil --transport direct "${l[@]}" srv "$name") ;;
  esac
}

# fwd_app_bytes up|down: the bytes the application has received so far: for an upload the sink's
# socket in the server namespace (what came out of the forward there), for a download the reader's
# socket in the client namespace. That is goodput, not the wire: retransmissions and framing do not
# count, so a forward that resent a lot would show it.
fwd_app_bytes() {
  local b
  case $1 in
    up) b=$(nsx "$NS_S" ss -tniH state established "( sport = :$LAT_SINK_PORT )" 2> /dev/null | grep -o 'bytes_received:[0-9]*' | head -n 1 | cut -d: -f2 || true) ;;
    down) b=$(fwd_rx_bytes "dport = :$LAT_FWD_DOWN") ;;
  esac
  printf '%s' "${b:-0}"
}

# fwd_bulk_cell KIND LOAD: one keystroke cell at 20 keys/s with the bulk LOAD (idle, up, down)
# through the same client's forward, on the bottleneck. Notes the goodput (and the wire rate, for
# reference); records the cell.
fwd_bulk_cell() {
  local kind=$1 load=$2 rate=20 tag name kp bulk='' b0 b1 t0 t1 dev mbit settle=6000
  tag=$kind$load
  name=fvs$tag
  fwd_bulk_cmd "$kind" "$name"
  log "=== cell $tag: ${FWD_CMD[*]}, bulk $load"
  lat_kp "$tag" "$rate" "$settle" -- "${FWD_CMD[@]}" &
  kp=$!
  lat_wait_listen "$NS_C" "$LAT_FWD_UP"
  if [[ $kind == quic ]]; then
    # The bulk starts on QUIC, not on the pipe it attached over.
    printf -v "FC_WHERE_$tag" x
    wait_log "$tag" 'Switched(Direct)' 20
  fi
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
  local a0 a1 wire
  if [[ -n $bulk ]]; then
    sleep 5
    b0=$(lat_tx_bytes "$dev")
    a0=$(fwd_app_bytes "$load")
    t0=$(now_ms)
    # Read the end while keyprobe still types (its last key ends the client, and the transfer).
    sleep $(($(lat_count "$rate") / rate - 8))
    b1=$(lat_tx_bytes "$dev")
    a1=$(fwd_app_bytes "$load")
    t1=$(now_ms)
  fi
  local rc=0
  wait "$kp" || rc=$?
  if [[ -n $bulk ]]; then
    mbit=$(awk -v b0="$a0" -v b1="$a1" -v t0="$t0" -v t1="$t1" 'BEGIN { printf "%.1f", (b1 - b0) * 8 / ((t1 - t0) / 1000) / 1e6 }')
    wire=$(awk -v b0="$b0" -v b1="$b1" -v t0="$t0" -v t1="$t1" 'BEGIN { printf "%.1f", (b1 - b0) * 8 / ((t1 - t0) / 1000) / 1e6 }')
    note "$tag.goodput_mbit=$mbit $tag.wire_mbit=$wire"
    printf -v "FWD_MBIT_$tag" '%s' "$mbit"
    kill "$bulk" 2> /dev/null || true
    wait "$bulk" 2> /dev/null || true
    awk -v m="$mbit" 'BEGIN { exit !(m >= 2) }' || fail "$tag: the application received $mbit Mbit/s: the link was not loaded (wire $wire Mbit/s)"
  fi
  [[ $rc -eq 0 ]] || fail "$tag: keyprobe failed: $(tail -n 3 "$OUT/run/$SCENARIO-$tag.err" 2> /dev/null)"
  lat_finish "$kind" "$tag" "$load" "$rate"
  if [[ $kind != ssh && $kind != sshno ]]; then
    lat_save_log "$tag"
    lat_check_path "$tag" "$kind" "$(lat_cell_field "$tag" first_key_ms)"
    cx_fk "$name"
  fi
}

# Keystrokes at 20 keys/s with a bulk transfer through the same client's -L, idle, up and down, on
# `delay 20ms 5ms rate 20mbit limit 500` each way: ssh -L (again, on this runner), fil over the
# pipe and fil over QUIC. For each fil row: H1's rule (p50 above 2x idle + 10 ms) and the plan's
# target against ssh (keystroke p99 no worse than ssh -L's, throughput within 2x of ssh's).
f_fwd_vs_ssh() {
  lat_need python3
  lat_net bottleneck
  client_exec_on x mkdir -p "$PD"
  trap lat_bulk_cleanup EXIT
  nsx "$NS_S" runuser -u rnfil -- python3 -c "$LAT_PY" "$LAT_SINK_PORT" "$LAT_SOURCE_PORT" > "$OUT/run/$SCENARIO-bulk-server.log" 2>&1 &
  lat_wait_listen "$NS_S" "$LAT_SOURCE_PORT"
  lat_wait_listen "$NS_S" "$LAT_SINK_PORT"
  local kind load
  for kind in ssh sshno pipe quic; do
    for load in idle up down; do fwd_bulk_cell "$kind" "$load"; done
  done
  local idle p50 p99 sp99 np99 trig mbit smbit v
  for kind in ssh sshno pipe quic; do
    idle=$(lat_cell_field "${kind}idle" p50_ms)
    trig=$(awk -v i="$idle" 'BEGIN { printf "%.1f", 2 * i + 10 }')
    note "$kind.idle_p50_ms=$idle $kind.h1_trigger_p50_ms=$trig"
    for load in up down; do
      p50=$(lat_cell_field "$kind$load" p50_ms)
      p99=$(lat_cell_field "$kind$load" p99_ms)
      sp99=$(lat_cell_field "ssh$load" p99_ms)
      v="FWD_MBIT_$kind$load"
      mbit=${!v}
      v="FWD_MBIT_ssh$load"
      smbit=${!v}
      note "$kind.$load: p50_ms=$p50 p99_ms=$p99 bulk_mbit=$mbit p50_vs_idle=$(awk -v p="$p50" -v i="$idle" 'BEGIN { printf "%.1fx", p / i }') h1_rule_exceeded=$(awk -v p="$p50" -v t="$trig" 'BEGIN { print (p > t) ? "yes" : "no" }')"
      if [[ $kind == pipe || $kind == quic ]]; then
        np99=$(lat_cell_field "sshno$load" p99_ms)
        note "$kind.$load.vs_ssh: p99_no_worse=$(awk -v a="$p99" -v b="$sp99" 'BEGIN { print (a <= b) ? "yes" : "no" }') (ssh p99_ms=$sp99) p99_no_worse_than_sshno=$(awk -v a="$p99" -v b="$np99" 'BEGIN { print (a <= b) ? "yes" : "no" }') (sshno p99_ms=$np99) goodput_within_2x=$(awk -v a="$mbit" -v b="$smbit" 'BEGIN { print (a * 2 >= b) ? "yes" : "no" }') (ssh goodput_mbit=$smbit)"
      fi
    done
  done
  fwd_verdict
}

FWD_SCENARIOS=(
  f_fwd_cable_pull f_fwd_reconnect f_fwd_swap f_fwd_sleep f_fwd_roam
  f_fwd_many_short f_fwd_ending_stress f_fwd_other_user f_fwd_vs_ssh
)

# Sourced by agent.sh for the topology and the helpers; run directly, it is the fwd part.
# `--rest` is every scenario but the cable pull (CI runs the two halves as two jobs, run.sh).
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  if [[ ${1:-} == --rest ]]; then
    shift
    rest=()
    for x in "${FWD_SCENARIOS[@]}"; do [[ $x == f_fwd_cable_pull ]] || rest+=("$x"); done
    set -- "${rest[@]}" "$@"
  fi
  part_main fwd fwd_setup fwd_teardown "${FWD_SCENARIOS[@]}" -- "$@"
fi
