# fil command index. `just` lists recipes.

# Extra flags for `cargo deny` (the advisory database is fetched on each run). `FIL_DENY_FLAGS=--offline
# just ci` checks against the cached database instead: that is NOT a merge pass (CLAUDE.md § Delivery).
deny_flags := env("FIL_DENY_FLAGS", "")

# Where cargo puts its output. The recipes that run a built binary use this, so they work under
# `just vm-run` (docs/dev/vm.md), which gives each worktree its own CARGO_TARGET_DIR.
target_dir := if env("CARGO_TARGET_DIR", "") == "" { source_directory() / "target" } else { env("CARGO_TARGET_DIR") }

default:
    @just --list

# The merge gate: run before every merge (CLAUDE.md § Delivery). `deny` runs first so that a network
# failure (the advisory database) shows up at once, not after the whole test suite.
ci: deny fmt-check clippy check-default release-deps release-test test

fmt-check:
    cargo fmt --all --check

clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Default-feature build of `fil`, as released. The other recipes use --all-features, which also
# compiles the vt100 backend and the test hooks; this proves `fil` builds without them.
check-default:
    cargo check -p fil

# The vt100 emulator backend must stay out of the release binary (only alacritty ships; DESIGN.md
# §6.5). `cargo tree -i` exits 0 even when it prints "nothing to print" to stderr, so test its
# output: it lists vt100's dependents on stdout when vt100 is in the graph.
release-deps:
    #!/usr/bin/env bash
    set -euo pipefail
    out=$(cargo tree -q -p fil -e normal,build --target all -i vt100)
    test -z "$out" || { echo "release-deps: vt100 is in the release build graph of fil" >&2; exit 1; }
    # The test hooks (FIL_TEST_WIRE_REVISION and friends) must not reach the released binary: a
    # production fil must never honor an environment variable that changes what wire it speaks.
    # fil-forward is checked once fil depends on it (until then `cargo tree -i` finds no such package).
    for c in fil-proto fil-daemon fil-transport fil-session fil-forward; do
      if ! tree=$(cargo tree -q -p fil -e normal,build,features --target all -i "$c" 2>&1); then
        case "$tree" in *"did not match any packages"*) continue ;; esac
        echo "$tree" >&2; exit 1
      fi
      # (The install bootstrap is in every build since the owner approved it, M4 OQ-1, 2026-11-04;
      # crates/fil/tests/remote_commands.rs checks what reaches ssh; release-check's `sh -c` scan is
      # a tripwire.)
      hooks=$(grep -E 'feature "test-hooks"' <<<"$tree" || true)
      test -z "$hooks" || { echo "release-deps: $c has a test-only feature in the release build of fil: $hooks" >&2; exit 1; }
    done

test:
    cargo test --workspace --all-features

# Regenerate the wire golden (crates/fil-proto/tests/golden/wire.txt) after a deliberate wire
# change. Bump PROTOCOL_REVISION in crates/fil-proto/src/limits.rs FIRST: this refuses to write
# changed bytes under an unchanged revision (CLAUDE.md § The wire).
wire-golden:
    FIL_UPDATE_GOLDEN=1 cargo test -p fil-proto --test wire_golden the_wire_matches_the_golden_for_this_revision

# Fetches the RustSec advisory database, so it needs network access.
# License, source and ban policy, RustSec advisories and yanked crates (deny.toml).
deny:
    cargo deny {{deny_flags}} check advisories licenses bans sources

# Slow link-simulator soak: many seeds, harsh conditions (not part of `ci`).
netsim-soak:
    cargo test -p fil-netsim --test quic --release -- --ignored --nocapture soak

# The MUX model (crates/fil-proto/tests/mux_model.rs: both ends over a reordering, dropping link)
# over many seeds, in release. Not part of `ci`, which runs a few. MUX_SEED_BASE moves the range.
mux-soak seeds="5000":
    MUX_SEEDS={{seeds}} cargo test --release -p fil-proto --test mux_model -- --nocapture

