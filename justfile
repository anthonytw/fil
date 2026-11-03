# fil command index. `just` lists recipes.

default:
    @just --list

# The merge gate: run before every merge (CLAUDE.md § Delivery).
ci: fmt-check clippy test deny

fmt-check:
    cargo fmt --all --check

clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
    cargo test --workspace --all-features

# License and source policy (deny.toml).
deny:
    cargo deny check licenses bans sources

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
# (docs/m1/binary.md); the relay starts the daemon. `just local` attaches to session
# "local" over the pipe only; `just local-direct` also lets it upgrade to QUIC on loopback.
local session="local":
    cargo build -q -p fil
    mkdir -p target/local
    printf '[defaults]\ntransport = "pipe"\npipe = ["%s/target/debug/fil", "server", "--stdio"]\n' "{{justfile_directory()}}" > target/local/config.toml
    target/debug/fil --config target/local/config.toml localhost {{session}}

local-direct session="local-direct":
    cargo build -q -p fil
    mkdir -p target/local
    printf '[defaults]\ntransport = "auto"\npipe = ["%s/target/debug/fil", "server", "--stdio"]\n[defaults.direct]\naddress = "127.0.0.1"\n' "{{justfile_directory()}}" > target/local/config-direct.toml
    target/debug/fil --config target/local/config-direct.toml localhost {{session}}

# M1 scenarios on the lab Raspberry Pis through the fil-test gate (docs/m1/pi.md): this machine's
# client in a pseudo-terminal against `fil server --stdio` on each Pi. The Pi binary must be
# installed first (docs/m1/pi.md). Flags: --hosts pi1,pi2, --only NAME, --verbose, --list.
# Targets: FIL_PI_TARGETS="pi1=fil-test@HOST,pi2=fil-test@HOST" (required; the keys are the hosts it may touch). Never part of `ci`.
pi-e2e *args:
    cargo build --release -p fil --bins --example pi_e2e
    target/release/examples/pi_e2e {{args}}
