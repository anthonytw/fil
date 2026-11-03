# fil command index. `just` lists recipes.

# Extra flags for `cargo deny` (the advisory database is fetched on each run). `FIL_DENY_FLAGS=--offline
# just ci` checks against the cached database instead: that is NOT a merge pass (CLAUDE.md § Delivery).
deny_flags := env("FIL_DENY_FLAGS", "")

default:
    @just --list

# The merge gate: run before every merge (CLAUDE.md § Delivery). `deny` runs first so that a network
# failure (the advisory database) shows up at once, not after the whole test suite.
ci: deny fmt-check clippy check-default release-deps test

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
    for c in fil-proto fil-daemon fil-transport fil-session; do
      hooks=$(cargo tree -q -p fil -e normal,build,features --target all -i "$c" | grep -E 'feature "test-hooks"' || true)
      test -z "$hooks" || { echo "release-deps: $c has its test-hooks feature in the release build of fil" >&2; exit 1; }
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
# `just realnet` runs everything; `just realnet ssh`, `udp` or `sysd`; add scenario names to narrow.
realnet *args:
    tests/realnet/run.sh {{args}}

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
      printf '[defaults]\ntransport = "%s"\npipe = ["%s/target/debug/fil", "server", "--stdio"]\n' "{{transport}}" "{{source_directory()}}"
      if [ "{{transport}}" != pipe ]; then
        printf '[defaults.direct]\naddress = "127.0.0.1"\n'
      fi
    } > "$config"
    exec target/debug/fil --config "$config" localhost "{{session}}"

# M1 scenarios on the lab Raspberry Pis through the fil-test gate (docs/m1/pi.md): this machine's
# client in a pseudo-terminal against `fil server --stdio` on each Pi. The Pi binary must be
# installed first (docs/m1/pi.md). Flags: --hosts pi1,pi2, --only NAME, --verbose, --list.
# Targets: FIL_PI_TARGETS="pi1=fil-test@HOST,pi2=fil-test@HOST" (required; the keys are the hosts it may touch). Never part of `ci`.
pi-e2e *args:
    cargo build --release -p fil --bins --example pi_e2e
    target/release/examples/pi_e2e {{args}}

# The drop-in suite (docs/m1/dropin.md): everyday ssh, et and mosh work (vim, less, top, history,
# floods, unicode, resizes, detach, takeover, a killed ssh or client, a sleeping client) through the
# real client on the lab Pis. Flags: --hosts pi1,pi2, --only NAME, --list, --verbose, --require TOOLS,
# --allow-missing, --allow-skip. `--local` runs against a private daemon on this machine instead (no
# targets needed; a development aid). Targets as for pi-e2e. A skipped scenario or a missing tool
# fails the run unless allowed. Never part of `ci`.
dropin *args:
    cargo build -q --release -p fil --bins --example dropin
    cargo build -q --release -p fil-daemon --bin fil-daemon-dev
    target/release/examples/dropin {{args}}
