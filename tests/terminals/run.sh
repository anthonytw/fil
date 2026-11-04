#!/usr/bin/env bash
# The real-terminal suite (docs/m3/terminals.md): fil's terminal writes replayed into real
# terminals under Xvfb and read back.
#
#   tests/terminals/run.sh [TERMINAL...] [-- RUN.PY OPTIONS]
#
# TERMINAL is xterm, kitty, wezterm or tmux (default: all four that are installed; a terminal
# named on the command line that is missing is an error). `-- --only NAME` narrows to traces
# whose name contains NAME; `-- --dump` keeps each check's raw read-back under the output
# directory. Environment: TERMINALS_OUT (default target/terminals under the repository, or
# $CARGO_TARGET_DIR), TERMINALS_SKIP_SELFCHECK=1.
#
# Linux only; needs python3, Xvfb (xvfb-run) and the terminals. Never part of `just ci`.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
here=$root/tests/terminals
out=${TERMINALS_OUT:-${CARGO_TARGET_DIR:-$root/target}/terminals}

wanted=()
while [ $# -gt 0 ]; do
  case $1 in
    --) shift; break ;;
    xterm | kitty | wezterm | tmux) wanted+=("$1"); shift ;;
    *) echo "terminals: unknown argument $1" >&2; exit 2 ;;
  esac
done
extra=("$@")

[ "$(uname -s)" = Linux ] || { echo "terminals: Linux only (it needs Xvfb and Linux terminals)" >&2; exit 2; }
command -v python3 > /dev/null || { echo "terminals: python3 is missing" >&2; exit 2; }

if [ ${#wanted[@]} -eq 0 ]; then
  for t in xterm kitty wezterm tmux; do
    command -v "$t" > /dev/null && wanted+=("$t")
  done
else
  for t in "${wanted[@]}"; do
    command -v "$t" > /dev/null || { echo "terminals: $t is not installed" >&2; exit 2; }
  done
fi
needs_x=0
for t in "${wanted[@]}"; do [ "$t" = tmux ] || needs_x=1; done
if [ $needs_x = 1 ] && ! command -v xvfb-run > /dev/null; then
  echo "terminals: xvfb-run is missing (apt-get install xvfb)" >&2
  exit 2
fi

manifest=$here/exporter/Cargo.toml
if [ "${TERMINALS_SKIP_SELFCHECK:-0}" != 1 ]; then
  echo "== self-check: every trace replays into alacritty with no difference"
  cargo test --locked --manifest-path "$manifest" -q
fi
cargo build --locked --manifest-path "$manifest" -q
exporter=$(cargo metadata --locked --no-deps --format-version 1 --manifest-path "$manifest" |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug/terminals-export

rm -rf "$out/traces"
mkdir -p "$out"
"$exporter" "$out/traces"

status=0
for t in "${wanted[@]}"; do
  echo "== $t"
  if [ "$t" = tmux ]; then
    python3 -I "$here/run.py" "$t" "$out/traces" "$out" "${extra[@]}" || status=1
  else
    xvfb-run -a -s "-screen 0 1280x1024x24" python3 -I "$here/run.py" "$t" "$out/traces" "$out" "${extra[@]}" || status=1
  fi
done
exit $status
