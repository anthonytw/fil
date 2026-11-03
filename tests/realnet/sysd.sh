#!/usr/bin/env bash
# Tier 1, part 3: sessions under systemd (docs/m1/realnet.md).
#
# One container whose PID 1 is systemd, with logind, a user manager, PAM and sshd. The real client
# runs on this machine (on a PTY, under ptydrive) and reaches the container's sshd over the Docker
# bridge, so the daemon and every session start from a real PAM login: a logind session, a user
# runtime directory, `loginctl enable-linger` meaning what it means. What is checked is where the
# session's processes end up (DESIGN.md section 5.1): with lingering on, under
# `systemd-run --user --scope`; with it off, in the process tree.
# Needs Docker and a runner that lets a privileged container run systemd. Run: sysd.sh [SCENARIO...]
#
# SAFETY: the container is --privileged, which is root on the machine it runs on. That is fine on a
# GitHub-hosted runner (a throwaway VM, no secrets in this job). NEVER run this part on a
# self-hosted runner, a lab machine or a developer's own computer; the other parts are not like this.
set -uo pipefail
# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
# shellcheck source=fil.sh
source "$(dirname "${BASH_SOURCE[0]}")/fil.sh"

SD=rn-sysd
IMAGE=fil-realnet-sysd
SD_DIR=$OUT/run/sysd
PD=$OUT/run/pd-sysd
# The real fil and ptydrive run on this machine.
PATH=$BIN_DIR:$PATH
export PATH

# fil.sh wants these. The client lives on this machine.
client_exec_on() {
  shift
  "$@"
}
client_bg_on() {
  shift
  "$@" > /dev/null 2>&1 &
}

# ---------------------------------------------------------------------------------------------
# Setup and teardown

