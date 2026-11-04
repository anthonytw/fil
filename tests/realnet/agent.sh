#!/usr/bin/env bash
# The M2 agent scenarios of the real-network harness (docs/m2/realnet.md, plan §5.3): the agent relay
# (A1, DESIGN §10) of the real `fil` binary, with a real OpenSSH ssh-agent on the client and a real
# ssh hop from the session. In the udp part's topology (fwd.sh, udp.sh), so it needs root and the
# same tools. Run: agent.sh [SCENARIO...], `just realnet agent`.
#
#   f_agent_ssh_hop      from the fil session, a real `ssh` to the server authenticates with a key
#                        that only the client's agent holds; again after the session moved to the
#                        pipe; without -A it is refused
#   f_agent_other_user   another user of the host cannot reach the session's agent socket: not
#                        through its 0700 directories, and with them opened not past the relay's
#                        peer-UID check; the session's SSH_AUTH_SOCK is that socket
#   f_agent_beside_stuck the agent's requests beside 8 forwards whose target never reads, paced
#                        and unpaced (H1)
set -uo pipefail
# shellcheck source=fwd.sh
source "$(dirname "${BASH_SOURCE[0]}")/fwd.sh"

AG_SOCK=$PD/agent.sock
AG_KEY=$PD/agentkey
AG_AUTH=/etc/fil-realnet-udp/authorized_keys

agent_setup() {
  fwd_setup
  cp "$OUT/udp.env" "$OUT/agent.env"
}

# ag_start: a throwaway ssh-agent of the client's user, holding a fresh key (made for the run, never
# on disk outside $PD). Sets AG_PID and AG_FP (the key's SHA256 fingerprint).
ag_start() {
  client_exec_on x rm -f "$AG_SOCK" "$AG_KEY" "$AG_KEY.pub"
  client_exec_on x ssh-keygen -q -t ed25519 -N '' -C fil-realnet-agent -f "$AG_KEY"
  local out
  out=$(client_exec_on x ssh-agent -a "$AG_SOCK")
  AG_PID=$(sed -n 's/.*SSH_AGENT_PID=\([0-9]*\).*/\1/p' <<< "$out" | head -n 1)
  [[ -n $AG_PID ]] || fail "ssh-agent did not start: $out"
  client_exec_on x env "SSH_AUTH_SOCK=$AG_SOCK" ssh-add -q "$AG_KEY" || fail "ssh-add into the throwaway agent failed"
  AG_FP=$(client_exec_on x ssh-keygen -lf "$AG_KEY.pub" | awk '{ print $2 }')
  [[ $AG_FP == SHA256:* ]] || fail "no fingerprint for the agent's key: $AG_FP"
  ok "a throwaway ssh-agent (pid $AG_PID) holds a fresh key $AG_FP"
}

# The end of every agent scenario (fc_finish calls scenario_cleanup): the agent stopped by its pid,
# the key out of sshd's authorized keys, and fwd.sh's own cleanup.
ag_cleanup() {
  [[ -z ${AG_PID:-} ]] || as_root kill "$AG_PID" 2> /dev/null || true
  if [[ -n ${AG_AUTH_SAVED:-} ]]; then as_root cp "$AG_AUTH_SAVED" "$AG_AUTH"; fi
  client_exec_on x rm -f "$AG_SOCK" "$AG_KEY" "$AG_KEY.pub"
}
eval "fwd_scenario_cleanup() $(declare -f scenario_cleanup | tail -n +2)"
scenario_cleanup() {
  ag_cleanup
  fwd_scenario_cleanup
}

# ag_attach NAME SESSION [FIL_ARGS...]: a client with the throwaway agent as its SSH_AUTH_SOCK.
ag_attach() {
  local name=$1 session=$2
  shift 2
  # shellcheck disable=SC2034 # FC_ENV is read by fc_start (fil.sh)
  FC_ENV=("SSH_AUTH_SOCK=$AG_SOCK")
  fc_start "$name" -- "$@" srv "$session"
  # shellcheck disable=SC2034
  FC_ENV=()
  fc_ready "$name"
}

