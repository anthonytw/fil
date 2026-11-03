#!/usr/bin/env bash
# Server policy on the real network (DESIGN.md §11.2, docs/m2/plan.md S1): the real fil daemon
# mirrors the runner's real /etc/ssh/sshd_config. It runs in the udp part's topology (network
# namespaces, the runner's own sshd in the server namespace with a config of the harness's own;
# see udp.sh), so it needs root and the same tools. Run: policy.sh [SCENARIO...], `just realnet
# policy`. Kept in a file of its own (sourced by the runner), apart from V1's forward scenarios.
#
#   p_sshd_no_tcp_forwarding   the runner's sshd_config says `AllowTcpForwarding no` (one line put
#                              first, the file restored after): `fil forward` -R and -L are refused
#                              naming the setting and its line, and `fil ports` lists the client's
#                              own refused -R the same way
#
# The harness's sshd reads /etc/fil-realnet-udp/sshd_config, not this file, so changing it moves
# nothing but what fil mirrors; the host's own sshd keeps running on what it read at its start.
set -uo pipefail
# shellcheck source=udp.sh
source "$(dirname "${BASH_SOURCE[0]}")/udp.sh"

SSHD_REAL=/etc/ssh/sshd_config
# What was there before the scenario, so it goes back exactly (made by the scenario, as root).
POLICY_BAK=/etc/ssh/.sshd_config.fil-realnet-bak
POLICY_NONE=/etc/ssh/.sshd_config.fil-realnet-none

policy_setup() {
  policy_restore
  udp_setup
  cp "$OUT/udp.env" "$OUT/policy.env"
}

policy_teardown() {
  policy_restore
  udp_teardown "$@"
}

# policy_restore: the runner's sshd_config as it was before the scenario (a crash midway is
# repaired by the next setup or teardown).
policy_restore() {
  if as_root test -f "$POLICY_BAK"; then
    as_root cp -p "$POLICY_BAK" "$SSHD_REAL" && as_root rm -f "$POLICY_BAK"
  elif as_root test -f "$POLICY_NONE"; then
    as_root rm -f "$SSHD_REAL" "$POLICY_NONE"
  fi
}

# policy_put LINE: LINE becomes the first line of the runner's sshd_config, the rest unchanged.
policy_put() {
  local new=$OUT/run/sshd_config.policy
  if as_root test -e "$SSHD_REAL"; then
    as_root cp -p "$SSHD_REAL" "$POLICY_BAK"
  else
    as_root install -d -m 755 /etc/ssh
    as_root touch "$POLICY_NONE"
  fi
  {
    printf '%s\n' "$1"
    as_root cat "$POLICY_BAK" 2> /dev/null || true
  } > "$new"
  as_root install -m 644 "$new" "$SSHD_REAL"
}

p_sshd_no_tcp_forwarding() {
  scenario_cleanup() { policy_restore; }
  policy_put "AllowTcpForwarding no"
  local want='refused by server policy: AllowTcpForwarding no (/etc/ssh/sshd_config:1)' out rc
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
  fc_detach a
  policy_restore
}

POLICY_SCENARIOS=(p_sshd_no_tcp_forwarding)

part_main policy policy_setup policy_teardown "${POLICY_SCENARIOS[@]}" -- "$@"