sysd_setup() {
  command -v docker > /dev/null || { echo "realnet: docker is required" >&2; exit 2; }
  docker info > /dev/null 2>&1 || { echo "realnet: the docker daemon is not reachable" >&2; exit 2; }
  sysd_teardown quiet
  mkdir -p "$OUT/run" "$OUT/logs" "$SD_DIR"
  "$REALNET_DIR/build.sh"
  local ctx=$OUT/ctx-sysd
  rm -rf "$ctx"
  mkdir -p "$ctx/bin"
  cp "$REALNET_DIR"/docker/* "$ctx/"
  cp "$BIN_DIR/fil" "$ctx/bin/"
  docker build -q -f "$ctx/Dockerfile.systemd" -t "$IMAGE" "$ctx" > /dev/null

  ssh-keygen -q -t ed25519 -N '' -C realnet-sysd -f "$SD_DIR/id"
  # privileged + cgroup v2: systemd runs as PID 1 and owns the container's cgroup subtree.
  # The cgroup namespace is private (explicitly: with the host's, systemd could disturb the
  # runner's cgroups).
  docker run -d --name "$SD" --hostname sysd --privileged --cgroupns=private \
    --tmpfs /run --tmpfs /run/lock "$IMAGE" > /dev/null

  local i state=
  for ((i = 0; i < 240; i++)); do
    state=$(docker exec "$SD" systemctl is-system-running 2> /dev/null || true)
    case $state in running | degraded) break ;; esac
    sleep 0.5
  done
  log "systemd state after boot: ${state:-none} ($((i / 2)) s)"
  case $state in
    running | degraded) ;;
    *)
      docker logs "$SD" 2>&1 | tail -n 40 >&2 || true
      echo "realnet: systemd did not boot in the container (state '${state:-none}')" >&2
      exit 1
      ;;
  esac
  docker exec "$SD" systemctl --failed --no-legend >&2 || true
  docker cp "$SD_DIR/id.pub" "$SD:/etc/fil-realnet/authorized_keys"
  docker exec "$SD" chmod 644 /etc/fil-realnet/authorized_keys
  # Ubuntu starts sshd on demand: ssh.socket listens on port 22 and starts ssh.service.
  docker exec "$SD" systemctl is-active ssh.socket > /dev/null || {
    docker exec "$SD" systemctl status ssh.socket ssh.service --no-pager >&2 || true
    echo "realnet: sshd is not listening in the systemd container" >&2
    exit 1
  }

  SD_IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$SD")
  [[ -n $SD_IP ]] || { echo "realnet: the container has no address" >&2; exit 1; }
  cat > "$SD_DIR/ssh_config" << EOF
Host sysd
  HostName $SD_IP
  User fil
  IdentityFile $SD_DIR/id
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  BatchMode yes
  ConnectTimeout 10
EOF
  # fil's pipe: plain ssh to the container, with this ssh config. The transport is the pipe only.
  cat > "$SD_DIR/fil.toml" << EOF
[defaults]
transport = "pipe"
pipe = ["ssh", "-F", "$SD_DIR/ssh_config", "-T", "--", "sysd", "fil", "server", "--stdio"]
EOF
  printf 'SD_IP=%q\n' "$SD_IP" > "$OUT/sysd.env"
  for ((i = 0; i < 100; i++)); do
    ssh -F "$SD_DIR/ssh_config" -T sysd true > /dev/null 2>&1 && break
    sleep 0.3
  done
  ssh -F "$SD_DIR/ssh_config" -T sysd true || { echo "realnet: ssh to the systemd container does not work" >&2; exit 1; }
  log "systemd host up at $SD_IP; ssh works; user fil is uid $(docker exec "$SD" id -u fil)"
}

sysd_teardown() {
  if [[ ${1:-} != quiet ]]; then
    mkdir -p "$OUT/logs"
    docker exec "$SD" journalctl --no-pager -b > "$OUT/logs/sysd-journal.log" 2>&1 || true
    docker cp "$SD:/home/fil/.local/state/fil/daemon.log" "$OUT/logs/fil-daemon-sysd.log" > /dev/null 2>&1 || true
  fi
  docker rm -f "$SD" > /dev/null 2>&1 || true
  rm -rf "$SD_DIR" "$PD"
}

# ---------------------------------------------------------------------------------------------
# Helpers

fil_cmd() { "$BIN_DIR/fil" --config "$SD_DIR/fil.toml" "$@"; }

# Start the client with this machine's fil and the config above. Environment of the PTY: a private
# state directory (the client's log).
sd_start() { # sd_start NAME SESSION
  local name=$1
  shift
  fc_start "$name" -- --config "$SD_DIR/fil.toml" sysd "$@"
}

# A command in the container as user fil, in that user's runtime directory.
sd_user() {
  docker exec -u fil -e "XDG_RUNTIME_DIR=/run/user/$(docker exec "$SD" id -u fil)" "$SD" "$@"
}

# The cgroup of a process in the container, one line.
sd_cgroup() { docker exec "$SD" cat "/proc/$1/cgroup" 2> /dev/null | tr '\n' ' '; }

# sd_wait_gone PID SECONDS
sd_wait_gone() {
  local i
  for ((i = 0; i < $2 * 5; i++)); do
    docker exec "$SD" test -d "/proc/$1" || return 0
    sleep 0.2
  done
  return 1
}

# sd_bg_jobs NAME TAG: starts two background jobs in the session's shell: a plain one
# (`sleep 7TAG1 &`) and one that leaves the process tree (`setsid ... sleep 7TAG2`, which gets a
# session of its own and is reparented to init). Sets JOB_PLAIN, JOB_SETSID and SHELL.
sd_bg_jobs() {
  local name=$1 tag=$2
  fc_show_pid "$name"
  SHELL_PID=$(shell_pid "$name")
  fc_line "$name" "sleep 7${tag}1 & setsid nohup sleep 7${tag}2 > /dev/null 2>&1 & echo JOBS-STARTED-\$((3+4))"
  fc_wait "$name" JOBS-STARTED-7
  local i
  for ((i = 0; i < 50; i++)); do
    JOB_PLAIN=$(docker exec "$SD" pgrep -f "^sleep 7${tag}1" | head -n 1 || true)
    JOB_SETSID=$(docker exec "$SD" pgrep -f "^sleep 7${tag}2" | head -n 1 || true)
    [[ -n $JOB_PLAIN && -n $JOB_SETSID ]] && break
    sleep 0.1
  done
  [[ -n $JOB_PLAIN && -n $JOB_SETSID ]] || fail "the background jobs are not running: $(docker exec "$SD" ps -eo pid,args)"
}

# ---------------------------------------------------------------------------------------------
# Scenarios

# Lingering on: the session is a transient user scope, which holds even the process that left the
# tree, and killing the session ends the scope and everything in it.
f_linger_scope() {
  docker exec "$SD" loginctl enable-linger fil
  [[ $(docker exec "$SD" loginctl show-user fil -p Linger) == Linger=yes ]] || fail "lingering did not turn on"
  local i
  for ((i = 0; i < 100; i++)); do
    sd_user systemctl --user is-system-running > /dev/null 2>&1 && break
    docker exec "$SD" test -S "/run/user/$(docker exec "$SD" id -u fil)/bus" && break
    sleep 0.2
  done
  sd_start a ls1
  fc_ready a 60
  sd_bg_jobs a 1
  local cg_shell cg_plain cg_setsid units
  cg_shell=$(sd_cgroup "$SHELL_PID")
  cg_plain=$(sd_cgroup "$JOB_PLAIN")
  cg_setsid=$(sd_cgroup "$JOB_SETSID")
  log "shell cgroup:  $cg_shell"
  log "plain job:     $cg_plain"
  log "setsid job:    $cg_setsid"
  units=$(sd_user systemctl --user list-units --type=scope --no-legend --plain 'fil-*' 2>&1 || true)
  log "user scopes: $units"
  [[ $units == *fil-[0-9a-f]*scope*running* ]] || fail "no running scope fil-HEX.scope: $units"
  ok "the session has a running user scope"
  [[ $cg_shell == *fil-[0-9a-f]*.scope* ]] || fail "the shell is not in the scope: $cg_shell"
  [[ $cg_plain == *fil-[0-9a-f]*.scope* ]] || fail "the background job is not in the scope: $cg_plain"
  [[ $cg_setsid == *fil-[0-9a-f]*.scope* ]] || fail "the setsid job is not in the scope: $cg_setsid"
  ok "shell, background job and the job that left the process tree are all in the scope"
  local scope desc env
  scope=$(sed -n 's/.*\(fil-[0-9a-f]*\.scope\).*/\1/p' <<< "$cg_shell" | head -n 1)
  note "scope=$scope"
  # Privacy (DESIGN.md 11.1): what other users can read about the scope (its name is in every
  # process's /proc/PID/cgroup, its description in the journal) carries no session name.
  [[ $scope != *ls1* ]] || fail "the session name is in the scope's name: $scope"
  desc=$(sd_user systemctl --user show -p Description --value "$scope" 2>&1 || true)
  [[ $desc == "fil session" ]] || fail "the scope's description is not the plain 'fil session': $desc"
  ok "the scope's name and description do not carry the session name"
  # The shell does not inherit the login that started the daemon.
  env=$(docker exec "$SD" sh -c "tr '\\0' '\\n' < /proc/$SHELL_PID/environ" 2>&1 || true)
  [[ -n $env ]] || fail "could not read the shell's environment"
  ! grep -Eq '^(XDG_SESSION_[A-Z]*|SSH_CONNECTION|SSH_CLIENT|SSH_TTY|DISPLAY|LC_TERMINAL)=' <<< "$env" \
    || fail "the shell inherited the login's environment: $(grep -E '^(XDG_SESSION_|SSH_|DISPLAY|LC_TERMINAL)' <<< "$env" | cut -d= -f1 | tr '\n' ' ')"
  ok "the shell's environment carries nothing of the login (XDG_SESSION_*, SSH_*, DISPLAY)"

  fil_cmd kill sysd ls1 > /dev/null || fail "fil kill failed"
  fc_wait_exit a 30
  note "exit_after_kill=$FC_CODE"
  sd_wait_gone "$JOB_PLAIN" 10 || fail "the background job survived fil kill"
  sd_wait_gone "$JOB_SETSID" 10 || fail "the setsid job survived fil kill (the scope should hold it)"
  ok "fil kill ended the shell and both background jobs"
  for ((i = 0; i < 50; i++)); do
    units=$(sd_user systemctl --user list-units --all --type=scope --no-legend --plain 'fil-*' 2>&1 || true)
    [[ -z $units ]] && break
    sleep 0.2
  done
  [[ -z $units ]] || fail "the scope is still there after the kill: $units"
  ok "the scope is gone"
}

