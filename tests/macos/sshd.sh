#!/usr/bin/env bash
# The sshd of the macOS-remote suite (DESIGN.md §16.9, docs/m4/plan.md MAC). Sourced by remote.sh,
# never run. Two ways to get an sshd on a Mac that the suite's `fil` can log in to as the current
# user:
#
#   user    a user-mode sshd on a high loopback port: throwaway host key and client key made for the
#           run, an authorized_keys of its own, PATH set by sshd_config's SetEnv. No sudo, nothing
#           outside $W is touched. This is the default, and the only mode that is meant for a laptop.
#   system  the system sshd (launchd's com.openssh.sshd, port 22) under the runner's passwordless
#           sudo: the real login path (launchd's per-connection sshd and its real session teardown).
#           It adds a drop-in to /etc/ssh/sshd_config.d and the throwaway client key to the user's
#           ~/.ssh/authorized_keys, and removes both at the end. Throwaway VM only.
#
# Both leave: SSH_PORT, SSH_CONFIG (a client config with the host alias `macremote`),
# SSH_KNOWN_HOSTS and AUTH_FILE (where sshd looks for authorized keys); $W/ssh/client_key is the
# key the suite logs in with.

# The PATH a login's command gets on the remote: the installed fil first, then the system's.
remote_path() { printf '%s' "$W/bin:/usr/bin:/bin:/usr/sbin:/sbin"; }

ssh_write_client_config() {
  cat > "$W/ssh/ssh_config" << EOF
Host macremote
  HostName 127.0.0.1
  Port $SSH_PORT
  User $(id -un)
  IdentityFile $W/ssh/client_key
  IdentitiesOnly yes
  UserKnownHostsFile $SSH_KNOWN_HOSTS
  StrictHostKeyChecking yes
  BatchMode yes
  LogLevel ERROR
EOF
  SSH_CONFIG=$W/ssh/ssh_config
}

# auth_add PUBKEYFILE: lets that key log in.
auth_add() { cat "$1" >> "$AUTH_FILE"; }

sshd_up_user() {
  SSH_PORT=${MACOS_SSH_PORT:-$((22000 + RANDOM % 5000))}
  mkdir -p "$W/ssh"
  chmod 700 "$W/ssh"
  ssh-keygen -q -t ed25519 -N '' -C fil-macos-host -f "$W/ssh/host_key"
  ssh-keygen -q -t ed25519 -N '' -C fil-macos-client -f "$W/ssh/client_key"
  AUTH_FILE=$W/ssh/authorized_keys
  cp "$W/ssh/client_key.pub" "$AUTH_FILE"
  chmod 600 "$AUTH_FILE"
  SSH_KNOWN_HOSTS=$W/ssh/known_hosts
  printf '[127.0.0.1]:%s %s\n' "$SSH_PORT" "$(cut -d' ' -f1,2 "$W/ssh/host_key.pub")" > "$SSH_KNOWN_HOSTS"
  cat > "$W/ssh/sshd_config" << EOF
Port $SSH_PORT
ListenAddress 127.0.0.1
HostKey $W/ssh/host_key
PidFile $W/ssh/sshd.pid
AuthorizedKeysFile $AUTH_FILE
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
UsePAM no
StrictModes no
AllowUsers $(id -un)
SetEnv PATH=$(remote_path)
LogLevel VERBOSE
EOF
  ssh_write_client_config
  # The descriptor limit a login gets from launchd is 256 (soft), which is what makes the daemon raise
  # its own and start sessions through the trampoline (fil_session::nofile). A runner or a developer
  # shell may start with more, so give this sshd (and so every login's children) the macOS default.
  (ulimit -S -n "${MACOS_SSHD_NOFILE:-256}" && exec /usr/sbin/sshd -D -e -f "$W/ssh/sshd_config") > "$W/ssh/sshd.log" 2>&1 &
  SSHD_PID=$!
  local i
  for ((i = 0; i < 100; i++)); do
    nc -z 127.0.0.1 "$SSH_PORT" 2> /dev/null && break
    kill -0 "$SSHD_PID" 2> /dev/null || { cat "$W/ssh/sshd.log" >&2; fail "the user-mode sshd died at start"; }
    sleep 0.1
  done
  SSHD_MODE_NOTE="user-mode sshd pid $SSHD_PID on 127.0.0.1:$SSH_PORT"
}

sshd_down_user() {
  [[ -z ${SSHD_PID:-} ]] || kill "$SSHD_PID" 2> /dev/null || true
}