# ag_hop NAME TAG: from the session, `ssh` to the server namespace's sshd (which accepts only the
# agent's key for this) runs `echo`; sets AG_RC to ssh's exit status and AG_OUT.
ag_hop() {
  local name=$1 tag=$2
  FC_N=$((FC_N + 1))
  # The status comes back as 1000 + $?, so the typed line (which holds the arithmetic) never matches.
  fc_line "$name" "ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 rnfil@$SRV_IP 'echo $tag-\$((6*7))'; echo RC$FC_N-\$((1000+\$?))"
  fc_wait "$name" "RC$FC_N-1" 30
  AG_RC=$(($(fc_screen "$name" | grep -o "RC$FC_N-1[0-9]*" | tail -n 1 | cut -d- -f2) - 1000))
  AG_OUT=$(fc_screen "$name")
}

# From the fil session, a real ssh to the server authenticates with the key only the client's agent
# holds: through the relay, as `git push` from a reattached session needs. After a swap to the pipe
# it still does (the same SSH_AUTH_SOCK). Without -A the same ssh is refused (the control: it is
# the relayed key that authenticates, not something else).
f_agent_ssh_hop() {
  reset_net
  netem_apply delay 20ms
  ag_start
  # sshd takes the agent's key for rnfil for this scenario only.
  AG_AUTH_SAVED=$OUT/run/$SCENARIO-authorized_keys.saved
  as_root cp "$AG_AUTH" "$AG_AUTH_SAVED"
  client_exec_on x cat "$AG_KEY.pub" | as_root tee -a "$AG_AUTH" > /dev/null
  FWD_SESSION=aghop
  ag_attach a "$FWD_SESSION" -A
  wait_log a 'Switched(Direct)' 15
  # The session sees the key (ssh-add -l through the relay), by its fingerprint.
  fc_line a "ssh-add -l; echo LIST-\$((6*7))"
  fc_wait a "LIST-42" 20
  fc_has a "$AG_FP" || fwd_miss "ssh-add -l in the session does not list the agent's key $AG_FP: $(fc_screen a | tail -n 5)"
  ag_hop a HOPA
  [[ $AG_RC == 0 ]] && fc_has a 'HOPA-42' || fwd_miss "the ssh hop from the session with -A failed (status $AG_RC): $(tail -n 6 <<< "$AG_OUT")"
  note "hop_over_quic=$([[ $AG_RC == 0 ]] && echo ok || echo "failed:$AG_RC")"
  # UDP out until the session is on the pipe: the same socket, the agent still relayed.
  local pipes0
  pipes0=$(fc_count_log a 'Switched(Pipe)')
  udp_block
  wait_log a 'Switched(Pipe)' 30 $((pipes0 + 1))
  ag_hop a HOPB
  udp_unblock
  [[ $AG_RC == 0 ]] && fc_has a 'HOPB-42' || fwd_miss "the ssh hop after the move to the pipe failed (status $AG_RC): $(tail -n 6 <<< "$AG_OUT")"
  note "hop_after_swap_to_pipe=$([[ $AG_RC == 0 ]] && echo ok || echo "failed:$AG_RC")"
  fc_detach a
  cx_fk "$FWD_SESSION"
  # The control: no -A, the same ssh, refused.
  FWD_SESSION=aghopno
  ag_attach b "$FWD_SESSION"
  ag_hop b HOPC
  # The specific failure: sshd refused the key (no agent to offer it), not some other error.
  [[ $AG_RC != 0 ]] && grep -q 'Permission denied (publickey' <<< "$AG_OUT" \
    || fwd_miss "without -A the ssh hop did not fail with 'Permission denied (publickey)' (status $AG_RC): $(tail -n 4 <<< "$AG_OUT")"
  note "hop_without_A=status_$AG_RC"
  fc_detach b
  cx_fk "$FWD_SESSION"
  fwd_verdict
}