# Lingering off: no scope; the process tree. The plain job dies with the session. A process that
# left the tree (setsid, reparented to init) may not: that is the documented limit.
f_no_linger_tree() {
  docker exec "$SD" loginctl disable-linger fil
  [[ $(docker exec "$SD" loginctl show-user fil -p Linger) == Linger=no ]] || fail "lingering did not turn off"
  sd_start a nl1
  fc_ready a 60
  sd_bg_jobs a 2
  local cg_shell cg_plain cg_setsid
  cg_shell=$(sd_cgroup "$SHELL_PID")
  cg_plain=$(sd_cgroup "$JOB_PLAIN")
  cg_setsid=$(sd_cgroup "$JOB_SETSID")
  log "shell cgroup:  $cg_shell"
  log "plain job:     $cg_plain"
  log "setsid job:    $cg_setsid"
  [[ $cg_shell != *fil-[0-9a-f]*.scope* && $cg_plain != *fil-[0-9a-f]*.scope* ]] || fail "a scope was used with lingering off: $cg_shell"
  # The processes sit where the login put them: the PAM session's scope, not a scope of fil's.
  [[ $cg_shell == *session-*.scope* && $cg_plain == *session-*.scope* ]] \
    || fail "the session's processes are not in the login's session scope: $cg_shell | $cg_plain"
  ok "no fil scope: the session is in the process tree, inside the login's session scope"
  fil_cmd kill sysd nl1 > /dev/null || fail "fil kill failed"
  fc_wait_exit a 30
  note "exit_after_kill=$FC_CODE"
  sd_wait_gone "$JOB_PLAIN" 10 || fail "the background job survived fil kill"
  ok "fil kill ended the shell and its plain background job"
  if sd_wait_gone "$JOB_SETSID" 3; then
    note "setsid_job=killed"
  else
    note "setsid_job=SURVIVED (left the tree; the documented limit of the fallback)"
    docker exec "$SD" kill -9 "$JOB_SETSID" || true
  fi
}

ALL_SCENARIOS=(f_linger_scope f_no_linger_tree)

part_main sysd sysd_setup sysd_teardown "${ALL_SCENARIOS[@]}" -- "$@"
