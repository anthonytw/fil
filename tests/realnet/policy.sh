#!/usr/bin/env bash
# Server policy on the real network (DESIGN.md §11.2, docs/m2/plan.md S1): the real fil daemon
# mirrors an sshd_config at the real path, /etc/ssh/sshd_config. It runs in the udp part's
# topology (network namespaces, the runner's own sshd in the server namespace with a config of the
# harness's own; see udp.sh), so it needs root and the same tools, plus unshare(1). Run:
# policy.sh [SCENARIO...], `just realnet policy`. Kept in a file of its own (sourced by the
# runner), apart from V1's forward scenarios.
#
#   p_sshd_no_tcp_forwarding   /etc/ssh/sshd_config says `AllowTcpForwarding no` on its first line:
#                              `fil forward` -R and -L are refused naming the setting and its line,
#                              `fil ports` lists the client's own refused -R the same way, and
#                              nothing listens on its port
#
# The host's own /etc/ssh is never written. The harness's sshd (and so every fil it starts, the
# daemon included) runs in a private mount namespace (`unshare -m`) where /etc/ssh is a bind mount
# of a directory of the run's own: a copy of the host's sshd_config with the line put first, and
# of its sshd_config.d (no host keys). A SIGKILL leaves nothing behind (the namespace goes with its
# processes), and a symbolic link at the host's path (NixOS) is never replaced.
#
# It still runs only where it was asked for: CI=true (GitHub Actions) or FIL_REALNET_POLICY=1 (the
# VM recipe, `just vm-realnet`, sets it). Elsewhere it says so and runs nothing: on its own it
# exits 0, and under run.sh (which sets REALNET_SKIP_RC) with that status, so the summary lists
# the part as skipped, not passed.
set -uo pipefail
# shellcheck source=udp.sh
source "$(dirname "${BASH_SOURCE[0]}")/udp.sh"

if [[ ${CI:-} != true && ${FIL_REALNET_POLICY:-} != 1 ]]; then
  echo "realnet policy: skipped (set FIL_REALNET_POLICY=1 to run it; CI=true does too)" >&2
  exit "${REALNET_SKIP_RC:-0}"
fi

POLICY_LINE="AllowTcpForwarding no"

policy_setup() {
  command -v unshare > /dev/null || { echo "realnet: 'unshare' is required (util-linux)" >&2; exit 2; }
  udp_setup
  cp "$OUT/udp.env" "$OUT/policy.env"
  # The private /etc/ssh: made by root, owned by root, mode 755, as the daemon expects of it.
  local etc=$OUT/run/policy-etc-ssh new=$OUT/run/policy-sshd_config
  as_root rm -rf "$etc"
  as_root install -d -m 755 "$etc"
  if as_root test -d /etc/ssh/sshd_config.d; then
    as_root cp -rL /etc/ssh/sshd_config.d "$etc/"
  fi
  {
    printf '%s\n' "$POLICY_LINE"
    as_root cat /etc/ssh/sshd_config 2> /dev/null || true
  } > "$new"
  as_root install -m 644 "$new" "$etc/sshd_config"
  rm -f "$new"
  # The harness's sshd again, this time in a private mount namespace with that /etc/ssh.
  local pid
  pid=$(as_root cat /etc/fil-realnet-udp/sshd.pid 2> /dev/null || true)
  [[ -n $pid ]] || { echo "realnet: the harness's sshd wrote no pid file" >&2; exit 1; }
  as_root kill "$pid"
  local i
  for ((i = 0; i < 50; i++)); do
    as_root kill -0 "$pid" 2> /dev/null || break
    sleep 0.1
  done
  nsx "$NS_S" unshare -m --propagation private sh -c \
    'mount --bind "$1" /etc/ssh && exec /usr/sbin/sshd -D -E "$2" -f /etc/fil-realnet-udp/sshd_config' \
    sh "$etc" "$OUT/logs/sshd-policy.log" &
  for ((i = 0; i < 100; i++)); do
    client_exec_on x ssh -T srv true > /dev/null 2>&1 && break
    sleep 0.2
  done
  client_exec_on x ssh -T srv true || { echo "realnet: the namespaced sshd did not come up" >&2; exit 1; }
  # What a session's processes see at /etc/ssh/sshd_config is the run's copy.
  [[ $(client_exec_on x ssh -T srv head -n 1 /etc/ssh/sshd_config) == "$POLICY_LINE" ]] \
    || { echo "realnet: the session does not see the private sshd_config" >&2; exit 1; }
  log "sshd in a private mount namespace: /etc/ssh/sshd_config starts with '$POLICY_LINE'"
}

policy_teardown() {
  udp_teardown "$@"
  as_root rm -rf "$OUT/run/policy-etc-ssh"
}

p_sshd_no_tcp_forwarding() {
  local want='refused by server policy: AllowTcpForwarding no (/etc/ssh/sshd_config:1)' out rc spec
  fc_start a -- -R 19410:127.0.0.1:19411 srv pol
  fc_ready a

  for spec in "-R 19400:127.0.0.1:19401" "-L 19402:127.0.0.1:19403"; do
    rc=0
    # shellcheck disable=SC2086 # the spec is two words on purpose
    out=$(client_exec_on x fil forward srv pol $spec 2>&1) || rc=$?
    [[ $rc -ne 0 ]] || fail "fil forward $spec was not refused: $out"
    [[ $out == *"$want"* ]] || fail "fil forward $spec: the refusal does not name the setting: $out"
    ok "fil forward $spec: $out"
  done
  note "refusal=\"${want#refused by server policy: }\""

  # The client's own -R from the command line: refused at its add, listed by `fil ports`.
  out=$(client_exec_on x fil ports srv pol 2>&1) || fail "fil ports: $out"
  [[ $out == *'REFUSED BY SERVER POLICY'* && $out == *'19410'*': AllowTcpForwarding no (/etc/ssh/sshd_config:1)'* ]] \
    || fail "fil ports does not list the refused -R with its setting: $out"
  ok "fil ports lists the refused -R"
  if nsx "$NS_S" ss -Hltn 'sport = :19410' | grep -q .; then
    fail "something listens on the refused -R port"
  fi
  ok "nothing listens on the refused -R port"
  # The host's own file is untouched.
  if as_root head -n 1 /etc/ssh/sshd_config 2> /dev/null | grep -qx "$POLICY_LINE"; then
    fail "the host's /etc/ssh/sshd_config starts with the scenario's line"
  fi
  fc_detach a
}

POLICY_SCENARIOS=(p_sshd_no_tcp_forwarding)

part_main policy policy_setup policy_teardown "${POLICY_SCENARIOS[@]}" -- "$@"
