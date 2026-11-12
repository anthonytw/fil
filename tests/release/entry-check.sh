#!/usr/bin/env bash
# The owner's release commands run only through tests/release/release.just (tests/release/just.sh),
# never through the top-level justfile, which has no code owner: just has already run whatever that
# file's assignments say by the time a recipe starts, with the keys about to be unlocked.
#
#   entry-check.sh RECIPE     release.just's owner recipes run it first
#
# It fails if any ancestor process is a `just` whose --justfile is not tests/release/release.just:
# `just release X` (the top-level justfile's forwarding recipe, which calls tests/release/just.sh) is
# refused, `tests/release/just.sh release X` is not. It is a guard against habit, not against a
# hostile justfile (one that ran before this could have done anything already): the defense is that
# the owner's commands never read it. Calling a release script directly needs no check (no justfile
# is read); the self-test does that.
#
# The self-test runs under `just ci` (a just on the top-level justfile), so it confines the walk to
# its own processes: with FIL_RELEASE_SELFTEST=1, FIL_ENTRY_CHECK_STOP=PID ends the walk at PID
# (that process and its ancestors are not looked at). Without FIL_RELEASE_SELFTEST=1 it is refused.
# The walk reads `ps -o args=` and `ps -o ppid=`, the same on Linux (procps) and macOS.
set -euo pipefail
recipe=${1:?usage: entry-check.sh RECIPE}
command -v ps > /dev/null || { echo "release: STOP: entry-check.sh needs ps" >&2; exit 1; }
stop=${FIL_ENTRY_CHECK_STOP:-}
if [ -n "$stop" ] && [ "${FIL_RELEASE_SELFTEST:-}" != 1 ]; then
  echo "release: STOP: FIL_ENTRY_CHECK_STOP is set (a test-only override): unset it (only the self-test sets it, with FIL_RELEASE_SELFTEST=1)" >&2
  exit 1
fi
pid=$PPID
while [ -n "$pid" ] && [ "$pid" -gt 1 ] && [ "$pid" != "$stop" ]; do
  args=$(ps -o args= -p "$pid" 2> /dev/null) || break
  [ -n "$args" ] || break
  read -r -a w <<< "$args"
  if [ "$(basename -- "${w[0]}")" = just ]; then
    jf=
    for ((i = 1; i < ${#w[@]}; i++)); do
      case ${w[i]} in
        --justfile | -f) jf=${w[i + 1]:-} ;;
        --justfile=*) jf=${w[i]#--justfile=} ;;
      esac
    done
    case $jf in
      tests/release/release.just | */tests/release/release.just) ;;
      *)
        echo "release: STOP: $recipe was started through a justfile other than tests/release/release.just (process $pid: ${args:0:200})." >&2
        echo "release: the top-level justfile has no code owner, and just ran it before this recipe. Run: tests/release/just.sh $recipe ... (docs/m4/release.md section 9)" >&2
        exit 1
        ;;
    esac
  fi
  pid=$(ps -o ppid= -p "$pid" 2> /dev/null | tr -d ' ') || break
done
