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