# Fuzz one target for SECONDS (libFuzzer via cargo-fuzz). Needs the NIGHTLY toolchain (the sanitizer
# flags are nightly-only) and cargo-fuzz:
#   rustup toolchain install nightly --profile minimal && cargo install cargo-fuzz --locked
# Not part of `ci`: the fuzz crates are workspaces of their own (crates/*/fuzz) that the stable build
# never sees; `fuzz-deny` (run first) holds them to the same license policy. `just fuzz` alone lists the targets. The seeds are committed (fuzz/seeds/TARGET); what
# the run finds goes to the git-ignored fuzz/corpus/TARGET, a crash to fuzz/artifacts/TARGET. To
# replay a crash on stable: put its bytes in a regression test of the owning crate.
fuzz target="" seconds="60":
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{source_directory()}}"
    targets=$(ls crates/*/fuzz/fuzz_targets/*.rs 2>/dev/null | sed -E 's#crates/([^/]+)/fuzz/fuzz_targets/(.*)\.rs#\2 (\1)#')
    if [[ -z "{{target}}" ]]; then
      echo "usage: just fuzz TARGET [SECONDS]   (nightly toolchain + cargo-fuzz; see the comment on this recipe)"
      echo "targets:"; sed 's/^/  /' <<<"$targets"
      exit 0
    fi
    src=$(ls crates/*/fuzz/fuzz_targets/"{{target}}".rs 2>/dev/null || true)
    if [[ -z "$src" || $(wc -l <<<"$src") -ne 1 ]]; then
      echo "fuzz: no (single) target named '{{target}}'. Targets:" >&2; sed 's/^/  /' <<<"$targets" >&2; exit 1
    fi
    crate=${src%/fuzz/fuzz_targets/*}
    if ! rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
      echo "fuzz: needs a nightly toolchain: rustup toolchain install nightly --profile minimal" >&2; exit 1
    fi
    if ! cargo fuzz --version >/dev/null 2>&1; then
      echo "fuzz: needs cargo-fuzz: cargo install cargo-fuzz --locked" >&2; exit 1
    fi
    if ! cargo deny --version >/dev/null 2>&1; then
      echo "fuzz: needs cargo-deny (for fuzz-deny): cargo install cargo-deny --locked" >&2; exit 1
    fi
    just fuzz-deny
    cd "$crate"
    dirs=("fuzz/corpus/{{target}}")
    if [[ -d "fuzz/seeds/{{target}}" ]]; then dirs+=("fuzz/seeds/{{target}}"); fi
    mkdir -p "fuzz/corpus/{{target}}"
    # -timeout: an input that takes 20 s is a hang, which is a bug. -max_len: past a frame's body
    # limit (16 KiB), and for the preamble scanner past its default skip limit (64 KiB).
    max_len=17000
    if [[ "{{target}}" == preamble ]]; then max_len=70000; fi
    # The sshd target makes a scratch directory in $TMPDIR; give it one to throw away.
    scratch=$(mktemp -d)
    trap 'rm -rf "$scratch"' EXIT
    TMPDIR=$scratch cargo +nightly fuzz run "{{target}}" "${dirs[@]}" -- -max_total_time={{seconds}} -timeout=20 -max_len=$max_len

# The license, ban and source policy (deny.toml) on each fuzz crate, which `deny` does not see (they
# are workspaces of their own). The advisory database is not consulted: the fuzz crates ship nothing.
# Resolving a fuzz crate that has no lock file yet needs network access (the registry index).
fuzz-deny:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{source_directory()}}"
    for m in crates/*/fuzz/Cargo.toml; do
      cargo deny --manifest-path "$m" check licenses bans sources
    done

# The forward suite at full size (crates/fil-forward/tests/forward.rs): 256 MiB each way through a
# forward, 20,000 sequential connections, on link::mem, the pipe and QUIC. `just ci` runs the same
# tests at 8 MiB and 2,000 (and 300 concurrent off QUIC). Release build. Also runs the SOCKS fuzz
# table and the new-epoch race with more seeds and runs. One test at a time: at these sizes,
# tests side by side overflow loopback listen queues (128 on macOS) and the ephemeral ports.
forward-soak mib="256" seq="20000":
    FIL_FORWARD_TEST_LIMIT=3600 FIL_FORWARD_BULK_MIB={{mib}} FIL_FORWARD_SEQ={{seq}} FIL_FORWARD_CONCURRENT=1000 FIL_FORWARD_EPOCH_RUNS=150 FIL_SOCKS_SEEDS=500000 cargo test --release -p fil-forward -- --nocapture --test-threads=1

# Regenerate the synthetic emulator fixtures from crates/fil-term/tests/common/synthetic.rs.
fixtures:
    cargo run -q -p fil-term --example fixtures

# Record the real-program fixtures on a PTY (crates/fil-term/examples/record.rs).
# Inspect every output for personal data before committing.
record *scenarios:
    cargo run -q --release -p fil-term --example record -- {{scenarios}}

# Emulator restore-fidelity harness (docs/spikes/emulator.md). Flags: --only NAME, --no-perf, --verbose.
fidelity *args:
    cargo run --release -p fil-term --example fidelity --all-features -- {{args}}

# Prediction overlay harness (docs/spikes/prediction.md): real programs on a PTY behind fil-netsim
# latency, checked against an independent emulator. Flags: --seeds N, --keys N, --latency MS,
# --only PROGRAM, --backend NAME, --hostile, --guard-off NAME, --jobs N, --verbose.
predict-harness *args:
    cargo run --release -p fil-predict --features harness --example predict_harness -- {{args}}

# The full-stack prediction harness (docs/m3/harness.md): the real client engine against a real session
# on a real PTY, over simulated pipe and QUIC links, the user's terminal an emulator that reflows.
# Commands: resize (the true-order resize reproduction; W1's gate), baseline, secret, all. Flags:
# --transport pipe|quic|both, --seeds N, --one-way-ms N, --filter TEXT, --verbose. `--ignored` tests
# of crates/fil-client/tests/predict_e2e.rs are the resize reproduction as a test.
[positional-arguments]
predict-e2e *args:
    cargo run --release -p fil-client --example predict_e2e -- "$@"

# Prediction demo in your own terminal: `just predict-demo 200ms` (latency each way).
# Flags: --backend NAME, --no-predict, --plain, --no-sync.
predict-demo *args:
    cargo run --release -p fil-predict --features harness --example predict_demo -- {{args}}

# Scripted prediction scenarios at scale (no PTY): many seeds, release build.
predict-soak seeds="200":
    FIL_PREDICT_SEEDS={{seeds}} cargo test --release -p fil-predict --test sim

# Guard-off sweep: proves every overlay guard is needed (slow; not in `ci`). Run after changing a guard.
predict-mutants:
    cargo test -p fil-predict --release --test mutants -- --ignored

# Status-line fuzz (crates/fil-predict/tests/status.rs): many random sessions checked against the
# oracle, in release. Run after changing the overlay's status drawing.
status-soak seeds="600":
    FIL_STATUS_SEEDS={{seeds}} cargo test --release -p fil-predict --test status random

# The client's real-terminal tests on a pseudo-terminal (crates/fil-client/tests/tty_pty.rs):
# raw mode in and out, input, window size, signals, hangup, the panic hook. They raise signals at
# their own process, so they run one at a time and are not part of `ci`.
client-pty:
    cargo test -p fil-client --test tty_pty -- --ignored --test-threads=1

# Real-network harness (docs/m1/realnet.md): real OpenSSH in Docker and real NAT/netem in network
# namespaces. Linux only, needs Docker and root; refuses on macOS. Never part of `ci`.
# `just realnet` runs everything; `just realnet ssh`, `udp`, `sysd`, `policy`, `fwd` (the M2
# forwards, docs/m2/realnet.md) or `agent`; add scenario names to narrow.
# `just realnet latency` is the latency lab (docs/m2/latency.md), not part of the default run.
realnet *args:
    tests/realnet/run.sh {{args}}

# Local Linux VM (docs/dev/vm.md): root-only tests without GitHub. The Lima instance `fil` (override
# with FIL_VM) is Ubuntu aarch64 with Docker, netem and no access to this Mac; code reaches it by
# rsync. GitHub's linux-check stays the x86_64 reference.

# Once after creating the VM: conntrack, the pinned toolchain, just and cargo-deny.
vm-setup:
    tests/realnet/vm.sh setup

# Copy this worktree (uncommitted work included) to ~/src/<worktree name> in the VM.
vm-sync:
    tests/realnet/vm.sh sync

# Sync, then run a command in that directory in the VM, with the pinned toolchain: `just vm-run just ci`.
[positional-arguments]
vm-run +cmd:
    tests/realnet/vm.sh run "$@"

# Sync, then `just realnet PART` in the VM, one run at a time across the whole VM (a lock). Parts and
# scenario names as for `realnet`; none runs everything. Killed after FIL_VM_TIMEOUT (default 60m).
[positional-arguments]
vm-realnet *args:
    tests/realnet/vm.sh realnet "$@"

# Remove this worktree's copy and build directory from the VM.
vm-clean:
    tests/realnet/vm.sh clean

# A real run of the `fil` binary without sshd: the pipe command is the local `fil server --stdio`
# (docs/m1/binary.md); the relay starts the daemon. `just local` attaches to session "local-pipe"
# over the pipe only; `just local auto` (session "local-auto") also lets it upgrade to QUIC on
# loopback. The session defaults to local-TRANSPORT so one run never takes over another.
local transport="pipe" session=("local-" + transport):
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{transport}}" in
      pipe | auto | direct) ;;
      *) echo "usage: just local [pipe|auto|direct] [session]" >&2; exit 2 ;;
    esac
    cargo build -q -p fil
    mkdir -p target/local
    config=target/local/config-{{transport}}.toml
    {
      printf '[defaults]\ntransport = "%s"\npipe = ["%s/debug/fil", "server", "--stdio"]\n' "{{transport}}" "{{target_dir}}"
      if [ "{{transport}}" != pipe ]; then
        printf '[defaults.direct]\naddress = "127.0.0.1"\n'
      fi
    } > "$config"
    exec "{{target_dir}}/debug/fil" --config "$config" localhost "{{session}}"

