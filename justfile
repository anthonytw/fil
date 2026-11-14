# fil command index. `just` lists recipes.

# Extra flags for `cargo deny` (the advisory database is fetched on each run). `FIL_DENY_FLAGS=--offline
# just ci` checks against the cached database instead: that is NOT a merge pass (CLAUDE.md § Delivery).
deny_flags := env("FIL_DENY_FLAGS", "")

# Where cargo puts its output. The recipes that run a built binary use this, so they work under
# `just vm-run` (docs/dev/vm.md), which gives each worktree its own CARGO_TARGET_DIR.
target_dir := if env("CARGO_TARGET_DIR", "") == "" { source_directory() / "target" } else { env("CARGO_TARGET_DIR") }

# The release path's recipes are in tests/release/release.just, which has a code owner; this file
# has none, so nothing on the release path reads it (just evaluates a justfile's assignments as soon
# as it parses it). release.yml, the release scripts and the owner run them as
# `tests/release/just.sh RECIPE` (docs/m4/release.md section 9). The recipes at the end of this file
# only forward to it, for `ci` and for convenience; the owner's release commands refuse to run
# through them and say what to type instead.

default:
    @just --list

# The per-PR checks (CLAUDE.md § Delivery; GitHub's linux-check runs it, macos-check runs `clippy`
# and `test-os` instead; docs/dev/timing-tests.md "Tiers"). `deny` runs first so that a network
# failure (the advisory database) shows up at once, not after the whole test suite; `notices` next,
# so that a missing cargo-about (the Notices section below) does too. Not in it: the wall-clock
# tests (on a shared CI runner they measure the host), the `soak_` tests and the full sizes
# (FIL_TEST_SCALE=full), and `release-test` (the release scripts' own tests, a parallel job of
# linux-check; run it when you touch tests/release, packaging or the workflows). `ci-release` has
# all of them.
ci: deny notices ci-workflow-test exporter-lock fmt-check clippy check-default release-deps test

