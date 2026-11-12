#!/usr/bin/env bash
# An arm64 Linux machine as the remote host, the way the test Pis are one (docs/m1/pi.md "On
# GitHub's arm64 runner"; owner decision 2026-11-11: the Pis are no longer a 0.2.0 gate, an
# automated Linux arm64 remote run replaces them). This machine plays both sides: a throwaway
# `fil-test` account behind the same forced-command gate the Pis have (tests/arm64/gate.sh) runs
# the static aarch64 `fil` (the `aarch64` job's artifact, the binary that would go on a Pi); the
# client side is this machine's own build, driven by `pi_e2e` and the drop-in suite as on the Pis.
#
# The host side has a network of its own: an sshd in the network namespace `filhost`, joined to
# this one by a veth pair (this side 10.0.9.1, the host 10.0.9.2: private test addresses, as in
# tests/realnet). Everything the gate starts (the daemon, its QUIC socket, the sessions) lives
# there, so the client reaches it over a network as it reaches a Pi, and the drop-in suite's cut
# relay (netcut.rs: TCP to the host's port 22, UDP to its ports 60000 to 61000) holds those ports
# here without taking the daemon's. On loopback the relay and the daemon would compete for them.
#
#   sudo -E tests/arm64/remote.sh setup      the account, the gate, the sshd drop-in, lingering,
#                                            the namespace and its sshd, the invoking user's key
#                                            and known_hosts entry
#   tests/arm64/remote.sh install DIR        installs DIR/fil and DIR/fwdprobe through the gate
#                                            (checked against DIR/NAME.sha256)
#   sudo tests/arm64/remote.sh teardown      removes what setup added (for a VM that is kept)
#
# Then: FIL_PI_TARGETS=arm64=fil-test@10.0.9.2 just pi-e2e --hosts arm64, and the same for
# `just dropin --hosts arm64 --scale full --predict always`.
#
# setup and teardown change the machine as root (a user, sshd's configuration, a namespace): they
# refuse to run unless this is a GitHub-hosted runner or FIL_ARM64_THROWAWAY=1 says the machine is
# throwaway (the Lima VM of docs/dev/vm.md). Nothing here prints a host name or a real address.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ACCOUNT=fil-test
GATE=/usr/local/bin/fil-test-gate
DROPIN=/etc/ssh/sshd_config.d/20-fil-test.conf
NS=filhost
HERE_IP=10.0.9.1
HOST_IP=10.0.9.2
NS_UNIT=fil-arm64-sshd
TARGET=$ACCOUNT@$HOST_IP

die() {
  echo "arm64-remote: $*" >&2
  exit 1
}

throwaway() {
  [[ ${GITHUB_ACTIONS:-} == true && ${RUNNER_ENVIRONMENT:-} == github-hosted ]] ||
    [[ ${FIL_ARM64_THROWAWAY:-} == 1 ]] ||
    die "refusing to change this machine: not a GitHub-hosted runner, and FIL_ARM64_THROWAWAY=1 is not set"
  [[ $(id -u) -eq 0 ]] || die "$1 needs root (sudo -E)"
  # The user that runs the client: the one who ran this through sudo, or FIL_ARM64_CLIENT_USER.
  CLIENT=${FIL_ARM64_CLIENT_USER:-${SUDO_USER:-}}
  [[ -n $CLIENT && $CLIENT != root ]] || die "run it through sudo from the user that runs the client (or set FIL_ARM64_CLIENT_USER)"
}

as_client() { runuser -u "$CLIENT" -- "$@"; }

stop_ns_sshd() {
  systemctl stop "$NS_UNIT" 2> /dev/null || true
  systemctl reset-failed "$NS_UNIT" 2> /dev/null || true
}