# M1 scenarios on the lab Raspberry Pis through the fil-test gate (docs/m1/pi.md): this machine's
# client in a pseudo-terminal against `fil server --stdio` on each Pi. The Pi binary must be
# installed first (docs/m1/pi.md). Flags: --hosts pi1,pi2, --only NAME, --verbose, --list.
# Targets: FIL_PI_TARGETS="pi1=fil-test@HOST,pi2=fil-test@HOST" (required; the keys are the hosts it may touch). Never part of `ci`.
pi-e2e *args:
    cargo build --release -p fil --bins
    cargo build --release -p fil --example pi_e2e
    just no-test-hooks "{{target_dir}}/release/fil"
    "{{target_dir}}/release/examples/pi_e2e" {{args}}

# The drop-in suite (docs/m1/dropin.md): everyday ssh, et and mosh work (vim, less, top, history,
# floods, unicode, resizes, detach, takeover, a killed ssh or client, a sleeping client) through the
# real client on the lab Pis. Flags: --hosts pi1,pi2, --only NAME, --list, --verbose, --require TOOLS,
# --allow-missing, --allow-skip, --strict. `--local` runs against a private daemon on this machine
# instead (no targets needed; a development aid). Targets as for pi-e2e. A skipped scenario or a
# missing tool fails the run unless allowed. The M2 forward scenarios (docs/m2/dropin.md) also use
# this machine's fwdprobe, built here, and the host's, installed through the gate. Never part of `ci`.
dropin *args:
    cargo build -q --release -p fil --bins
    cargo build -q --release -p fil --example dropin
    cargo build -q --release -p fil-daemon --bin fil-daemon-dev
    cargo build -q --release -p fil-testbins --bin fwdprobe
    just no-test-hooks "{{target_dir}}/release/fil" "{{target_dir}}/release/fil-daemon-dev"
    "{{target_dir}}/release/examples/dropin" {{args}}

