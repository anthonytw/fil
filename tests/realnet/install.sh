#!/usr/bin/env bash
# Getting fil onto a host, on real OpenSSH (docs/m4/plan.md §3 U1). Two containers
# (docker/Dockerfile.shells): an Ubuntu host whose users log in with bash, dash, zsh, fish and
# tcsh, and a stock Alpine host (busybox ash, busybox head -c). The client runs on this machine,
# through a `ssh` on PATH that adds this part's ssh config. Run: install.sh [SCENARIO...]
#
#   i_manual_message   with no fil on the host, `fil ls` says where fil looks and what to do,
#                      under every login shell (tcsh and csh say a missing command with status 1)
#   i_install_SHELL    `fil install`, `--check`, a second install (nothing to do) and `--remove`,
#                      for each login shell (bash dash zsh fish tcsh ash)
#   i_forced           a key that allows only `fil server --stdio`: `fil install` says so and
#                      writes nothing (the bootstrap's marker tells a forced command apart)
#   i_bundle           a client unpacked like a release archive, whose own binary is not eligible:
#                      the bundle for the host is checked against its SHA256SUMS and sent; one
#                      byte changed in it, and nothing is sent
#   i_not_a_binary     `--binary` with a file that is not a binary for the host: refused, nothing
#                      sent
#
# GATED ON M4 OQ-1: `fil install` exists only in a build with the `install-bootstrap` feature,
# which this part builds for itself: a release build with the builder's paths remapped and marked
# with FIL_RELEASE_BUILD=1, standing in for the release workflow, so it passes the eligibility
# checks. No release build has the feature until the owner says yes.
set -uo pipefail
# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

SH_HOST=rn-shells
ALP_HOST=rn-alpine
IN_DIR=$OUT/run/install
SHELLS=(bash dash zsh fish tcsh)

