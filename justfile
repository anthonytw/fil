# fil command index. `just` lists recipes.

default:
    @just --list

# The merge gate: run before every merge (CLAUDE.md § Delivery).
ci: fmt-check clippy check-default test deny

fmt-check:
    cargo fmt --all --check

clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Default-feature build of `fil`, as released. The other recipes use --all-features, which also
# compiles the vt100 backend and the test hooks; this proves `fil` builds without them.
check-default:
    cargo check -p fil

test:
    cargo test --workspace --all-features

# Fetches the RustSec advisory database, so it needs network access.
# License, source and ban policy, RustSec advisories and yanked crates (deny.toml).
deny:
    cargo deny check advisories licenses bans sources

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
# (docs/m1/binary.md); the relay starts the daemon. `just local` attaches to session "local" over
# the pipe only; `just local auto` also lets it upgrade to QUIC on loopback. Arguments:
# transport (`pipe` or `auto`) and session name.
local transport="pipe" session="local":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -q -p fil
    mkdir -p target/local
    config=target/local/config-{{transport}}.toml
    {
      printf '[defaults]\ntransport = "%s"\npipe = ["%s/target/debug/fil", "server", "--stdio"]\n' "{{transport}}" "{{justfile_directory()}}"
      if [ "{{transport}}" != pipe ]; then
        printf '[defaults.direct]\naddress = "127.0.0.1"\n'
      fi
    } > "$config"
    exec target/debug/fil --config "$config" localhost {{session}}

# M1 scenarios on the lab Raspberry Pis through the fil-test gate (docs/m1/pi.md): this machine's
# client in a pseudo-terminal against `fil server --stdio` on each Pi. The Pi binary must be
# installed first (docs/m1/pi.md). Flags: --hosts pi1,pi2, --only NAME, --verbose, --list.
# Targets: FIL_PI_TARGETS="pi1=fil-test@HOST,pi2=fil-test@HOST" (required; the keys are the hosts it may touch). Never part of `ci`.
pi-e2e *args:
    cargo build --release -p fil --bins --example pi_e2e
    target/release/examples/pi_e2e {{args}}