# macOS as the REMOTE host (DESIGN.md §16.9, docs/m4/plan.md MAC): the real release `fil` serves a
# session over a real ssh login to this Mac (an sshd of its own on a high loopback port with
# throwaway keys; MACOS_SSHD=system uses the system sshd, on a throwaway runner only), driven by the
# client on a pty. Scenarios: `just macos-remote --list`; narrow with names. It starts and stops a
# `fil` daemon of its own and refuses to run beside one of yours (tests/macos/remote.sh). macOS
# only; never part of `ci`. `fil` is built in an invocation of its own, so it has no test hooks.
macos-remote *args:
    cargo build -q --release -p fil --bins
    cargo build -q --release -p fil-testbins --bin ptydrive --bin fwdprobe
    just no-test-hooks "{{target_dir}}/release/fil"
    FIL_BIN_DIR="{{target_dir}}/release" tests/macos/remote.sh {{args}}

# Fails if a binary contains the test hooks' variable names (FIL_TEST_*): such a build reads the
# environment to change what wire it speaks and where its sockets are, and must never be the fil
# that is installed or released. Building an example or a test with `-p fil` unifies the dev-
# dependencies' `test-hooks` features into every target of that invocation, which is why the
# recipes that install `target/release/fil` build the binary in an invocation of its own.
no-test-hooks +binaries:
    #!/usr/bin/env bash
    set -euo pipefail
    for b in {{binaries}}; do
      # A missing binary must fail, not pass: grep reports it as an error (status 2), which an
      # `if grep` would read as "no hooks found".
      [[ -f $b && -r $b ]] || { echo "no-test-hooks: $b is missing or unreadable (build it first)" >&2; exit 1; }
      rc=0
      grep -aq 'FIL_TEST_' "$b" || rc=$?
      case $rc in
        0)
          echo "no-test-hooks: $b was built with the test hooks (FIL_TEST_*); rebuild it with its own 'cargo build --release -p fil --bins'" >&2
          exit 1
          ;;
        1) ;;
        *) echo "no-test-hooks: could not read $b (grep status $rc)" >&2; exit 1 ;;
      esac
    done