install_setup() {
  command -v docker > /dev/null || { echo "realnet: docker is required" >&2; exit 2; }
  docker info > /dev/null 2>&1 || { echo "realnet: the docker daemon is not reachable" >&2; exit 2; }
  install_teardown quiet
  mkdir -p "$IN_DIR" "$OUT/logs"
  local target
  case $(uname -m) in
    x86_64) target=x86_64-unknown-linux-musl ;;
    aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
    *) echo "realnet: unsupported architecture $(uname -m)" >&2; exit 2 ;;
  esac
  # A build of its own (its own target directory, so the other parts' cache is untouched): the
  # feature on, the builder's paths remapped, marked as a release build, no test hooks.
  local tdir=${CARGO_TARGET_DIR:-$REPO_ROOT/target}/install-boot
  log "building fil with the install-bootstrap feature for $target"
  (cd "$REPO_ROOT" && CARGO_TARGET_DIR=$tdir FIL_RELEASE_BUILD=1 \
    RUSTFLAGS="--remap-path-prefix=$HOME=/build-home --remap-path-prefix=$REPO_ROOT=/src" \
    cargo build --release --locked --target "$target" -p fil --features install-bootstrap) || exit 1
  cp "$tdir/$target/release/fil" "$IN_DIR/fil"
  if grep -aqE 'FIL_TEST_|/home/|/Users/' "$IN_DIR/fil"; then
    echo "realnet: the install build still has test hooks or builder paths in it" >&2
    exit 1
  fi
  printf 'IN_ARCH=%q\n' "$(uname -m)" > "$OUT/install.env"

  local ctx=$OUT/ctx-install
  rm -rf "$ctx"
  mkdir -p "$ctx/bin"
  cp "$REALNET_DIR"/docker/* "$ctx/"
  cp "$IN_DIR/fil" "$ctx/bin/fil-boot"
  docker build -q -f "$ctx/Dockerfile.shells" --target shells -t fil-realnet-shells "$ctx" > /dev/null || exit 1
  docker build -q -f "$ctx/Dockerfile.shells" --target alpine -t fil-realnet-alpine "$ctx" > /dev/null || exit 1

  rm -f "$IN_DIR/id" "$IN_DIR/id.pub"
  ssh-keygen -q -t ed25519 -N '' -C realnet-install -f "$IN_DIR/id" < /dev/null
  docker run -d --name "$SH_HOST" --hostname shells fil-realnet-shells > /dev/null || exit 1
  docker run -d --name "$ALP_HOST" --hostname alpine fil-realnet-alpine > /dev/null || exit 1
  local pub u
  pub=$(cat "$IN_DIR/id.pub")
  for u in "${SHELLS[@]}"; do
    docker exec "$SH_HOST" sh -c "printf '%s\n' '$pub' > /etc/fil-realnet/keys/u-$u"
  done
  docker exec "$SH_HOST" sh -c "printf '%s\n' 'command=\"/opt/fil/fil server --stdio\" $pub' > /etc/fil-realnet/keys/gated"
  docker exec "$ALP_HOST" sh -c "printf '%s\n' '$pub' > /etc/fil-realnet/keys/u-ash"
  docker exec "$SH_HOST" chmod -R a+r /etc/fil-realnet/keys
  docker exec "$ALP_HOST" chmod -R a+r /etc/fil-realnet/keys

  local sh_ip alp_ip
  sh_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$SH_HOST")
  alp_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$ALP_HOST")
  cat > "$IN_DIR/ssh_config" << EOF
Host shells
  HostName $sh_ip
Host alpine
  HostName $alp_ip
Host *
  IdentityFile $IN_DIR/id
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  BatchMode yes
  ConnectTimeout 10
  LogLevel ERROR
EOF
  # fil runs `ssh` from PATH: this one adds the part's config.
  mkdir -p "$IN_DIR/path"
  printf '#!/bin/sh\nexec %s -F %s "$@"\n' "$(command -v ssh)" "$IN_DIR/ssh_config" > "$IN_DIR/path/ssh"
  chmod 755 "$IN_DIR/path/ssh"
  printf '[defaults]\ntransport = "pipe"\n' > "$IN_DIR/config.toml"
  local i
  for h in u-bash@shells u-ash@alpine; do
    for ((i = 0; i < 100; i++)); do
      "$IN_DIR/path/ssh" -T "$h" true > /dev/null 2>&1 && break
      sleep 0.3
    done
    "$IN_DIR/path/ssh" -T "$h" true || { echo "realnet: ssh to $h does not work" >&2; exit 1; }
  done
  log "install hosts up: shells at $sh_ip, alpine at $alp_ip"
}

install_teardown() {
  if [[ ${1:-} != quiet ]]; then
    docker logs "$SH_HOST" > "$OUT/logs/install-shells-sshd.log" 2>&1 || true
  fi
  docker rm -f "$SH_HOST" "$ALP_HOST" > /dev/null 2>&1 || true
}

# ---------------------------------------------------------------------------------------------
# Helpers

# in_fil BINARY ARGS...: a fil client with this part's ssh and config, its output in $IN_OUT.
in_fil() {
  local bin=$1
  shift
  local rc=0
  IN_OUT=$(PATH="$IN_DIR/path:$PATH" XDG_STATE_HOME="$IN_DIR/state" timeout 120 "$bin" --config "$IN_DIR/config.toml" "$@" 2>&1) || rc=$?
  log "fil $*: rc=$rc: $IN_OUT"
  return $rc
}

# host_of USER: the container a user lives on.
host_of() { if [[ $1 == u-ash ]]; then echo "$ALP_HOST"; else echo "$SH_HOST"; fi; }
at_of() { if [[ $1 == u-ash ]]; then echo "$1@alpine"; else echo "$1@shells"; fi; }

# on USER COMMAND: a shell command in the user's container, as that user.
on() { docker exec -u "$1" "$(host_of "$1")" sh -c "$2"; }

dir_name() { "$IN_DIR/fil" --version | head -n 1 | sed -E 's/^fil ([^ ]+) \(wire ([0-9]+)\.([0-9]+)\)$/\1-w\2.\3/'; }

# ---------------------------------------------------------------------------------------------
# Scenarios

i_manual_message() {
  local u
  for u in "${SHELLS[@]/#/u-}" u-ash; do
    if in_fil "$IN_DIR/fil" ls "$(at_of "$u")"; then fail "$u: fil ls worked with no fil on the host"; fi
    assert "$u: the manual-install words" grep -q "fil is not installed on the remote host" <<< "$IN_OUT"
    assert "$u: where fil looks" grep -q "PATH of a non-interactive ssh login" <<< "$IN_OUT"
  done
}

# i_install_one USER
i_install_one() {
  local u=$1 at d
  at=$(at_of "$u")
  d=$(dir_name)
  on "$u" 'rm -rf "$HOME/.local"'
  in_fil "$IN_DIR/fil" install "$at" || fail "$u: install failed"
  assert "$u: installed" grep -q "installed fil" <<< "$IN_OUT"
  assert "$u: the copy, 0700" on "$u" "test \"\$(stat -c %a \"\$HOME/.local/share/fil/$d\")\" = 700 && test \"\$(stat -c %a \"\$HOME/.local/share/fil/$d/fil\")\" = 700"
  assert "$u: no upload directory left" on "$u" "! ls -a \"\$HOME/.local/share/fil\" | grep -q '^\.new-'"
  assert "$u: the same bytes" cmp -s "$IN_DIR/fil" <(on "$u" "cat \"\$HOME/.local/share/fil/$d/fil\"")
  in_fil "$IN_DIR/fil" install "$at" || fail "$u: a second install failed"
  assert "$u: nothing to do the second time" grep -q "already there" <<< "$IN_OUT"
  in_fil "$IN_DIR/fil" install --check "$at" || fail "$u: check failed"
  assert "$u: check lists it" grep -qF "$d (this version)" <<< "$IN_OUT"
  in_fil "$IN_DIR/fil" install --remove "$at" || fail "$u: remove failed"
  assert "$u: removed" grep -q "removed 1 " <<< "$IN_OUT"
  assert "$u: the install root is gone, its parents stay" on "$u" 'test ! -e "$HOME/.local/share/fil" && test -d "$HOME/.local/share"'
  note "shell=${u#u-}"
}

i_install_bash() { i_install_one u-bash; }
i_install_dash() { i_install_one u-dash; }
i_install_zsh() { i_install_one u-zsh; }
i_install_fish() { i_install_one u-fish; }
i_install_tcsh() { i_install_one u-tcsh; }
i_install_ash() { i_install_one u-ash; }

i_forced() {
  if in_fil "$IN_DIR/fil" install gated@shells; then fail "an install through a forced command worked"; fi
  assert "the forced command is named" grep -q "forced command allows only the plain one" <<< "$IN_OUT"
  # The relay it ran may start its daemon (its state directory is the plain command's own), but
  # no install directory appears and no binary was sent to it.
  assert "no install directory on the host" on gated 'test ! -e "$HOME/.local/share/fil"'
  # The plain command still works through the gate.
  in_fil "$IN_DIR/fil" ls gated@shells || fail "fil ls through the gate failed"
}

# A client laid out like an unpacked release archive (docs/m4/release.md §4.3).
make_layout() { # make_layout DIR TAMPER(0|1)
  local d=$1 arch
  case $IN_ARCH in x86_64) arch=x86_64-linux ;; *) arch=aarch64-linux ;; esac
  rm -rf "$d"
  mkdir -p "$d/bin" "$d/libexec/fil/remote/$arch"
  cp "$IN_DIR/fil" "$d/bin/fil"
  cp "$IN_DIR/fil" "$d/libexec/fil/remote/$arch/fil"
  (cd "$d/libexec/fil/remote" && sha256sum "$arch/fil" > SHA256SUMS)
  if [[ $2 == 1 ]]; then
    printf '\001' | dd of="$d/libexec/fil/remote/$arch/fil" bs=1 seek=4096 conv=notrunc status=none
  fi
}

i_bundle() {
  on u-zsh 'rm -rf "$HOME/.local"'
  make_layout "$IN_DIR/rel-bad" 1
  if in_fil "$IN_DIR/rel-bad/bin/fil" install u-zsh@shells; then fail "a tampered bundle was sent"; fi
  assert "the tampered bundle is refused" grep -q "does not match the bundle" <<< "$IN_OUT"
  assert "nothing written on the host" on u-zsh 'test ! -e "$HOME/.local"'
  make_layout "$IN_DIR/rel" 0
  in_fil "$IN_DIR/rel/bin/fil" install u-zsh@shells || fail "the bundle install failed"
  assert "installed from the bundle" on u-zsh "test -x \"\$HOME/.local/share/fil/$(dir_name)/fil\""
  in_fil "$IN_DIR/rel/bin/fil" install --remove u-zsh@shells || fail "remove failed"
}

i_not_a_binary() {
  on u-dash 'rm -rf "$HOME/.local"'
  if in_fil "$IN_DIR/fil" install u-dash@shells --binary /dev/null; then fail "an empty file was sent"; fi
  if in_fil "$IN_DIR/fil" install --binary "$REALNET_DIR/install.sh" u-dash@shells; then fail "a script was sent"; fi
  assert "not a binary for the host" grep -q "not a binary for the remote host" <<< "$IN_OUT"
  assert "nothing written on the host" on u-dash 'test ! -e "$HOME/.local"'
}

part_main install install_setup install_teardown \
  i_manual_message i_install_bash i_install_dash i_install_zsh i_install_fish i_install_tcsh \
  i_install_ash i_forced i_bundle i_not_a_binary -- "$@"
