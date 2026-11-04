#!/usr/bin/env bash
# `remote_peers = "same-user"` on the real network (OD-9, DESIGN.md §7.2): the real
# daemon, configured with `[forward] remote_peers = "same-user"` in its daemon.toml, looks each
# -R connection's peer up in the real /proc/net/tcp and resets one from another Unix user before
# a byte reaches the client. Sourced by udp.sh, whose topology it runs in (the -R port is on the
# server namespace's loopback, the -R target in the client namespace); it adds its scenario to
# the udp part's list. Kept in a file of its own, apart from V1's forward scenarios.
#
#   f_fwd_same_user   the server's user reaches its own -R port; a throwaway second user
#                     (rnpeer) is refused, its bytes never reach the target, and `fil ports`
#                     counts the refusal without naming the user
#
# The daemon reads daemon.toml when it starts, so the scenario restarts rnfil's daemon (the
# harness's own, by pid) with the setting, and at the end puts the file back as it was (a copy
# saved first) and restarts it again. Cleanup is scenario_cleanup, which fc_start's EXIT trap
# (fc_finish) runs after saving the client's artifacts; it is safe to run twice.

FIL_BINS+=(fwdprobe)

PEERS_USER=rnpeer
PEERS_R_PORT=19420
PEERS_R_TARGET=19421

# The server user's daemons: pids only (-u keeps the root helper's own command line out).
peers_daemon_pids() { nsx "$NS_S" pgrep -u rnfil -f 'fil server --daemon' || true; }

peers_restart_daemon() {
  local pid i
  for pid in $(peers_daemon_pids); do as_root kill "$pid" 2> /dev/null || true; done
  for ((i = 0; i < 50; i++)); do
    [[ -z $(peers_daemon_pids) ]] && return 0
    sleep 0.1
  done
  fail "rnfil's daemon did not exit"
}

PEERS_TOML=/home/rnfil/.config/fil/daemon.toml

peers_daemon_toml() {
  printf '%s' "$1" | as_root tee "$PEERS_TOML" > /dev/null
  as_root chown rnfil:rnfil "$PEERS_TOML"
}

# peers_wait_listen NS PORT: something listens on 127.0.0.1:PORT in NS within 10 s.
peers_wait_listen() {
  local i
  for ((i = 0; i < 100; i++)); do
    nsx "$1" ss -Hltn "sport = :$2" | grep -q . && return 0
    sleep 0.1
  done
  fail "nothing listens on port $2 in $1"
}

# peers_connect USER OUT: USER sends 1000 bytes through the -R port; fwdprobe's lines in OUT.
peers_connect() {
  nsx "$NS_S" runuser -u "$1" -- timeout 20 "$UDP_BIN/fwdprobe" connect "127.0.0.1:$PEERS_R_PORT" \
    send 1000 --show-reply > "$2" 2>&1 || true
}

f_fwd_same_user() {
  local d=$OUT/run/$SCENARIO out
  mkdir -p "$d"
  PEERS_SAVED=$d/daemon.toml.saved
  as_root cp -p "$PEERS_TOML" "$PEERS_SAVED"
  scenario_cleanup() {
    local p
    for p in $(nsx "$NS_C" pgrep -u rnclient -x fwdprobe || true); do as_root kill "$p" 2> /dev/null || true; done
    if as_root test -f "$PEERS_SAVED"; then
      as_root cp -p "$PEERS_SAVED" "$PEERS_TOML"
      as_root rm -f "$PEERS_SAVED"
      peers_restart_daemon
    fi
    as_root userdel -r -f "$PEERS_USER" > /dev/null 2>&1 || true
  }
  trap scenario_cleanup EXIT
  as_root userdel -r -f "$PEERS_USER" > /dev/null 2>&1 || true
  as_root useradd -m -s /bin/bash -p '*' "$PEERS_USER"
  [[ $(id -u "$PEERS_USER") != "$(id -u rnfil)" ]] || fail "$PEERS_USER has rnfil's uid"

  reset_net
  peers_daemon_toml "$(as_root cat "$PEERS_SAVED")"$'\n[forward]\nremote_peers = "same-user"\n'
  peers_restart_daemon
  client_bg_on x fwdprobe serve "127.0.0.1:$PEERS_R_TARGET" sink --reply
  peers_wait_listen "$NS_C" "$PEERS_R_TARGET"
  fc_start a -- --transport pipe -R "$PEERS_R_PORT:127.0.0.1:$PEERS_R_TARGET" srv peers
  fc_ready a
  peers_wait_listen "$NS_S" "$PEERS_R_PORT"
  ok "attached with -R $PEERS_R_PORT; the daemon runs with remote_peers = \"same-user\""

  # The user's own connection: through to the target and back.
  peers_connect rnfil "$d/own.out"
  grep -aqx 'REPLY=SINK_BYTES=1000' "$d/own.out" || fail "rnfil's own connection did not get through: $(tr '\n' ' ' < "$d/own.out")"
  ok "rnfil's own connection reached the target (1000 bytes)"

  # Another user's: reset, nothing sent on.
  peers_connect "$PEERS_USER" "$d/other.out"
  if grep -aq '^REPLY=' "$d/other.out"; then fail "$PEERS_USER got through: $(tr '\n' ' ' < "$d/other.out")"; fi
  ok "$PEERS_USER was refused: $(grep -a -E '^(CONNECT|IN_END|IN_BYTES)=' "$d/other.out" | tr '\n' ' ')"
  note "other_user=$(grep -a -E '^IN_END=' "$d/other.out" | cut -d= -f2)"

  # `fil ports`: a count on the -R row, and nothing about who.
  local want='1 connection refused: not from your user on the host (remote_peers = same-user)' i
  for ((i = 0; i < 30; i++)); do
    out=$(client_exec_on x fil ports srv peers 2>&1) || fail "fil ports: $out"
    [[ $out == *"$want"* ]] && break
    sleep 0.2
  done
  [[ $out == *"$want"* ]] || fail "fil ports does not count the refusal: $out"
  [[ $out != *"$PEERS_USER"* ]] || fail "fil ports names the other user: $out"
  ok "fil ports counts the refusal and names nobody"
  out=$(client_exec_on x fil ports srv peers --json 2>&1) || fail "fil ports --json: $out"
  [[ $out == *'"peers_refused": 1'* || $out == *'"peers_refused":1'* ]] || fail "fil ports --json: no peers_refused 1: $out"

  # The same user again, after the refusal: still through.
  peers_connect rnfil "$d/own2.out"
  grep -aqx 'REPLY=SINK_BYTES=1000' "$d/own2.out" || fail "rnfil's second connection did not get through"
  ok "rnfil still gets through"

  fc_detach a
  cx_fk peers
}

ALL_SCENARIOS+=(f_fwd_same_user)