# Another user cannot reach the session's agent socket (DESIGN §10, §11): the directories stop it,
# and with them and the socket opened by hand, the relay's peer-UID check hangs up on it. The
# session's own user gets in (and the agent then waits for a request).
f_agent_other_user() {
  reset_net
  ag_start
  FWD_SESSION=agother
  ag_attach a "$FWD_SESSION" -A
  local fuid sock dir envsock
  fuid=$(id -u rnfil)
  sock=$(as_root find "/tmp/fil-$fuid" -mindepth 2 -maxdepth 2 -path "*/a-*/$FWD_SESSION.sock" | head -n 1)
  [[ -n $sock ]] || fail "no agent socket for $FWD_SESSION under /tmp/fil-$fuid"
  dir=$(dirname "$sock")
  note "agent_dir_mode=$(as_root stat -c %a "$dir") agent_sock_mode=$(as_root stat -c %a "$sock")"
  [[ $(as_root stat -c '%a %U' "$dir") == "700 rnfil" ]] || fwd_miss "the agent directory is $(as_root stat -c '%a %U' "$dir"), not 700 rnfil"
  [[ $(as_root stat -c '%a %U' "$sock") == "600 rnfil" ]] || fwd_miss "the agent socket is $(as_root stat -c '%a %U' "$sock"), not 600 rnfil"
  # The session's SSH_AUTH_SOCK is that socket (the stable path, OD-6).
  FC_N=$((FC_N + 1))
  fc_line a "echo SOCK$FC_N=\$SSH_AUTH_SOCK"
  fc_wait a "SOCK$FC_N=/" 15
  envsock=$(fc_screen a | grep -o "SOCK$FC_N=/[^ ]*" | tail -n 1 | cut -d= -f2)
  [[ $envsock == "$sock" ]] || fwd_miss "the session's SSH_AUTH_SOCK is $envsock, not the relay's $sock"
  fwd_door_check agent "$sock" WAITING "/tmp/fil-$fuid" "$dir"
  fc_detach a
  # Detached: the socket is gone, so a connect is refused at once.
  if as_root test -e "$sock"; then fwd_miss "the agent socket still exists after ~."; fi
  cx_fk "$FWD_SESSION"
  fwd_verdict
}

# The agent beside 8 forwards whose target never reads (the review of PR #83, B2), on V1's
# bottleneck over QUIC, paced and unpaced: 20 `ssh-add -l` from the session's agent socket, each
# a request and an answer through the relay. With pacing they must not wait behind the stuck
# forwards (the agent has a reserve of its own beside the cap).
f_agent_beside_stuck() {
  lat_net bottleneck
  ag_start
  nsx "$NS_S" runuser -u rnfil -- python3 -c "$FWD_H1_PY" deaf "$FWD_H1_DEAF" > /dev/null 2>&1 &
  lat_wait_listen "$NS_S" "$FWD_H1_DEAF"
  local pace fuid sock ms
  fuid=$(id -u rnfil)
  for pace in off on; do
    fwd_pacing "$pace"
    FWD_SESSION=agstuck$pace
    # shellcheck disable=SC2034 # FC_ENV is read by fc_start (fil.sh)
    FC_ENV=("SSH_AUTH_SOCK=$AG_SOCK" "FIL_FORWARD_PACING=$pace")
    fc_start a -- -A -L "127.0.0.1:$FWD_H1_STUCK:127.0.0.1:$FWD_H1_DEAF" srv "$FWD_SESSION"
    # shellcheck disable=SC2034
    FC_ENV=()
    fc_ready a
    wait_log a 'Switched(Direct)' 15
    client_bg_on x timeout 90 python3 -c "$FWD_H1_PY" stuff "$FWD_H1_STUCK" 8
    sleep 10
    sock=$(as_root find "/tmp/fil-$fuid" -mindepth 2 -maxdepth 2 -path "*/a-*/$FWD_SESSION.sock" | head -n 1)
    [[ -n $sock ]] || fail "no agent socket for $FWD_SESSION"
    ms=$(nsx "$NS_S" runuser -u rnfil -- env "SSH_AUTH_SOCK=$sock" bash -c \
      'for i in $(seq 20); do s=$(date +%s%N); ssh-add -l > /dev/null || echo FAIL; e=$(date +%s%N); echo $(((e - s) / 1000000)); done' 2>&1)
    grep -q FAIL <<< "$ms" && fwd_miss "ssh-add -l beside stuck forwards failed (pacing $pace)"
    note "H1AGENT pacing=$pace ssh_add_ms_p50=$(grep -v FAIL <<< "$ms" | sort -n | awk '{ a[NR] = $1 } END { print a[int((NR + 1) / 2)] }') max=$(grep -v FAIL <<< "$ms" | sort -n | tail -n 1)"
    fc_detach a
    cx_fk "$FWD_SESSION"
  done
  fwd_pacing on
  lat_stop_listener "$NS_S" "$FWD_H1_DEAF"
  fwd_verdict
}

AGENT_SCENARIOS=(f_agent_ssh_hop f_agent_other_user f_agent_beside_stuck)

part_main agent agent_setup fwd_teardown "${AGENT_SCENARIOS[@]}" -- "$@"
