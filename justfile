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

# Real-network harness (docs/m1/realnet.md): real OpenSSH in Docker and real NAT/netem in network
# namespaces. Linux only, needs Docker and root; refuses on macOS. Never part of `ci`.
# `just realnet` runs everything; `just realnet ssh` or `udp`; add scenario names to narrow.
realnet *args:
    tests/realnet/run.sh {{args}}
