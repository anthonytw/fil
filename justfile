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