# Before a release (docs/m4/release.md §9.2), on a quiet machine (an hour or more; not yet timed whole): `ci`, then
# everything the per-PR tiers leave out: the release scripts' tests, every test at full size with
# the `soak_` tests (`test-release`), the wall-clock tests alone, the full drop-in suite, the
# prediction matrix, terminal pacing, the PTY and prediction harnesses, and the shipped (fat-LTO)
# build's no-test-hooks check. GitHub's suite=all runs the rest (realnet, aarch64, terminals,
# Intel macOS, macOS as the remote host). Every part runs even when an earlier one fails (one red
# test must not hide the drop-in suite or pacing); the recipe fails at the end, listing the parts
# that failed.
ci-release:
    #!/usr/bin/env bash
    set -uo pipefail
    cd "{{source_directory()}}"
    j=(just --justfile "{{source_file()}}" --working-directory "{{source_directory()}}")
    failed=()
    part() { echo "==== ci-release: $*"; "${j[@]}" "$@" || failed+=("$*"); }
    part ci
    part release-test
    part test-release
    part test-wallclock
    part client-pty
    part predict-mutants
    part shipped-no-test-hooks
    part fidelity
    part predict-harness
    part predict-suites all
    part pacing
    part dropin --local --scale full
    if [ ${#failed[@]} -gt 0 ]; then
      echo "==== ci-release: FAILED: ${#failed[@]} part(s):" >&2
      printf '  just %s\n' "${failed[@]}" >&2
      exit 1
    fi
    echo "==== ci-release: every part passed"

# The release binary as shipped (the release profile: fat LTO, one codegen unit) has no test hooks.
# The per-PR check builds the `ci` profile instead (`no-test-hooks` checks feature unification,
# which the profile does not change); this one runs in `ci-release` and the check workflows'
# suite=all.
shipped-no-test-hooks:
    cargo build --release -p fil-shell --bins
    just no-test-hooks "{{target_dir}}/release/fil"
    "{{target_dir}}/release/fil" --version

# The cases of the Gitea workflow's push-to-main skip decision (tests/ci/push_skip.py, issue #38):
# offline fakes of git and the API, under a second. Part of `ci`; needs python3.
ci-workflow-test:
    python3 -I -B tests/ci/test_push_skip.py

fmt-check:
    cargo fmt --all --check

# The terminal exporter (tests/terminals/exporter) is outside the workspace and has its own
# Cargo.lock, which pins the workspace crates' version. Only GitHub's `linux-check` terminals job
# builds it (`--locked`), so a stale lock went unnoticed until that job ran (0.1.0's set-version
# left fil-term and fil-predict at 0.0.0 there). This fails fast if the lock needs an update.
exporter-lock:
    cargo update -w --locked -q --manifest-path tests/terminals/exporter/Cargo.toml

clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Default-feature build of `fil`, as released. The other recipes use --all-features, which also
# compiles the vt100 backend and the test hooks; this proves `fil` builds without them.
check-default:
    cargo check -p fil-shell

# Every test but the wall-clock ones (named wallclock_*, below) and the soaks (soak_*), at smoke
# size, in parallel, each test in a process of its own (cargo-nextest; `.config/nextest.toml`'s
# default profile), then the doctests, which nextest does not run. Logic and OS tests alike:
# linux-check runs this; macos-check runs only the OS tests (`test-os`). Every failure is reported
# (fail-fast is off); the recipe still fails on any. Needs cargo-nextest 0.9.149, the version the
# workflows pin (`_nextest` below says how to install it).
test: _nextest
    #!/usr/bin/env bash
    set -uo pipefail
    # Both halves run even when the first fails, so one run reports every failure.
    rc=0
    cargo nextest run --workspace --all-features || rc=1
    cargo test --workspace --all-features --doc || rc=1
    exit $rc

# cargo-nextest, which `test`, `test-os`, `test-release` and `forward-soak` need: a clear error
# naming the install, never a quiet fallback to `cargo test` (which would run the tests in one
# process per binary, without .config/nextest.toml's groups and filters).
_nextest:
    @cargo nextest --version > /dev/null 2>&1 || { echo "cargo-nextest is missing (the tests run under it, .config/nextest.toml): cargo install --locked cargo-nextest@0.9.149, or the prebuilt binary from https://nexte.st; in the Linux VM, just vm-setup" >&2; exit 1; }

# The OS tests only (PTYs, signals, sockets, processes, login shells; `.config/nextest.toml`'s `os`
# profile), at smoke size: what macos-check runs per PR, since pure logic behaves the same on Linux.
test-os: _nextest
    cargo nextest run --workspace --all-features -P os

# Every test at full size (FIL_TEST_SCALE=full; fil_testkit::scale), `soak_*` included, in the `ci`
# profile (optimized, as the soaks always ran): the release suite's half of `test`. fil-forward runs
# apart and one test at a time (`forward-soak`), since its full sizes use up the machine's ephemeral
# ports and listen queues. The wall-clock tests are `test-wallclock`'s. Both halves run even when
# the first fails; the recipe fails at the end, naming them.
test-release: _nextest
    #!/usr/bin/env bash
    set -uo pipefail
    cd "{{source_directory()}}"
    failed=()
    FIL_TEST_SCALE=full cargo nextest run --workspace --all-features --cargo-profile ci -P release -E 'not package(fil-forward) and not test(/(^|::)wallclock_/)' || failed+=("every test but fil-forward, at full size")
    just --justfile "{{source_file()}}" --working-directory "{{source_directory()}}" forward-soak || failed+=("forward-soak")
    if [ ${#failed[@]} -gt 0 ]; then
      printf 'test-release: FAILED: %s\n' "${failed[@]}" >&2
      exit 1
    fi

# The wall-clock tests, one at a time (docs/dev/timing-tests.md): tests named wallclock_*, whose
# pass or fail depends on real elapsed time (a latency percentile, "at once", Ctrl-C within 3 s).
# Run beside the rest of the suite they measure the contention between tests (a CI container
# capped at a few CPUs is throttled by the kernel in 100 ms periods); alone they measure the
# product. `test`, `test-release` (the soak_ tests and the full sizes) and this recipe together run
# every test: `ci-release` runs all three. Name a new wall-clock test with the prefix and it is in
# this step and out of the others.
test-wallclock:
    cargo test --workspace --all-features --no-fail-fast -- wallclock_ --test-threads=1

# Regenerate the wire golden (crates/fil-proto/tests/golden/wire.txt) after a deliberate wire
# change. Bump PROTOCOL_REVISION in crates/fil-proto/src/limits.rs FIRST: this refuses to write
# changed bytes under an unchanged revision (CLAUDE.md § The wire).
wire-golden:
    FIL_UPDATE_GOLDEN=1 cargo test -p fil-proto --test wire_golden the_wire_matches_the_golden_for_this_revision

# Fetches the RustSec advisory database, so it needs network access.
# License, source and ban policy, RustSec advisories and yanked crates (deny.toml).
deny:
    cargo deny {{deny_flags}} check advisories licenses bans sources

# Slow link-simulator soak: many seeds, harsh conditions (a `soak_` test: `test-release` runs it,
# not `ci`).
netsim-soak:
    cargo test -p fil-netsim --test quic --profile ci -- --nocapture soak_

# Terminal pacing (M3 K1, crates/fil-session/tests/terminal_pacing.rs) in release: Ctrl-C during a
# flood at 1 Mbit/s paced and unpaced (the unpaced QUIC cell takes about 20 s), and throughput
# paced against unpaced on a fast, a 100 Mbit/s 40 ms and the 1 Mbit/s link. Not part of `ci-release`,
# which runs the paced Ctrl-C cells (`test-wallclock`).
pacing:
    cargo test --release -p fil-session --test terminal_pacing -- --include-ignored --nocapture --test-threads=1

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

# Every fil-forward test at full size (FIL_TEST_SCALE=full; crates/fil-forward/tests/forward.rs):
# 256 MiB each way through a forward, 20,000 sequential and 1,000 concurrent connections, on
# link::mem, the pipe and QUIC, the SOCKS fuzz table and the new-epoch race with more seeds and
# runs. `just ci` runs the same tests at smoke size (2 MiB, 1,500, 300). The `ci` profile. One test
# at a time: at these sizes, tests side by side overflow loopback listen queues (128 on macOS) and
# the ephemeral ports. Part of `test-release`; the wall-clock tests are `test-wallclock`'s.
forward-soak mib="256" seq="20000": _nextest
    FIL_TEST_SCALE=full FIL_FORWARD_BULK_MIB={{mib}} FIL_FORWARD_SEQ={{seq}} cargo nextest run --all-features --cargo-profile ci -P release -p fil-forward -j 1 -E 'not test(/(^|::)wallclock_/)'

# Regenerate the synthetic emulator fixtures from crates/fil-term/tests/common/synthetic.rs.
fixtures:
    cargo run -q -p fil-term --example fixtures

# Record the real-program fixtures on a PTY (crates/fil-term/examples/record.rs).
# Inspect every output for personal data before committing.
record *scenarios:
    cargo run -q --release -p fil-term --example record -- {{scenarios}}

# Emulator restore-fidelity harness (docs/spikes/emulator.md). Flags: --only NAME, --no-perf, --verbose.
# The `ci` profile (optimized, quick to link), as every harness build here.
fidelity *args:
    cargo run --profile ci -p fil-term --example fidelity --all-features -- {{args}}

# Prediction overlay harness (docs/spikes/prediction.md): real programs on a PTY behind fil-netsim
# latency, checked against an independent emulator. Flags: --seeds N, --keys N, --latency MS,
# --only PROGRAM, --backend NAME, --hostile, --guard-off NAME, --jobs N, --verbose.
predict-harness *args:
    cargo run --profile ci -p fil-predict --features harness --example predict_harness -- {{args}}

# The full-stack prediction harness (docs/m3/harness.md): the real client engine against a real session
# on a real PTY, over simulated pipe and QUIC links, the user's terminal an emulator that reflows.
# Commands: resize (the true-order resize reproduction; W1's gate), baseline, secret, all. Flags:
# --transport pipe|quic|both, --seeds N, --one-way-ms N, --filter TEXT, --verbose. `--ignored` tests
# of crates/fil-client/tests/predict_e2e.rs are the resize reproduction as a test.
[positional-arguments]
predict-e2e *args:
    cargo run --release -p fil-client --example predict_e2e -- "$@"

# T1 part 2's prediction suites (docs/m3/harness.md section 9): every program of docs/m3/plan.md
# section 2.5 on both transports at 50, 150 and 400 ms, with jitter, loss and the faults, each
# written glyph classified from the flush tap's bytes and checked against the core's counts.
# `just predict-suites` runs the matrix; `secrets` the password prompts; `hardening` measures the
# proposed EchoAck-before-first-write guard; `all` all three. Env filters: FIL_SUITES_FILTER,
# FIL_SUITES_RTTS, FIL_SUITES_TRANSPORTS, FIL_SUITES_SEEDS, FIL_SUITES_LINES, FIL_SUITES_JOBS.
predict-suites which="matrix":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{which}}" in
      matrix) t=matrix_of_the_prediction_suites ;;
      secrets) t=secrets_of_the_prediction_suites ;;
      hardening) t=hardening_cost_of_ack_before_write ;;
      all) t="" ;;
      *) echo "predict-suites: matrix, secrets, hardening or all" >&2; exit 2 ;;
    esac
    cargo test --release -p fil-client --test predict_suites -- --ignored --nocapture --test-threads 1 $t