# ---- Releases (docs/m4/release.md; the scripts are in tests/release/) ----

# The release scripts' own tests: the workflow lint (permissions, pins, no secrets or caches) with a
# mutation for each rule, tag verification (the signers come from main, never from the tag),
# release-check on synthetic releases, and the checksums. Part of `ci`. With FIL_RELEASE_TEST_DOCKER=1
# it also assembles archives twice in the pinned image and requires identical hashes (pulls the image).
release-test:
    tests/release/selftest.sh

# Build the release the way release.yml does: both Linux binaries in the digest-pinned `cross`
# images (needs `cross` 0.2.5 and Docker; on an arm64 host see docs/m4/release.md section 9), the
# universal macOS binary when run on macOS, then the archives, the source archive and SHA256SUMS
# into OUT/files (assembled by the GNU tar and gzip inside the pinned image). Off macOS, name a macOS
# binary with FIL_MACOS_BINARY=path (a dry run's, for example) or the archives are not assembled.
release-build out="target/release-dist":
    #!/usr/bin/env bash
    set -euo pipefail
    out="{{out}}"
    rm -rf "$out"
    for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
      tests/release/build-binary.sh "$t" "$out/bins"
    done
    if [ "$(uname -s)" = Darwin ]; then
      tests/release/macos-build.sh "$out/bins"
    elif [ -n "${FIL_MACOS_BINARY:-}" ]; then
      mkdir -p "$out/bins/universal-apple-darwin"
      cp "$FIL_MACOS_BINARY" "$out/bins/universal-apple-darwin/fil"
    else
      echo "release-build: built the Linux binaries in $out/bins; no macOS binary (set FIL_MACOS_BINARY), so no archives" >&2
      exit 0
    fi
    tests/release/package.sh --bins "$out/bins" --out "$out/files"
    echo "release-build: the release files are in $out/files; run: just release-check --dry-run $out/files" >&2

# Everything a release candidate must satisfy: `just release-check --dry-run DIR` for a build of a
# branch, `just release-check --tag vX.Y.Z --complete DIR` for a release. One top-level entry per
# archive, the section 4.3 layout, byte-identical bundle copies, SHA256SUMS exactly the four archives,
# no builder path (/Users/, /home/) or FIL_TEST_ string in a binary, the path remap applied,
# `no-test-hooks` and `release-deps`, static Linux binaries, `fil --version` where it can run, the tag,
# the changelog and the wire. See tests/release/check.sh.
[positional-arguments]
release-check *args:
    tests/release/check.sh "$@"

# The owner's check before signing, and anyone's afterwards: `just release-verify 0.1.0`. Tag and
# SHA256SUMS.sig against .github/release-signers on main, the attestations (narrowly), release-check,
# then an independent rebuild of every Linux binary and archive in the Lima VM under amd64 emulation
# (Rosetta), compared hash for hash. `--dry-run --from-run RUN --ref SHA` checks a dry run instead.
# See tests/release/verify.sh and docs/m4/release.md section 9.
[positional-arguments]
release-verify version *args:
    tests/release/verify.sh "$@"