sshd_up_system() {
  SSH_PORT=22
  mkdir -p "$W/ssh"
  chmod 700 "$W/ssh"
  ssh-keygen -q -t ed25519 -N '' -C fil-macos-client -f "$W/ssh/client_key"
  sudo -n true || fail "system sshd: no passwordless sudo"
  # Remote Login is launchd's job. `systemsetup -setremotelogin` needs Full Disk Access on recent
  # macOS, so load the job directly. A job that is already loaded is fine.
  if ! nc -z 127.0.0.1 22 2> /dev/null; then
    sudo launchctl enable system/com.openssh.sshd 2> /dev/null || true
    sudo launchctl bootstrap system /System/Library/LaunchDaemons/ssh.plist 2>&1 || true
    SYSTEM_SSHD_LOADED=1
    local i
    for ((i = 0; i < 100; i++)); do nc -z 127.0.0.1 22 2> /dev/null && break; sleep 0.2; done
    nc -z 127.0.0.1 22 2> /dev/null || fail "system sshd: nothing listens on port 22 after loading com.openssh.sshd"
  fi
  # PATH for the login's command (the installed fil first).
  sudo mkdir -p /etc/ssh/sshd_config.d
  printf 'SetEnv PATH=%s\n' "$(remote_path)" | sudo tee /etc/ssh/sshd_config.d/100-fil-macos-check.conf > /dev/null
  SYSTEM_DROPIN=1
  mkdir -p ~/.ssh
  chmod 700 ~/.ssh
  AUTH_FILE=$HOME/.ssh/authorized_keys
  if [[ -f $AUTH_FILE ]]; then cp "$AUTH_FILE" "$W/ssh/authorized_keys.saved"; fi
  touch "$AUTH_FILE"
  chmod 600 "$AUTH_FILE"
  cat "$W/ssh/client_key.pub" >> "$AUTH_FILE"
  SSH_KNOWN_HOSTS=$W/ssh/known_hosts
  ssh-keyscan -t ed25519 127.0.0.1 > "$SSH_KNOWN_HOSTS" 2> /dev/null
  [[ -s $SSH_KNOWN_HOSTS ]] || fail "system sshd: ssh-keyscan got no host key"
  ssh_write_client_config
  SSHD_MODE_NOTE="the system sshd (launchd) on 127.0.0.1:22"
}

sshd_down_system() {
  set +e
  if [[ -n ${SYSTEM_DROPIN:-} ]]; then sudo rm -f /etc/ssh/sshd_config.d/100-fil-macos-check.conf; fi
  if [[ -f $W/ssh/authorized_keys.saved ]]; then
    cp "$W/ssh/authorized_keys.saved" "$HOME/.ssh/authorized_keys"
  elif [[ -n ${AUTH_FILE:-} ]]; then
    rm -f "$AUTH_FILE"
  fi
  if [[ -n ${SYSTEM_SSHD_LOADED:-} ]]; then sudo launchctl bootout system/com.openssh.sshd 2> /dev/null; fi
}

sshd_up() {
  case ${MACOS_SSHD:-user} in
    user) sshd_up_user ;;
    system) sshd_up_system ;;
    *) fail "MACOS_SSHD must be user or system" ;;
  esac
  local out
  out=$(ssh -F "$SSH_CONFIG" -T macremote 'echo LOGIN-$((6*7)); echo "PATH=$PATH"; echo "SHELL=$SHELL"; echo "NOFILE=$(ulimit -Sn)/$(ulimit -Hn)"; echo "FIL=$(command -v fil)"' 2>&1) \
    || { echo "$out" >&2; [[ ! -f $W/ssh/sshd.log ]] || tail -n 20 "$W/ssh/sshd.log" >&2; fail "cannot log in to $SSHD_MODE_NOTE"; }
  grep -q 'LOGIN-42' <<< "$out" || fail "login output: $out"
  log "sshd: $SSHD_MODE_NOTE"
  log "the login's environment: $(tr '\n' ' ' <<< "$out")"
  # A login shell can reset PATH from its own startup files (bash reads ~/.bashrc under sshd, and
  # a runner's sets its tool paths): then `fil` is not the installed one, or is not found. The
  # pipe command then names the installed fil by its path, and the log says so.
  if grep -q "^FIL=$W/bin/fil\$" <<< "$out"; then
    REMOTE_FIL=fil
  else
    REMOTE_FIL=$W/bin/fil
    log "NOTE: the login's PATH does not reach the installed fil ($(grep '^FIL=' <<< "$out")); the pipe command names $REMOTE_FIL"
  fi
}

sshd_down() {
  case ${MACOS_SSHD:-user} in
    user) sshd_down_user ;;
    system) sshd_down_system ;;
  esac
}