# Prediction demo in your own terminal: `just predict-demo 200ms` (latency each way).
# Flags: --backend NAME, --no-predict, --plain, --no-sync.
predict-demo *args:
    cargo run --release -p fil-predict --features harness --example predict_demo -- {{args}}

# Scripted prediction scenarios at scale (no PTY): many seeds, release build. Also the shadow's row
# stamps against a byte-by-byte reference (crates/fil-predict/tests/screen_view.rs).
predict-soak seeds="200" bursts="0":
    FIL_PREDICT_SEEDS={{seeds}} FIL_PREDICT_BURSTS={{bursts}} cargo test --release -p fil-predict --test sim
    FIL_STAMP_SEEDS={{seeds}} cargo test --release -p fil-predict --test screen_view every_row

# The soak's counts by program and delivery, not judged (docs/m3/plan.md §2.5's tables): 1,000 seeds
# and both backends by default; `bursts` is the percentage of keys held up as by a lost packet.
predict-soak-counts seeds="1000" bursts="0":
    FIL_PREDICT_SEEDS={{seeds}} FIL_PREDICT_BURSTS={{bursts}} cargo test --release -p fil-predict --test sim soak_counts -- --ignored --nocapture

# Guard-off sweep: proves every overlay guard is needed (slow; not in `ci`). Run after changing a guard.
predict-mutants:
    cargo test -p fil-predict --profile ci --test mutants -- --ignored

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
# `just realnet latency` is the latency lab (docs/m2/latency.md), not part of the default run;
# nor is `just realnet predict`, M3's prediction cells (docs/m3/plan.md §5.5).
# The sysd part runs a --privileged container (root on the host) and refuses unless this is a
# GitHub-hosted runner or FIL_REALNET_PRIVILEGED_OK=1 says the machine may be given one.
realnet *args:
    tests/realnet/run.sh {{args}}

