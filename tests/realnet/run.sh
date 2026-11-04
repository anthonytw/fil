#!/usr/bin/env bash
# The real-network harness (Tier 1, docs/m1/realnet.md). Linux only: Docker for the OpenSSH
# topology, network namespaces and root for the UDP/NAT topology.
#
#   run.sh [ssh|udp|sysd|policy|latency|all] [SCENARIO...]
#
# Exit status: 0 if every scenario that ran passed, 1 if any failed, 2 if the host cannot run it.
# A part that chose not to run (policy, outside CI and the VM) is listed as SKIPPED, not passed.
set -uo pipefail
# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

require_linux

part=${1:-all}
shift || true
case $part in
  ssh) parts=(ssh) ;;
  udp) parts=(udp) ;;
  sysd) parts=(sysd) ;;
  policy) parts=(policy) ;; # server policy, tests/realnet/policy.sh (plan S1)
  latency) parts=(latency) ;; # not in `all`: the latency lab, docs/m2/latency.md
  all) parts=(ssh udp sysd policy) ;;
  *)
    echo "usage: run.sh [ssh|udp|sysd|policy|latency|all] [SCENARIO...]" >&2
    exit 2
    ;;
esac

rm -rf "$OUT/notes" "$OUT/logs"
mkdir -p "$OUT"
# A part that skips itself exits with this (policy.sh), so it is not counted as passed.
export REALNET_SKIP_RC=3
rc=0
skipped=()
for p in "${parts[@]}"; do
  # Part scripts exit 2 when the host lacks Docker, root or a tool; that is not a test failure.
  bash "$REALNET_DIR/$p.sh" "$@"
  prc=$?
  if [[ $prc -eq 2 ]]; then exit 2; fi
  if [[ $prc -eq $REALNET_SKIP_RC ]]; then
    skipped+=("$p")
    continue
  fi
  if [[ $prc -ne 0 ]]; then rc=1; fi
done

echo
for p in ${skipped[@]+"${skipped[@]}"}; do echo "realnet: $p SKIPPED (not run on this host; see above)"; done
if [[ $rc -ne 0 ]]; then
  echo "realnet: FAILURES (see the tables above)"
elif [[ ${#skipped[@]} -gt 0 ]]; then
  echo "realnet: every scenario that ran passed; skipped: ${skipped[*]}"
else
  echo "realnet: all scenarios passed"
fi
exit $rc
