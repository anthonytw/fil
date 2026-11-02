#!/usr/bin/env bash
# Shared helpers for the real-network harness (docs/m1/realnet.md). Sourced, never run.
#
# Each scenario is one bash function, run in its own `bash` process (see run_scenarios) so that
# `set -euo pipefail` really applies inside it. A scenario passes by returning 0; assertions exit
# non-zero. `note k=v` records a measurement for the summary table.

REALNET_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$REALNET_DIR/../.." && pwd)
OUT=${REALNET_OUT:-$REPO_ROOT/target/realnet}
BIN_DIR=$OUT/bin
export REALNET_DIR REPO_ROOT OUT BIN_DIR

# Commands that need root run through this (empty when we already are root).
if [[ $EUID -eq 0 ]]; then ROOT_CMD=(); else ROOT_CMD=(sudo); fi

as_root() { ${ROOT_CMD[@]+"${ROOT_CMD[@]}"} "$@"; }

now_ms() { date +%s%3N; }

log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

require_linux() {
  if [[ $(uname -s) != Linux ]]; then
    echo "realnet: this harness needs Linux with Docker, network namespaces and root." >&2
    echo "realnet: it does not run on $(uname -s). Use the linux-check workflow instead:" >&2
    echo "realnet:   gh workflow run linux-check --ref BRANCH -f ref=BRANCH -f suite=realnet" >&2
    exit 2
  fi
}

# ---------------------------------------------------------------------------------------------
# Assertions (exit the scenario process on failure)

fail() {
  log "FAIL: $*"
  exit 1
}

ok() { log "ok: $*"; }

# assert DESCRIPTION COMMAND...: the command must succeed.
assert() {
  local desc=$1
  shift
  if "$@"; then ok "$desc"; else fail "$desc"; fi
}

# assert_num DESCRIPTION A OP B, e.g. assert_num "fast" "$ms" -lt 3000
assert_num() {
  local desc=$1 a=$2 op=$3 b=$4
  if [[ -n $a ]] && test "$a" "$op" "$b"; then ok "$desc ($a $op $b)"; else fail "$desc (got '$a', wanted $op $b)"; fi
}

# note KEY=VALUE: a measurement for the summary table.
note() {
  mkdir -p "$OUT/notes"
  printf '%s\n' "$*" >> "$OUT/notes/${SCENARIO:?}.txt"
  log "note: $*"
}

# ---------------------------------------------------------------------------------------------
# Parsing the programs' `KEY k=v k=v` lines

# kv "LINE" key -> value (no spaces in values; the free-text fields are always last).
kv() {
  local t
  for t in $1; do
    if [[ $t == "$2="* ]]; then
      printf '%s' "${t#*=}"
      return 0
    fi
  done
  return 1
}

# last_line FILE PREFIX -> the last line starting with PREFIX (empty if none).
last_line() { grep -a "^$2" "$1" | tail -n 1 || true; }

# count_lines FILE PREFIX
count_lines() { grep -a -c "^$2" "$1" || true; }

# wait_for_line FILE PREFIX TIMEOUT_S: poll until a line starting with PREFIX appears.
wait_for_line() {
  local file=$1 prefix=$2 limit=$3 i
  for ((i = 0; i < limit * 10; i++)); do
    if grep -a -q "^$prefix" "$file" 2>/dev/null; then return 0; fi
    sleep 0.1
  done
  return 1
}

# shellcheck disable=SC2034 # WAIT_RC is read by the callers
# wait_exit PID TIMEOUT_S: waits for a background job of THIS shell. Sets WAIT_RC to its exit
# status, or 124 if it was still running at the timeout. (Not usable in $(...): wait needs the parent.)
wait_exit() {
  local pid=$1 limit=$2 i
  WAIT_RC=124
  for ((i = 0; i < limit * 10; i++)); do
    if ! kill -0 "$pid" 2> /dev/null; then
      if wait "$pid"; then WAIT_RC=0; else WAIT_RC=$?; fi
      return 0
    fi
    sleep 0.1
  done
  return 0
}