# Local Linux VM (docs/dev/vm.md): root-only tests without GitHub. The Lima instance `fil` (override
# with FIL_VM) is Ubuntu aarch64 with Docker, netem and no access to this Mac; code reaches it by
# rsync. GitHub's linux-check stays the x86_64 reference.

# Once after creating the VM (and again when a tool is added): conntrack, the pinned toolchain,
# just, cargo-deny, cargo-about and cargo-nextest.
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
    cargo build -q -p fil-shell
    mkdir -p target/local
    config=target/local/config-{{transport}}.toml
    {
      printf '[defaults]\ntransport = "%s"\npipe = ["%s/debug/fil", "server", "--stdio"]\n' "{{transport}}" "{{target_dir}}"
      if [ "{{transport}}" != pipe ]; then
        printf '[defaults.direct]\naddress = "127.0.0.1"\n'
      fi
    } > "$config"
    exec "{{target_dir}}/debug/fil" --config "$config" localhost "{{session}}"

# M1 scenarios on the test Raspberry Pis through the fil-test gate (docs/m1/pi.md): this machine's
# client in a pseudo-terminal against `fil server --stdio` on each Pi. The Pi binary must be
# installed first (docs/m1/pi.md). Flags: --hosts pi1,pi2, --only NAME, --verbose, --list.
# Targets: FIL_PI_TARGETS="pi1=fil-test@HOST,pi2=fil-test@HOST" (required; the keys are the hosts it may touch). Never part of `ci`.
pi-e2e *args:
    cargo build --release -p fil-shell --bins
    cargo build --release -p fil-shell --example pi_e2e
    just no-test-hooks "{{target_dir}}/release/fil"
    "{{target_dir}}/release/examples/pi_e2e" {{args}}