# The host's network: the namespace, the veth pair, and an sshd of its own in it (the system's
# configuration, the drop-in included, listening on the host's address only).
start_ns() {
  # Captured first, as below: under pipefail, `cmd | grep -q` fails when grep exits before cmd
  # has written.
  local have
  have=$(ip netns list)
  if ! grep -qw "$NS" <<< "$have"; then
    ip netns add "$NS"
    ip link add fil-here type veth peer name fil-host
    ip link set fil-host netns "$NS"
    ip addr add "$HERE_IP/24" dev fil-here
    ip link set fil-here up
    ip -n "$NS" addr add "$HOST_IP/24" dev fil-host
    ip -n "$NS" link set fil-host up
    ip -n "$NS" link set lo up
  fi
  # sshd -t wants its privilege separation directory, which only a running sshd has made.
  install -d -m 0755 /run/sshd
  sshd -t
  stop_ns_sshd
  # The service's mount namespace must see the user runtime directories logind mounts later (a
  # systemd host's mounts are shared already; a container's may be private).
  [[ $(findmnt -no PROPAGATION /run) == shared* ]] || mount --make-rshared /
  # A systemd service in the namespace's network (NetworkNamespacePath), as sshd is a service on a
  # Pi. Without the user's runtime directory in its mounts, pam_systemd sets no XDG_RUNTIME_DIR
  # ("not in order"), and fil's sessions get no `systemd-run --user --scope` (pi_e2e's `lingering`
  # scenario fails on it).
  systemd-run --quiet --unit="$NS_UNIT" -p "NetworkNamespacePath=/var/run/netns/$NS" \
    /usr/sbin/sshd -D -o "ListenAddress=$HOST_IP" -o "PidFile=none"
}

setup() {
  throwaway setup
  local home key pub uid i
  export DEBIAN_FRONTEND=noninteractive
  # The tools the drop-in suite requires on a host (REQUIRED_TOOLS) and the shells it uses when
  # they are there (zsh, fish), as on the Pis; nc for the client side's relay checks.
  apt-get update -q
  apt-get install -y -q openssh-server vim less htop git procps python3 zsh fish netcat-openbsd \
    dbus-user-session file iproute2 curl

  if ! id "$ACCOUNT" > /dev/null 2>&1; then
    useradd --create-home --shell /bin/bash "$ACCOUNT"
  fi
  # No password, but not a locked account ("!" refuses even a key without PAM).
  usermod -p '*' "$ACCOUNT"
  chmod 0700 "/home/$ACCOUNT"
  install -m 0755 -o root -g root "$HERE/gate.sh" "$GATE"

  # The invoking user's key, restricted to the gate.
  home=$(getent passwd "$CLIENT" | cut -d: -f6)
  key=$home/.ssh/id_ed25519
  if [[ ! -f $key ]]; then
    as_client mkdir -p -m 0700 "$home/.ssh"
    as_client ssh-keygen -q -t ed25519 -N '' -C fil-arm64-remote -f "$key"
  fi
  pub=$(cat "$key.pub")
  install -d -m 0700 -o "$ACCOUNT" -g "$ACCOUNT" "/home/$ACCOUNT/.ssh"
  printf 'restrict,command="%s" %s\n' "$GATE" "$pub" > "/home/$ACCOUNT/.ssh/authorized_keys"
  chown "$ACCOUNT:$ACCOUNT" "/home/$ACCOUNT/.ssh/authorized_keys"
  chmod 0600 "/home/$ACCOUNT/.ssh/authorized_keys"

  # sshd forces the gate for the account too. No AllowTcpForwarding, AllowAgentForwarding or
  # AllowStreamLocalForwarding here: fil's daemon mirrors those settings as its own server policy
  # (DESIGN.md §11.2), so they would refuse every forward and the agent relay of the
  # M2 scenarios, as sshd's would. ssh's own forwarding on this login is off anyway: the key's
  # `restrict` option turns off port, agent and X11 forwarding and the pty.
  cat > "$DROPIN" << EOF
# tests/arm64/remote.sh (fil): the throwaway fil-test account reaches only the gate.
Match User $ACCOUNT
    ForceCommand $GATE
    AuthenticationMethods publickey
    X11Forwarding no
    PermitTunnel no
    PermitTTY no
    PermitUserRC no
EOF
  start_ns

  # Lingering: fil runs each session in a `systemd-run --user --scope`, which needs the user
  # manager, and the daemon outlives the login (pi_e2e's `lingering` scenario).
  loginctl enable-linger "$ACCOUNT"
  uid=$(id -u "$ACCOUNT")
  for ((i = 0; i < 100; i++)); do
    [[ -S /run/user/$uid/bus ]] && break
    sleep 0.1
  done
  [[ -S /run/user/$uid/bus ]] || die "the user manager of $ACCOUNT did not start"

  # The host key, for the client's ssh (BatchMode) and for the cut relay's HostKeyAlias.
  for ((i = 0; i < 50; i++)); do
    ssh-keyscan -t ed25519 "$HOST_IP" > /tmp/fil-arm64-hostkey 2> /dev/null && [[ -s /tmp/fil-arm64-hostkey ]] && break
    sleep 0.2
  done
  [[ -s /tmp/fil-arm64-hostkey ]] || die "the host's sshd does not answer"
  as_client touch "$home/.ssh/known_hosts"
  as_client ssh-keygen -q -R "$HOST_IP" -f "$home/.ssh/known_hosts" > /dev/null 2>&1 || true
  as_client tee -a "$home/.ssh/known_hosts" < /tmp/fil-arm64-hostkey > /dev/null
  rm -f /tmp/fil-arm64-hostkey

  # Captured first: under pipefail, `ssh | grep -q` fails when grep exits before ssh has written.
  local status
  status=$(as_client ssh -o BatchMode=yes "$TARGET" status) || die "the gate's \`status\` failed over ssh"
  grep -q -- '--- processes' <<< "$status" || die "the gate does not answer \`status\` over ssh"
  echo "arm64-remote: $ACCOUNT ready behind the gate ($(uname -m))"
}