# ---------------------------------------------------------------------------------------------
# Running scenarios and the summary table

# run_scenarios PART SCRIPT SCENARIO...: each scenario in a fresh process: `SCRIPT --one NAME`.
# The caller must not run under `set -e` (a failing scenario is a result, not an abort).
# Records PART's results in $OUT/results-PART.tsv. Returns non-zero if any scenario failed.
run_scenarios() {
  local part=$1 script=$2 s start rc status failed=0
  shift 2
  mkdir -p "$OUT/logs" "$OUT/notes"
  : > "$OUT/results-$part.tsv"
  for s in "$@"; do
    log "=== $part: $s"
    rm -f "$OUT/notes/$s.txt"
    start=$SECONDS
    rc=0
    bash "$script" --one "$s" 2>&1 | tee "$OUT/logs/$part-$s.log"
    rc=${PIPESTATUS[0]}
    if [[ $rc -eq 0 ]]; then status=PASS; else status=FAIL; failed=1; fi
    printf '%s\t%s\t%s\t%s\n' "$s" "$status" "$((SECONDS - start))" \
      "$( (cat "$OUT/notes/$s.txt" 2> /dev/null || true) | tr '\n' ' ')" >> "$OUT/results-$part.tsv"
  done
  return $failed
}

print_summary() {
  local part
  printf '\n%-30s %-5s %5s  %s\n' SCENARIO RESULT SECS MEASUREMENTS
  for part in "$@"; do
    [[ -f $OUT/results-$part.tsv ]] || continue
    while IFS=$'\t' read -r name status secs notes; do
      printf '%-30s %-5s %5s  %s\n' "$name" "$status" "$secs" "$notes"
    done < "$OUT/results-$part.tsv"
  done
  if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
    {
      echo '### realnet results'
      echo
      echo '| scenario | result | secs | measurements |'
      echo '|---|---|---|---|'
      for part in "$@"; do
        [[ -f $OUT/results-$part.tsv ]] || continue
        while IFS=$'\t' read -r name status secs notes; do
          echo "| $name | $status | $secs | $notes |"
        done < "$OUT/results-$part.tsv"
      done
    } >> "$GITHUB_STEP_SUMMARY"
  fi
}

# any_failed PART...: true if any recorded scenario failed.
any_failed() {
  local part
  for part in "$@"; do
    [[ -f $OUT/results-$part.tsv ]] || continue
    if awk -F'\t' '$2 == "FAIL" { found = 1 } END { exit !found }' "$OUT/results-$part.tsv"; then
      return 0
    fi
  done
  return 1
}

# Standard main for a part script: `script.sh` runs all (or the named) scenarios; `--one NAME`
# runs a single scenario in this process (used by run_scenarios).
#   part_main PART SETUP_FN TEARDOWN_FN ALL_SCENARIOS... -- "$@"
part_main() {
  local part=$1 setup=$2 teardown=$3
  shift 3
  local all=()
  while [[ $1 != -- ]]; do
    all+=("$1")
    shift
  done
  shift
  if [[ ${1:-} == --one ]]; then
    SCENARIO=$2
    export SCENARIO
    set -euo pipefail
    # shellcheck disable=SC1090,SC1091
    [[ -f $OUT/$part.env ]] && source "$OUT/$part.env"
    "$2"
    exit $?
  fi
  require_linux
  local chosen=("$@")
  [[ ${#chosen[@]} -gt 0 ]] || chosen=("${all[@]}")
  mkdir -p "$OUT"
  PART_TEARDOWN=$teardown
  trap '$PART_TEARDOWN' EXIT
  "$setup"
  local rc=0
  run_scenarios "$part" "${BASH_SOURCE[1]}" "${chosen[@]}" || rc=$?
  print_summary "$part"
  exit $rc
}
