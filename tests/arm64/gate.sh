#!/usr/bin/env bash
# The `fil-test` gate for the arm64 remote run (tests/arm64/remote.sh, docs/m1/pi.md "On GitHub's
# arm64 runner"): the forced command of the throwaway `fil-test` account, with the verbs the test
# Pis' gate offers, so `pi_e2e` and the drop-in suite drive this host exactly as they drive a Pi.
# It runs AS fil-test (sshd's ForceCommand and the key's command="..."), never as root.
#
#   fil server --stdio          fil's fixed remote command (DESIGN.md §8.2), exactly
#   install NAME SHA256 < file  stores ~/.local/bin/NAME: verified, atomic, 128 MiB cap
#   run NAME [ARGS]             an installed binary, with plain arguments
#   status, logs [N]            processes, /tmp/fil-UID, installed hashes; the user journal
#   reset                       kills everything the account runs and clears fil's state
#
# Like the Pis' gate, the containment is the account, not this script: `install` and `run` execute
# what the client uploads. It prints no host name or address (the run's logs are uploaded).
set -u
set -f # no globbing when splitting the command
cmd=${SSH_ORIGINAL_COMMAND:-$*}
# shellcheck disable=SC2086 # split on purpose: the verbs take plain words
set -- $cmd
verb=${1:-}
[ $# -gt 0 ] && shift
bin="$HOME/.local/bin"
state="$HOME/.local/state/fil"
uid=$(id -u)
me=$(id -un)
deny() {
  echo "fil-test-gate: denied: ${cmd:-<empty>}" >&2
  exit 77
}
name_ok() { [[ ${1:-} =~ ^[a-z][a-z0-9-]{0,31}$ ]]; }
arg_ok() { [[ $1 =~ ^[A-Za-z0-9._:/=@,+%-]*$ ]]; }
max_bytes=$((128 * 1024 * 1024))
export XDG_RUNTIME_DIR="/run/user/$uid"
# A machine-wide XDG base directory that is not under this account's home is another user's: the
# GitHub runner image sets XDG_CONFIG_HOME=/home/runner/.config in /etc/environment, which
# pam_env hands to every ssh login, so `fil server` looked for its daemon.toml in the runner's
# home (Permission denied). On a Pi none is set; drop such a one, so fil uses its defaults here.
for v in XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME; do
  case ${!v-} in
    "" | "$HOME"/*) ;;
    *) unset "$v" ;;
  esac
done
# The same for the session bus, which the gate's `systemctl --user` and fil's `systemd-run --user
# --scope` reach: this account's own, never one handed down from the machine.
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
# And a plain system PATH: the image's /etc/environment also puts the runner user's own tool
# directories on every login's PATH, and a Pi's account has none of them.
export PATH=/usr/local/bin:/usr/bin:/bin

case "$verb" in
  fil)
    [ "$*" = "server --stdio" ] || deny
    [ -x "$bin/fil" ] || {
      echo "fil-test-gate: fil is not installed (use: install fil SHA256)" >&2
      exit 78
    }
    exec "$bin/fil" server --stdio
    ;;
  install)
    { name_ok "${1:-}" && [[ ${2:-} =~ ^[0-9a-f]{64}$ ]] && [ $# -eq 2 ]; } || deny
    mkdir -p "$bin"
    tmp=$(mktemp "$bin/.$1.XXXXXX")
    head -c $((max_bytes + 1)) > "$tmp"
    size=$(stat -c %s "$tmp")
    if [ "$size" -gt "$max_bytes" ]; then
      rm -f "$tmp"
      echo "fil-test-gate: larger than $max_bytes bytes" >&2
      exit 1
    fi
    got=$(sha256sum "$tmp" | cut -d' ' -f1)
    if [ "$got" != "$2" ]; then
      rm -f "$tmp"
      echo "fil-test-gate: sha256 mismatch (got $got)" >&2
      exit 1
    fi
    chmod 0755 "$tmp" && mv -f "$tmp" "$bin/$1" && echo "installed $1 ($size bytes, $got)"
    ;;
  run)
    { name_ok "${1:-}" && [ -x "$bin/${1:-}" ]; } || deny
    for a in "${@:2}"; do arg_ok "$a" || deny; done
    exec "$bin/$1" "${@:2}"
    ;;
  status)
    [ $# -eq 0 ] || deny
    echo "host: $(uname -m), $(. /etc/os-release && echo "$PRETTY_NAME")"
    echo "linger: $(loginctl show-user "$me" -p Linger --value 2> /dev/null)"
    echo "--- processes"
    # shellcheck disable=SC2009 # the full command lines are the point (the drivers read them)
    ps -u "$me" -o pid,etime,args --no-headers | grep -v 'fil-test-gate\|ps -u'
    echo "--- /tmp/fil-$uid"
    ls -la "/tmp/fil-$uid" 2> /dev/null || echo "(none)"
    echo "--- installed"
    set +f
    for f in "$bin"/*; do
      if [ -f "$f" ]; then echo "$(sha256sum "$f" | cut -c1-16)  $(basename "$f")"; fi
    done
    ;;
  logs)
    n=${1:-80}
    { [[ $n =~ ^[0-9]{1,5}$ ]] && [ $# -le 1 ]; } || deny
    set +f
    for f in "$state"/*.log; do
      if [ -f "$f" ]; then
        echo "--- $f"
        tail -n "$n" "$f"
      fi
    done
    journalctl --user -n "$n" --no-pager 2> /dev/null || true
    ;;
  reset)
    # Everything this account runs but this gate and its ancestors (the sshd session), then fil's
    # state. The account belongs to the run, so nothing else of anyone's is here.
    [ $# -eq 0 ] || deny
    keep=" $$ "
    p=$$
    while [ "$p" -gt 1 ]; do
      p=$(ps -o ppid= -p "$p" | tr -d ' ')
      keep+="$p "
    done
    systemctl --user stop 'fil-*.scope' 2> /dev/null
    for sig in TERM KILL; do
      for pid in $(pgrep -u "$me"); do
        [[ $keep == *" $pid "* ]] || kill -"$sig" "$pid" 2> /dev/null
      done
      [ "$sig" = TERM ] && sleep 1
    done
    rm -rf "/tmp/fil-$uid" "$state"
    echo "reset done"
    ;;
  *) deny ;;
esac