# The drop-in suite (docs/m1/dropin.md): everyday ssh, et and mosh work (vim, less, top, history,
# floods, unicode, resizes, detach, takeover, a killed ssh or client, a sleeping client) through the
# real client on the test Pis. Flags: --hosts pi1,pi2, --only NAME, --list, --verbose, --require TOOLS,
# --allow-missing, --allow-skip, --strict, --dump DIR (where the transcript of a client whose output
# check fails goes). `--local` runs against a private daemon on this machine instead (no targets
# needed; a development aid). Targets as for pi-e2e. A skipped scenario or a
# missing tool fails the run unless allowed. The M2 forward scenarios (docs/m2/dropin.md) also use
# this machine's fwdprobe, built here, and the host's, installed through the gate. `--scale
# smoke|full` (default FIL_TEST_SCALE, else smoke): smoke, macos-check's per-PR run, leaves out
# the long scenarios; `ci-release` and suite=all run full. Built in the `ci` profile. Never part of `ci`.
dropin *args:
    cargo build -q --profile ci -p fil-shell --bins
    cargo build -q --profile ci -p fil-shell --example dropin
    cargo build -q --profile ci -p fil-daemon --bin fil-daemon-dev
    cargo build -q --profile ci -p fil-testbins --bin fwdprobe
    just no-test-hooks "{{target_dir}}/ci/fil" "{{target_dir}}/ci/fil-daemon-dev"
    "{{target_dir}}/ci/examples/dropin" --fil "{{target_dir}}/ci/fil" {{args}}

# macOS as the REMOTE host (DESIGN.md §16.9, docs/m4/plan.md MAC): the real release `fil` serves a
# session over a real ssh login to this Mac (an sshd of its own on a high loopback port with
# throwaway keys; MACOS_SSHD=system uses the system sshd and refuses to run unless this is a GitHub-hosted runner or
# MACOS_SSHD_SYSTEM_THROWAWAY=1 says the Mac is throwaway), driven by the
# client on a pty. Scenarios: `just macos-remote --list`; narrow with names. It starts and stops a
# `fil` daemon of its own and refuses to run beside one of yours (tests/macos/remote.sh). macOS
# only; never part of `ci`. `fil` is built in an invocation of its own, so it has no test hooks.
macos-remote *args:
    cargo build -q --release -p fil-shell --bins
    cargo build -q --release -p fil-testbins --bin ptydrive --bin fwdprobe
    just no-test-hooks "{{target_dir}}/release/fil"
    FIL_BIN_DIR="{{target_dir}}/release" tests/macos/remote.sh {{args}}

# ---- Packaging channels (docs/m4/packaging.md) ----

# Install-test the .deb and .rpm packages in DIR (a release's files, or `release-build`'s) in Debian
# and Fedora containers: install, `fil --version`, the bundle, the package manager's verify, remove.
# `--arch all` also runs the other architecture where Docker can (Rosetta).
[positional-arguments]
packages-install *args:
    tests/release/install-packages.sh "$@"