install_bins() {
  local dir=${1:?install DIR} b sha kind
  for b in fil fwdprobe; do
    [[ -f $dir/$b && -f $dir/$b.sha256 ]] || die "$dir/$b or its .sha256 is missing"
    sha=$(cut -d' ' -f1 "$dir/$b.sha256")
    (cd "$dir" && sha256sum -c "$b.sha256" > /dev/null) || die "$dir/$b does not match its .sha256"
    # A static aarch64 binary, as on the Pis: the remote side is the release target's build.
    kind=$(file -b "$dir/$b")
    grep -Eq 'ELF 64-bit.*ARM aarch64.*(statically linked|static-pie linked)' <<< "$kind" ||
      die "$dir/$b is not a static aarch64 ELF: $kind"
    ssh -o BatchMode=yes "$TARGET" install "$b" "$sha" < "$dir/$b"
  done
  ssh -o BatchMode=yes "$TARGET" run fil --version
}

teardown() {
  throwaway teardown
  local home
  loginctl terminate-user "$ACCOUNT" 2> /dev/null || true
  loginctl disable-linger "$ACCOUNT" 2> /dev/null || true
  if id "$ACCOUNT" > /dev/null 2>&1; then
    userdel --remove "$ACCOUNT" 2> /dev/null || userdel --remove --force "$ACCOUNT"
  fi
  stop_ns_sshd
  ip link del fil-here 2> /dev/null || true
  ip netns del "$NS" 2> /dev/null || true
  rm -f "$GATE" "$DROPIN"
  home=$(getent passwd "$CLIENT" | cut -d: -f6)
  as_client ssh-keygen -q -R "$HOST_IP" -f "$home/.ssh/known_hosts" > /dev/null 2>&1 || true
  echo "arm64-remote: removed $ACCOUNT, the gate, the sshd drop-in and the namespace"
}

case ${1:-} in
  setup) setup ;;
  install) install_bins "${2:-}" ;;
  teardown) teardown ;;
  *) die "usage: remote.sh setup | install DIR | teardown" ;;
esac
