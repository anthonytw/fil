#!/usr/bin/env bash
# Run the real-network harness (and anything else) in the local Lima VM (docs/dev/vm.md). This runs
# on the developer's Mac, not in the harness: the justfile's vm-* recipes call it.
#
#   vm.sh check                 the instance exists and is running
#   vm.sh setup                 install what the VM lacks (conntrack, the toolchain, just, cargo-deny, cargo-about, cargo-nextest)
#   vm.sh sync                  rsync this worktree to ~/src/<worktree> in the VM
#   vm.sh run COMMAND...        sync, then run COMMAND in that directory with the pinned toolchain
#   vm.sh realnet [ARGS...]     sync, then `just realnet ARGS` in the VM under the VM-wide lock
#   vm.sh clean                 remove this worktree's copy and build directory from the VM
#
# Written for bash 3.2 (the Mac's /bin/bash). The code reaches the VM only by rsync over Lima's own
# ssh; the VM mounts nothing from this machine.
set -euo pipefail

VM=${FIL_VM:-fil}
LIMA_HOME_DIR=${LIMA_HOME:-$HOME/.lima}
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
# The name is the directory's plus a short hash of its absolute path, so two checkouts with the
# same name (a second clone, say) never share a copy or a build directory.
NAME=$(basename "$ROOT")-$(printf %s "$ROOT" | shasum | cut -c1-8)
# One copy and one cargo target directory per worktree, so agents on different branches never share
# build state. Paths are relative to the VM user's home.
SRC="src/$NAME"
TARGET="target/$NAME"
# Held for a whole realnet run: the harness uses fixed Docker, network namespace and user names,
# so two runs (from any worktree) would trample each other.
LOCK=.fil-realnet.lock
TIMEOUT=${FIL_VM_TIMEOUT:-60m}

die() {
  echo "vm: $*" >&2
  exit 1
}

check() {
  command -v limactl > /dev/null || die "limactl not found (brew install lima); see docs/dev/vm.md"
  local status
  status=$(limactl list --format '{{.Status}}' "$VM" 2> /dev/null || true)
  case $status in
    Running) ;;
    "") die "no Lima instance named '$VM'; create it as described in docs/dev/vm.md" ;;
    *) die "Lima instance '$VM' is $status; start it with: limactl start $VM" ;;
  esac
}

# vm_sh SCRIPT [ARG...]: run a bash script in the VM (ARGs are $1.. of the script).
vm_sh() {
  local script=$1
  shift
  limactl shell --workdir / "$VM" -- bash -c "$script" vm-sh "$@"
}

# The prelude every remote command starts with: the pinned toolchain from rust-toolchain.toml.
# rustup 1.28 and later do not install it on first use, so do it the way the CI jobs do.
# It also sets `umask 022`: Ubuntu's login default (002) makes the tests' temporary directories
# group-writable, and fil rightly refuses such a directory for its offline log (a test assumes 022).
PRELUDE='export PATH="$HOME/.cargo/bin:$PATH"
umask 022
src=$1 target=$2 timeout=$3
shift 3
cd "$HOME/$src"
export CARGO_TARGET_DIR="$HOME/$target"
rustup show active-toolchain > /dev/null 2>&1 || rustup toolchain install
'

# The rsync on this machine must understand the options sync() uses. macOS's own (openrsync) does;
# one that does not fails here with a clear message, not with a usage screen.
check_rsync() {
  command -v rsync > /dev/null || die "rsync not found (brew install rsync)"
  local t
  t=$(mktemp -d)
  mkdir "$t/a" "$t/b"
  rsync -a --delete --exclude=/x --filter=':- .gitignore' "$t/a/" "$t/b/" > /dev/null 2>&1 \
    || { rm -rf "$t"; die "this rsync ($(command -v rsync)) rejects -a --delete --exclude --filter; install a current one (brew install rsync)"; }
  rm -rf "$t"
}

sync() {
  check
  check_rsync
  local cfg=$LIMA_HOME_DIR/$VM/ssh.config
  [[ -f $cfg ]] || die "no ssh config for '$VM' at $cfg"
  # Uncommitted work is what matters, so this copies the working tree, not a commit. Whatever
  # .gitignore ignores stays out (target/ among it), and so do .git (a worktree's is a file that
  # points outside the tree) and .claude. --delete removes files deleted here; excluded paths in
  # the VM (the build outputs, target/realnet) are left alone.
  # (No --mkpath: macOS's rsync lacks it, so the directory is made first.)
  vm_sh 'mkdir -p "$HOME/$1"' "$SRC"
  rsync -a --delete \
    -e "ssh -F $cfg" \
    --exclude=/.git --exclude=/.claude --exclude=/target --exclude=.DS_Store \
    --filter=':- .gitignore' \
    "$ROOT/" "lima-$VM:$SRC/"
}

run() {
  sync
  vm_sh "$PRELUDE"'exec "$@"' "$SRC" "$TARGET" "$TIMEOUT" "$@"
}

realnet() {
  sync
  # `flock -o` keeps the lock in the flock process, so daemons a scenario leaves behind cannot hold it.
  vm_sh "$PRELUDE"'lock=$HOME/'"$LOCK"'
if ! flock -n -o "$lock" true; then
  echo "vm-realnet: another realnet run holds the VM lock; waiting for it" >&2
fi
# The policy part runs only where asked for (tests/realnet/policy.sh): the VM is that place.
export FIL_REALNET_POLICY=1
exec flock -o "$lock" timeout -k 60 "$timeout" just realnet "$@"' "$SRC" "$TARGET" "$TIMEOUT" "$@"
}

setup() {
  check
  sync
  # The one package the image lacks (the VM config in docs/dev/vm.md stays as created), then the
  # toolchain and the four tools `just ci` needs, at the versions the CI jobs pin.
  vm_sh "$PRELUDE"'
if ! command -v conntrack > /dev/null; then
  sudo apt-get update -q
  sudo apt-get install -y -q conntrack
fi
command -v just > /dev/null || cargo install --locked just@1.59.0
command -v cargo-deny > /dev/null || cargo install --locked cargo-deny@0.20.2
command -v cargo-about > /dev/null || cargo install --locked cargo-about@0.9.3 --features cli
command -v cargo-nextest > /dev/null || cargo install --locked cargo-nextest@0.9.149
just --version
cargo --version' "$SRC" "$TARGET" "$TIMEOUT"
}

clean() {
  check
  vm_sh 'rm -rf "$HOME/$1" "$HOME/$2"' "$SRC" "$TARGET"
}

cmd=${1:-}
[[ -n $cmd ]] && shift
case $cmd in
  check) check ;;
  sync) sync ;;
  setup) setup ;;
  clean) clean ;;
  run)
    [[ $# -gt 0 ]] || die "usage: vm.sh run COMMAND..."
    run "$@"
    ;;
  realnet) realnet "$@" ;;
  *) die "usage: vm.sh check|setup|sync|run COMMAND...|realnet [ARGS...]|clean" ;;
esac