# Set the one package version (docs/m4/release.md section 1): [workspace.package] version and the
# exact `=X.Y.Z` requirements of [workspace.dependencies] in the root Cargo.toml, then Cargo.lock
# and the terminal exporter's own Cargo.lock (it is outside the workspace; `just exporter-lock`).
set-version version:
    #!/usr/bin/env bash
    set -euo pipefail
    v="{{version}}"
    printf '%s' "$v" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?' || { echo "set-version: '$v' is not X.Y.Z or X.Y.Z-PRE" >&2; exit 1; }
    perl -0pi -e 's/(\[workspace\.package\]\nversion = ")[^"]*"/${1}'"$v"'"/; s/(version = ")=[^"]*(")/${1}='"$v"'${2}/g' Cargo.toml
    cargo update -w --offline -q
    cargo update -w --offline -q --manifest-path tests/terminals/exporter/Cargo.toml
    grep -nE '^version = |version = "=' Cargo.toml

# The Homebrew formula for VERSION, from the release's SHA256SUMS: by default the copy that
# `tests/release/just.sh release-verify VERSION` verified (and the owner signed). Writes OUT/homebrew/fil.rb, for the
# tap anthonytw/homebrew-tap (docs/m4/packaging.md says how it gets there).
brew-formula version sums=("target/release-verify/" + version + "/files/SHA256SUMS") out="target/packaging":
    packaging/render.sh brew "{{version}}" "{{sums}}" "{{out}}/homebrew"

# The AUR package fil-shell-bin for VERSION: OUT/aur/PKGBUILD and .SRCINFO, from the same verified
# SHA256SUMS. Prepared, not published (docs/m4/packaging.md).
aur-pkgbuild version sums=("target/release-verify/" + version + "/files/SHA256SUMS") out="target/packaging":
    packaging/render.sh aur "{{version}}" "{{sums}}" "{{out}}/aur"

# ---- Documentation (docs/m4/plan.md, DOC; docs/man/build.sh) ----

# The generated documentation, laid out like an archive under OUT: fil(1), fil-config(5) and
# fil-policy(5), the completions for bash, zsh and fish, and the example configs. release.yml builds
# the same tree and gives it to package.sh --extra. Needs pandoc, at the version docs/man/build.sh pins.
docs out=(target_dir / "docs"):
    docs/man/build.sh "{{out}}"

# Rewrite the committed docs/man/fil.1 after a change to the command line (crates/fil/tests/docs_sync.rs
# fails until you do), then commit it.
docs-man:
    cargo run --quiet --locked -p fil-shell --example gen_docs -- --man-page > docs/man/fil.1.new
    mv docs/man/fil.1.new docs/man/fil.1

# ---- Forwarded to tests/release/release.just (the release path; see the top of this file) ----

[positional-arguments]
crates-check *args:
    tests/release/just.sh crates-check "$@"

[positional-arguments]
no-test-hooks *args:
    tests/release/just.sh no-test-hooks "$@"

[positional-arguments]
notices *args:
    tests/release/just.sh notices "$@"

[positional-arguments]
notices-update *args:
    tests/release/just.sh notices-update "$@"

[positional-arguments]
release *args:
    tests/release/just.sh release "$@"

[positional-arguments]
release-build *args:
    tests/release/just.sh release-build "$@"

[positional-arguments]
release-check *args:
    tests/release/just.sh release-check "$@"

[positional-arguments]
release-deps *args:
    tests/release/just.sh release-deps "$@"

[positional-arguments]
release-keys *args:
    tests/release/just.sh release-keys "$@"

[positional-arguments]
release-preflight *args:
    tests/release/just.sh release-preflight "$@"

[positional-arguments]
release-sign-macos *args:
    tests/release/just.sh release-sign-macos "$@"

[positional-arguments]
release-sign-packages *args:
    tests/release/just.sh release-sign-packages "$@"

[positional-arguments]
release-test *args:
    tests/release/just.sh release-test "$@"

[positional-arguments]
release-verify *args:
    tests/release/just.sh release-verify "$@"

[positional-arguments]
reserve-crate-names *args:
    tests/release/just.sh reserve-crate-names "$@"
